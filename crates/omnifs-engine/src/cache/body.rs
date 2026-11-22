//! Global content-addressed storage for durable provider bodies.

use parking_lot::{Mutex, RwLock, RwLockReadGuard};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

pub(crate) use crate::view::BodyId;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// One content-addressed body store shared by all projections. A body stays
/// while a fact of a projection refers to it or a [`BodyHold`] holds it, and
/// leaves with the collection after ([`crate::cache::collect`]).
#[derive(Debug)]
pub(crate) struct BodyStore {
    root: PathBuf,
    pins: Arc<Mutex<Pins>>,
    /// Held by each publication from its first body to its commit, so that
    /// a collection begins after the publications in flight.
    publications: RwLock<()>,
    /// The bytes of the bodies that the store took since the begin of its
    /// last collection.
    taken: AtomicU64,
    /// The bytes of the bodies that its last collection left.
    left: AtomicU64,
}

/// The bodies that a collection must leave whatever the projections refer
/// to: the ones that a [`BodyHold`] holds, and, for a collection in flight,
/// each one that a publication or a hold named since its begin, whose
/// reference the collection may not have read.
#[derive(Debug, Default)]
struct Pins {
    held: HashMap<BodyId, usize>,
    named: Option<HashSet<BodyId>>,
}

impl Pins {
    fn name(&mut self, id: BodyId) {
        if let Some(named) = &mut self.named {
            named.insert(id);
        }
    }
}

/// Keeps a body in the store while it lives: the body of an operation in
/// flight, which no fact refers to yet, or the body of a file that a reader
/// was told of.
#[derive(Debug)]
pub(crate) struct BodyHold {
    pins: Arc<Mutex<Pins>>,
    id: BodyId,
}

impl BodyHold {
    #[must_use]
    pub(crate) fn id(&self) -> BodyId {
        self.id
    }
}

impl Clone for BodyHold {
    fn clone(&self) -> Self {
        let mut pins = self.pins.lock();
        *pins.held.entry(self.id).or_default() += 1;
        Self {
            pins: Arc::clone(&self.pins),
            id: self.id,
        }
    }
}

impl Drop for BodyHold {
    fn drop(&mut self) {
        let mut pins = self.pins.lock();
        if let Some(count) = pins.held.get_mut(&self.id) {
            *count -= 1;
            if *count == 0 {
                pins.held.remove(&self.id);
            }
        }
    }
}

/// A collection of the store in flight, from [`BodyStore::begin_collection`]
/// to its drop.
pub(crate) struct Collection<'a> {
    store: &'a BodyStore,
}

impl Collection<'_> {
    /// Remove each body of the store that is not in `referred`, that no
    /// hold holds, and that nothing named since the begin of the
    /// collection. Returns the bodies that it removed.
    pub(crate) fn sweep(self, referred: &HashSet<BodyId>) -> Result<Vec<BodyId>, BodyStoreError> {
        let mut removed = Vec::new();
        let mut left = 0_u64;
        for entry in fs::read_dir(&self.store.root)? {
            let entry = entry?;
            let Some(id) = entry.file_name().to_str().and_then(body_of_name) else {
                continue;
            };
            let length = entry.metadata()?.len();
            if referred.contains(&id) {
                left = left.saturating_add(length);
                continue;
            }
            // The check and the removal are one step for a publication,
            // which names its body before it looks for it.
            let pins = self.store.pins.lock();
            if pins.held.contains_key(&id)
                || pins.named.as_ref().is_some_and(|named| named.contains(&id))
            {
                left = left.saturating_add(length);
                continue;
            }
            fs::remove_file(entry.path())?;
            drop(pins);
            removed.push(id);
        }
        if !removed.is_empty() {
            sync_directory(&self.store.root)?;
        }
        self.store.left.store(left, Ordering::Relaxed);
        Ok(removed)
    }
}

impl Drop for Collection<'_> {
    fn drop(&mut self) {
        self.store.pins.lock().named = None;
    }
}

impl BodyStore {
    pub(crate) fn open(root: impl AsRef<Path>) -> Result<Self, BodyStoreError> {
        let root = crate::cache::canonical_directory(root.as_ref()).map_err(|error| {
            if error.kind() == io::ErrorKind::InvalidInput {
                BodyStoreError::SymlinkRoot
            } else {
                BodyStoreError::Io(error)
            }
        })?;
        crate::cache::ensure_directory(&root).map_err(|error| {
            if error.kind() == io::ErrorKind::Other {
                BodyStoreError::SymlinkRoot
            } else {
                BodyStoreError::Io(error)
            }
        })?;
        for entry in fs::read_dir(&root)? {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with(".body-")
                && Path::new(name).extension().is_some_and(|ext| ext == "tmp")
            {
                fs::remove_file(path)?;
            }
        }
        Ok(Self {
            root,
            pins: Arc::default(),
            publications: RwLock::new(()),
            taken: AtomicU64::new(0),
            left: AtomicU64::new(0),
        })
    }

    /// Keep the body `id` in the store while the hold lives. The hold of a
    /// body that the store lacks keeps nothing: [`Self::validate`] after it
    /// tells whether the store has the body.
    pub(crate) fn hold(&self, id: BodyId) -> BodyHold {
        let mut pins = self.pins.lock();
        pins.name(id);
        *pins.held.entry(id).or_default() += 1;
        BodyHold {
            pins: Arc::clone(&self.pins),
            id,
        }
    }

    /// Whether a hold holds the body `id`.
    pub(crate) fn is_held(&self, id: BodyId) -> bool {
        self.pins.lock().held.contains_key(&id)
    }

    /// The publication of a projection in flight: held from before its
    /// first body to after its commit.
    pub(crate) fn publication(&self) -> RwLockReadGuard<'_, ()> {
        self.publications.read()
    }

    /// Whether a collection is due: the store took as many bytes since its
    /// last collection as that one left, so that the bodies without a
    /// reference never outweigh the ones with one, and the scan of a
    /// collection is paid by the bytes that came since the one before.
    pub(crate) fn collection_due(&self) -> bool {
        let taken = self.taken.load(Ordering::Relaxed);
        taken > 0 && taken >= self.left.load(Ordering::Relaxed)
    }

    /// Begin a collection: from here on, the store names each body of a
    /// publication and of a hold to the collection, and each publication
    /// that began before has ended. The facts that the caller reads after
    /// refer to each body that a sweep may not remove, beside those.
    pub(crate) fn begin_collection(&self) -> Collection<'_> {
        {
            let mut pins = self.pins.lock();
            let held = pins.held.keys().copied().collect();
            pins.named = Some(held);
        }
        self.taken.store(0, Ordering::Relaxed);
        drop(self.publications.write());
        Collection { store: self }
    }

    fn name(&self, id: BodyId) {
        self.pins.lock().name(id);
    }

    fn took(&self, length: u64) {
        self.taken.fetch_add(length, Ordering::Relaxed);
    }

    /// Publish bytes before any projection can reference their returned id.
    /// Existing content is immutable and therefore treated as an idempotent
    /// successful publication after the temporary file is removed.
    pub(crate) fn publish(&self, bytes: &[u8]) -> Result<BodyId, BodyStoreError> {
        let (id, linked) = self.link(bytes)?;
        if linked {
            sync_directory(&self.root)?;
        }
        Ok(id)
    }

    /// Publish each of `bodies` as [`publish`](Self::publish) does, with one
    /// thread for each thread of the computer, and with one sync of the
    /// directory after the links of all of them.
    pub(crate) fn publish_all(&self, bodies: &[&[u8]]) -> Result<(), BodyStoreError> {
        let threads = std::thread::available_parallelism()?;
        let next = AtomicUsize::new(0);
        let linked = AtomicBool::new(false);
        let failure = OnceLock::new();
        std::thread::scope(|scope| {
            for _ in 0..threads.get().min(bodies.len()) {
                scope.spawn(|| {
                    while let Some(bytes) = bodies.get(next.fetch_add(1, Ordering::Relaxed)) {
                        match self.link(bytes) {
                            Ok((_, true)) => linked.store(true, Ordering::Relaxed),
                            Ok((_, false)) => {},
                            Err(error) => {
                                let _ = failure.set(error);
                            },
                        }
                        if failure.get().is_some() {
                            break;
                        }
                    }
                });
            }
        });
        let synced = if linked.load(Ordering::Relaxed) {
            sync_directory(&self.root)
        } else {
            Ok(())
        };
        if let Some(error) = failure.into_inner() {
            return Err(error);
        }
        Ok(synced?)
    }

    /// Write and sync the file of `bytes` and link it at its id. Returns
    /// whether the directory took a link or a removal, which its sync makes
    /// durable.
    fn link(&self, bytes: &[u8]) -> Result<(BodyId, bool), BodyStoreError> {
        let id = BodyId::from_bytes(bytes);
        self.name(id);
        let destination = self.path(id);
        match fs::symlink_metadata(&destination) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(BodyStoreError::Destination { id });
            },
            Ok(_) => {
                Self::verify_file(&destination, id, bytes.len() as u64)?;
                return Ok((id, false));
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {},
            Err(error) => return Err(error.into()),
        }

        let temporary = self.temporary_path();
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            Self::verify_file(&temporary, id, bytes.len() as u64)?;
            match fs::hard_link(&temporary, &destination) {
                Ok(()) => {
                    fs::remove_file(&temporary)?;
                    self.took(bytes.len() as u64);
                    Ok((id, true))
                },
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let winner = Self::verify_destination(&destination, id, bytes.len() as u64);
                    let removed = fs::remove_file(&temporary);
                    winner?;
                    removed?;
                    Ok((id, true))
                },
                Err(error) => Err(error.into()),
            }
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    /// Create a streaming body staged in the global body directory.
    pub(crate) fn stage(&self) -> Result<BodyWriter, BodyStoreError> {
        let path = self.temporary_path();
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(BodyWriter {
            root: self.root.clone(),
            path,
            file: Some(file),
            hasher: blake3::Hasher::new(),
            length: 0,
            published: false,
        })
    }

    /// Publish a streamed body without materializing it in memory. The hold
    /// of the body keeps it until a fact refers to it.
    pub(crate) fn publish_staged(
        &self,
        mut staged: BodyWriter,
    ) -> Result<BodyHold, BodyStoreError> {
        let file = staged.file.as_mut().expect("staged body file exists");
        file.flush()?;
        file.sync_all()?;
        let id = BodyId::from_digest_bytes(*staged.hasher.finalize().as_bytes());
        let expected_len = staged.length;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() != expected_len {
            return Err(BodyStoreError::Length {
                id,
                expected: expected_len,
                actual: metadata.len(),
            });
        }
        let hold = self.hold(id);
        let destination = self.path(id);
        match fs::hard_link(&staged.path, &destination) {
            Ok(()) => self.took(expected_len),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                self.validate(id, Some(expected_len))?;
            },
            Err(error) => return Err(error.into()),
        }
        drop(staged.file.take());
        fs::remove_file(&staged.path)?;
        sync_directory(&self.root)?;
        staged.published = true;
        Ok(hold)
    }

    pub(crate) fn read(
        &self,
        id: BodyId,
        expected_len: Option<u64>,
    ) -> Result<Vec<u8>, BodyStoreError> {
        let path = self.path(id);
        let path_metadata = fs::symlink_metadata(&path)?;
        if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
            return Err(BodyStoreError::Destination { id });
        }
        let mut file = open_nofollow(&path)?;
        let metadata = file.metadata()?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(BodyStoreError::Destination { id });
        }
        if expected_len.is_some_and(|expected| expected != metadata.len()) {
            return Err(BodyStoreError::Length {
                id,
                expected: expected_len.unwrap_or(metadata.len()),
                actual: metadata.len(),
            });
        }
        let mut bytes = expected_len
            .and_then(|length| usize::try_from(length).ok())
            .map_or_else(Vec::new, Vec::with_capacity);
        file.read_to_end(&mut bytes)?;
        if BodyId::from_bytes(&bytes) != id {
            return Err(BodyStoreError::Digest { id });
        }
        Ok(bytes)
    }

    /// At most `len` bytes of the body `id` of `length` bytes from `offset`,
    /// without the rest of the body in memory. The digest of a body is of
    /// its whole, which its publication checked.
    pub(crate) fn read_range(
        &self,
        id: BodyId,
        length: u64,
        offset: u64,
        len: u32,
    ) -> Result<Vec<u8>, BodyStoreError> {
        let path = self.path(id);
        let mut file = open_nofollow(&path)?;
        let metadata = file.metadata()?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(BodyStoreError::Destination { id });
        }
        if metadata.len() != length {
            return Err(BodyStoreError::Length {
                id,
                expected: length,
                actual: metadata.len(),
            });
        }
        let end = offset.saturating_add(u64::from(len)).min(length);
        let Some(count) = end.checked_sub(offset).filter(|count| *count > 0) else {
            return Ok(Vec::new());
        };
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes =
            vec![0; usize::try_from(count).expect("a range of at most u32::MAX bytes fits usize")];
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    /// Whether the store has the body `id` with `expected_len` bytes, for
    /// a fact that is about to refer to it. The digest of a body is of its
    /// whole, which its publication checked: the store reads no body again
    /// for it.
    pub(crate) fn validate(
        &self,
        id: BodyId,
        expected_len: Option<u64>,
    ) -> Result<(), BodyStoreError> {
        self.name(id);
        let metadata = fs::symlink_metadata(self.path(id))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(BodyStoreError::Destination { id });
        }
        let length = metadata.len();
        if let Some(expected) = expected_len.filter(|expected| *expected != length) {
            return Err(BodyStoreError::Length {
                id,
                expected,
                actual: length,
            });
        }
        Ok(())
    }

    /// The directory of the store.
    pub(crate) fn path_of_store(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub(crate) fn path(&self, id: BodyId) -> PathBuf {
        self.root.join(id.hex())
    }

    fn temporary_path(&self) -> PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        self.root
            .join(format!(".body-{}-{sequence}.tmp", std::process::id()))
    }

    fn verify_file(path: &Path, id: BodyId, expected_len: u64) -> Result<(), BodyStoreError> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(BodyStoreError::Destination { id });
        }
        let mut file = open_nofollow(path)?;
        if metadata.len() != expected_len {
            return Err(BodyStoreError::Length {
                id,
                expected: expected_len,
                actual: metadata.len(),
            });
        }
        let mut hasher = blake3::Hasher::new();
        let mut length = 0_u64;
        let mut buffer = [0_u8; 8192];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            length += read as u64;
            hasher.update(&buffer[..read]);
        }
        if length != expected_len || hasher.finalize() != blake3::Hash::from_bytes(*id.as_bytes()) {
            return Err(BodyStoreError::Digest { id });
        }
        Ok(())
    }

    fn verify_destination(
        path: &Path,
        id: BodyId,
        expected_len: u64,
    ) -> Result<(), BodyStoreError> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                Err(BodyStoreError::Destination { id })
            },
            Ok(_) => Self::verify_file(path, id, expected_len),
            Err(error) => Err(error.into()),
        }
    }
}

/// A body staged by [`BodyStore::stage`]. It owns the temporary file until
/// publication, so a failed or abandoned stream cannot leave a cache body.
pub(crate) struct BodyWriter {
    root: PathBuf,
    path: PathBuf,
    file: Option<File>,
    hasher: blake3::Hasher,
    length: u64,
    published: bool,
}

impl Write for BodyWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self
            .file
            .as_mut()
            .expect("staged body file exists")
            .write(bytes)?;
        self.hasher.update(&bytes[..written]);
        self.length = self
            .length
            .checked_add(written as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "body length overflow"))?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().expect("staged body file exists").flush()
    }
}

impl Drop for BodyWriter {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_file(&self.path);
            let _ = sync_directory(&self.root);
        }
    }
}

/// The body whose file in the store has `name`, none for another file.
fn body_of_name(name: &str) -> Option<BodyId> {
    let mut digest = [0_u8; 32];
    hex::decode_to_slice(name, &mut digest).ok()?;
    Some(BodyId::from_digest_bytes(digest))
}

fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn open_nofollow(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        File::open(path)
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum BodyStoreError {
    #[error("body store I/O failed")]
    Io(#[source] io::Error),
    #[error("body store root must not be a symlink")]
    SymlinkRoot,
    #[error("body {id} has length {actual}, expected {expected}")]
    Length {
        id: BodyId,
        expected: u64,
        actual: u64,
    },
    #[error("body {id} failed digest validation")]
    Digest { id: BodyId },
    #[error("body {id} destination is not a regular file")]
    Destination { id: BodyId },
}

impl From<io::Error> for BodyStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl std::fmt::Display for BodyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.hex())
    }
}

#[cfg(test)]
mod tests {
    use super::BodyStore;
    use std::collections::HashSet;
    use std::io::Write;

    /// A collection removes the body that nothing refers to, and leaves the
    /// one that a fact refers to and the one that a hold holds, until the
    /// collection after the end of the hold.
    #[test]
    fn a_collection_leaves_the_body_of_a_reference_and_of_a_hold() {
        let dir = tempfile::tempdir().unwrap();
        let store = BodyStore::open(dir.path()).unwrap();
        let referred = store.publish(b"referred").unwrap();
        let pinned = store.publish(b"held").unwrap();
        let lost = store.publish(b"lost").unwrap();
        let hold = store.hold(pinned);
        let also = hold.clone();

        let removed = store
            .begin_collection()
            .sweep(&HashSet::from([referred]))
            .unwrap();
        assert_eq!(removed, vec![lost]);
        assert!(store.validate(referred, Some(8)).is_ok());
        assert!(store.validate(pinned, Some(4)).is_ok());
        assert!(store.validate(lost, None).is_err());

        // A body stays while one of its holds lives.
        drop(hold);
        assert!(store.is_held(pinned));
        assert!(
            store
                .begin_collection()
                .sweep(&HashSet::from([referred]))
                .unwrap()
                .is_empty()
        );
        drop(also);
        assert!(!store.is_held(pinned));
        let removed = store
            .begin_collection()
            .sweep(&HashSet::from([referred]))
            .unwrap();
        assert_eq!(removed, vec![pinned]);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// The body that a publication names after the begin of a collection
    /// stays, whatever the collection read of the facts: its fact commits
    /// after that read. A publication in flight at the begin ends first.
    #[test]
    fn a_collection_leaves_the_body_that_a_publication_names_since_its_begin() {
        let dir = tempfile::tempdir().unwrap();
        let store = BodyStore::open(dir.path()).unwrap();
        let before = store.publish(b"before").unwrap();
        let known = store.publish(b"known").unwrap();

        let collection = store.begin_collection();
        // During the collection: a new body, a body that the store had, by
        // its bytes and by its validation for a fact, and a staged body.
        let new = store.publish(b"new").unwrap();
        assert_eq!(store.publish(b"known").unwrap(), known);
        let mut staged = store.stage().unwrap();
        staged.write_all(b"staged").unwrap();
        let staged = store.publish_staged(staged).unwrap().id();
        let removed = collection.sweep(&HashSet::new()).unwrap();
        assert_eq!(removed, vec![before]);
        for (body, length) in [(new, 3), (known, 5), (staged, 6)] {
            assert!(store.validate(body, Some(length)).is_ok());
        }

        // The begin of a collection waits for the publication in flight.
        let store = std::sync::Arc::new(store);
        let publication = store.publication();
        let (began, waits) = std::sync::mpsc::channel();
        let collector = std::thread::spawn({
            let store = std::sync::Arc::clone(&store);
            move || {
                let collection = store.begin_collection();
                began.send(()).unwrap();
                collection.sweep(&HashSet::new()).unwrap().len()
            }
        });
        assert!(
            waits
                .recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "a collection began beside a publication in flight"
        );
        drop(publication);
        assert_eq!(collector.join().unwrap(), 3);
    }

    /// A collection is due once the store took as many bytes as the one
    /// before left.
    #[test]
    fn a_collection_is_due_by_the_bytes_that_the_store_took_since_the_last() {
        let dir = tempfile::tempdir().unwrap();
        let store = BodyStore::open(dir.path()).unwrap();
        assert!(!store.collection_due());
        let kept = store.publish(b"12345678").unwrap();
        assert!(store.collection_due());
        store
            .begin_collection()
            .sweep(&HashSet::from([kept]))
            .unwrap();
        assert!(!store.collection_due());
        store.publish(b"1234").unwrap();
        // The bytes of a body that the store has are no bytes that it took.
        store.publish(b"12345678").unwrap();
        assert!(!store.collection_due());
        store.publish(b"abcd").unwrap();
        assert!(store.collection_due());
    }

    #[test]
    fn publish_all_publishes_each_body_once_beside_a_body_of_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = BodyStore::open(dir.path()).unwrap();
        let known = store.publish(b"known").unwrap();
        let bodies: Vec<Vec<u8>> = (0..64_u8).map(|byte| vec![byte; 3]).collect();
        let mut all: Vec<&[u8]> = bodies.iter().map(Vec::as_slice).collect();
        all.push(b"known");
        all.push(&bodies[0]);
        store.publish_all(&all).unwrap();
        for bytes in &bodies {
            let id = super::BodyId::from_bytes(bytes);
            assert_eq!(store.read(id, Some(3)).unwrap(), *bytes);
        }
        assert_eq!(store.read(known, Some(5)).unwrap(), b"known");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 65);
        store.publish_all(&[]).unwrap();
    }

    #[test]
    fn publish_reopens_and_verifies_content_addressed_body() {
        let temp = tempfile::tempdir().unwrap();
        let store = BodyStore::open(temp.path()).unwrap();
        let id = store.publish(b"body").unwrap();
        assert_eq!(store.read(id, None).unwrap(), b"body");
        assert_eq!(store.publish(b"body").unwrap(), id);
        std::fs::write(store.path(id), b"corrupt").unwrap();
        assert!(matches!(
            store.publish(b"body"),
            Err(super::BodyStoreError::Length { .. } | super::BodyStoreError::Digest { .. })
        ));
        std::fs::remove_file(store.path(id)).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("elsewhere", store.path(id)).unwrap();
            assert!(matches!(
                store.publish(b"body"),
                Err(super::BodyStoreError::Destination { .. })
            ));
            assert!(matches!(
                store.read(id, None),
                Err(super::BodyStoreError::Destination { .. })
            ));
        }
    }
}

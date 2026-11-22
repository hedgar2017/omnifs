use crate::Runtime;
use omnifs_core::path::Path;
use parking_lot::Mutex;

#[derive(Default)]
pub(crate) struct InvalidationState {
    invalidated_prefixes: Mutex<Vec<Path>>,
    invalidated_paths: Mutex<Vec<Path>>,
    changed_dirs: Mutex<Vec<(Path, bool)>>,
    changed_files: Mutex<Vec<Path>>,
    /// Notified at each record, for the drain of the namespace.
    recorded: std::sync::Arc<tokio::sync::Notify>,
}

impl InvalidationState {
    pub(crate) fn record_prefix(&self, prefix: Path) {
        self.invalidated_prefixes.lock().push(prefix);
        self.recorded.notify_one();
    }

    pub(crate) fn record_path(&self, path: Path) {
        self.invalidated_paths.lock().push(path);
        self.recorded.notify_one();
    }

    pub(crate) fn record_changed_dir(&self, path: Path, fed: bool) {
        self.changed_dirs.lock().push((path, fed));
        self.recorded.notify_one();
    }

    pub(crate) fn record_changed_file(&self, path: Path) {
        self.changed_files.lock().push(path);
        self.recorded.notify_one();
    }

    fn drain_changed_files(&self) -> Vec<Path> {
        let mut paths = self.changed_files.lock();
        std::mem::take(&mut *paths)
    }

    fn drain_prefixes(&self) -> Vec<Path> {
        let mut prefixes = self.invalidated_prefixes.lock();
        std::mem::take(&mut *prefixes)
    }

    fn drain_paths(&self) -> Vec<Path> {
        let mut paths = self.invalidated_paths.lock();
        std::mem::take(&mut *paths)
    }

    fn drain_changed_dirs(&self) -> Vec<(Path, bool)> {
        let mut paths = self.changed_dirs.lock();
        std::mem::take(&mut *paths)
    }
}

impl Runtime {
    /// The notification of a record of an invalidation or a change of the
    /// mount, which a drain takes.
    pub(crate) fn recorded(&self) -> std::sync::Arc<tokio::sync::Notify> {
        std::sync::Arc::clone(&self.invalidation.recorded)
    }

    pub fn cache_delete_prefix(&self, prefix: &Path) {
        self.resources.memory.invalidate_prefix(prefix);
        self.invalidation.record_prefix(prefix.clone());
    }

    pub fn cache_delete_path(&self, path: &Path) {
        self.resources.memory.delete_exact(path);
        self.invalidation.record_path(path.clone());
    }

    /// Record that directory `path`'s contents changed without touching the
    /// cached view record, with whether its listing is `fed`. Used by
    /// pagination's `@next`/`@all`, a changed listing page, and the listings a
    /// feed answer writes into; filesystems see the directory's attributes
    /// changed in place, so it keeps its identity and the nodes below it keep
    /// theirs.
    pub fn record_dir_changed(&self, path: &Path, fed: bool) {
        self.invalidation.record_changed_dir(path.clone(), fed);
    }

    /// Record that a feed answer changed the fed file `path`. Filesystems see
    /// its attributes changed in place, so it keeps its identity, as the
    /// filehandle of a read that waits for the answer.
    pub(crate) fn record_file_changed(&self, path: &Path) {
        self.invalidation.record_changed_file(path.clone());
    }

    pub(crate) fn drain_changed_files(&self) -> Vec<Path> {
        self.invalidation.drain_changed_files()
    }

    pub fn drain_invalidated_prefixes(&self) -> Vec<Path> {
        self.invalidation.drain_prefixes()
    }

    pub fn drain_invalidated_paths(&self) -> Vec<Path> {
        self.invalidation.drain_paths()
    }

    pub fn drain_changed_dirs(&self) -> Vec<(Path, bool)> {
        self.invalidation.drain_changed_dirs()
    }

    pub(crate) fn record_view_invalidations(&self, prefixes: Vec<Path>, paths: Vec<Path>) {
        for prefix in prefixes {
            self.invalidation.record_prefix(prefix);
        }
        for path in paths {
            self.invalidation.record_path(path);
        }
    }
}

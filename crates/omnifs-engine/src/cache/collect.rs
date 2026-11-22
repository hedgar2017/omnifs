//! Collection of the body store and of the kept answers of blob requests.
//!
//! The body store is content-addressed and shared by every projection of the
//! database, so a body leaves only when no fact of any projection refers to
//! it and no [`BodyHold`](super::body::BodyHold) holds it. A collection reads
//! the references of every projection ([`MountResources::body_references`])
//! and sweeps the rest. It reads the facts once, at the start of the store
//! and whenever the store took as many bytes as the collection before left
//! ([`BodyStore::collection_due`]), never for an operation.
//!
//! A publication cannot race it: [`BodyStore::begin_collection`] waits for
//! the publications in flight, and each body that a publication or a hold
//! names from then on stays whatever the collection read.
//!
//! The kept answer of a blob request is a reference of its own. The host
//! cannot know whether a provider asks a request again, so the files bound
//! it: the answer leaves at the second collection in a row that finds no
//! fact of a file with its body, and its body with it.

use super::body::{BodyId, BodyStore};
use super::mount::{BLOB_KEYS, BlobFact, MountResources, ProjectionError};
use fjall::{KeyspaceCreateOptions, OptimisticTxDatabase, PersistMode, Readable};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// What a collection removed.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Collected {
    pub bodies: usize,
    pub blobs: usize,
}

pub(crate) struct Collector {
    body: Arc<BodyStore>,
    database: OptimisticTxDatabase,
    /// One collection runs at a time.
    running: Mutex<()>,
    scheduled: AtomicBool,
}

impl Collector {
    pub(crate) fn new(body: Arc<BodyStore>, database: OptimisticTxDatabase) -> Arc<Self> {
        Arc::new(Self {
            body,
            database,
            running: Mutex::new(()),
            scheduled: AtomicBool::new(false),
        })
    }

    /// Run a collection on a thread of its own once one is due, so that no
    /// operation waits for it.
    pub(crate) fn collect_when_due(self: &Arc<Self>) {
        if !self.body.collection_due() || self.scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let collector = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("omnifs-collect".into())
            .spawn(move || {
                let result = collector.collect();
                collector.scheduled.store(false, Ordering::Release);
                if let Err(error) = result {
                    tracing::warn!(%error, "the collection of the body store failed");
                }
            });
        if let Err(error) = spawned {
            self.scheduled.store(false, Ordering::Release);
            tracing::warn!(%error, "the collection of the body store did not start");
        }
    }

    /// Remove each kept answer of a blob request that two collections in a
    /// row found without a fact of a file with its body, and each body that
    /// no fact of a projection refers to and no hold holds.
    pub(crate) fn collect(&self) -> Result<Collected, ProjectionError> {
        let _running = self.running.lock();
        let collection = self.body.begin_collection();
        let mut referred = HashSet::new();
        let mut collected = Collected::default();
        for name in self.database.list_keyspace_names() {
            if !name.starts_with("facts.") {
                continue;
            }
            let facts = self
                .database
                .keyspace(&name, KeyspaceCreateOptions::default)?;
            let snapshot = self.database.read_tx();
            let mut bodies = HashSet::new();
            let mut blobs = Vec::new();
            for entry in snapshot.iter(&facts) {
                let (key, value) = entry.into_inner()?;
                if key.starts_with(BLOB_KEYS) {
                    blobs.push((key, value));
                } else {
                    MountResources::body_references(&key, &value, &mut bodies)?;
                }
            }
            // The answers of the blob requests of the projection: one that
            // a file has stays, one without a file lapses, and one that
            // lapsed before leaves. The body of each that stays is referred
            // to.
            let mut tx = self
                .database
                .write_tx()?
                .durability(Some(PersistMode::SyncAll));
            let mut kept = Vec::new();
            let mut removed = 0;
            let mut changed = false;
            for (key, value) in &blobs {
                let fact = BlobFact::decode(value);
                let body = fact
                    .as_ref()
                    .map(|fact| BodyId::from_digest_bytes(fact.body_id));
                kept.extend(body);
                // Another fact than the one that the collection read is of
                // a fetch since, which stands.
                if tx.get(&facts, key)?.as_deref() != Some(&**value) {
                    continue;
                }
                // A fact that this host cannot read answers no request.
                let (Some(mut fact), Some(body)) = (fact, body) else {
                    tx.remove(&facts, &**key);
                    removed += 1;
                    continue;
                };
                let has_file = bodies.contains(&body) || self.body.is_held(body);
                if !has_file && fact.lapsed {
                    tx.remove(&facts, &**key);
                    kept.pop();
                    removed += 1;
                } else if fact.lapsed == has_file {
                    fact.lapsed = !has_file;
                    tx.insert(&facts, &**key, fact.encode()?);
                    changed = true;
                }
            }
            if removed > 0 || changed {
                if tx.commit()?.is_ok() {
                    collected.blobs += removed;
                } else {
                    // A fetch wrote an answer since: the answers of the
                    // projection stand as they are until the next
                    // collection.
                    kept = blobs
                        .iter()
                        .filter_map(|(_, value)| BlobFact::decode(value))
                        .map(|fact| BodyId::from_digest_bytes(fact.body_id))
                        .collect();
                }
            }
            referred.extend(bodies);
            referred.extend(kept);
        }
        collected.bodies = collection.sweep(&referred)?.len();
        Ok(collected)
    }
}

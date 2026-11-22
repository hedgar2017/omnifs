//! The syncs of a mount, which say when a fact of the projection is due, and
//! the wall clock, which the host reads only to compare a time that an
//! upstream states.
//!
//! A fact is due once in each sync of its mount, and never outside a sync:
//! its expiry is the number of the first sync in which it is due
//! ([`crate::cache::MountResources::due_in`]). An upstream that states a
//! time, as the seconds of the interval of a page or of a `Retry-After`, is
//! honoured by a comparison of the wall clock with that time.

use crate::view::Stability;

/// The expiry of a fact of `stability` written when `due_in` is the first
/// sync in which a fact written now is due: a stable fact is never due, a
/// dynamic one in the next sync, and a live one at each operation.
pub(crate) fn freshness_expiry(stability: Stability, due_in: u64) -> Option<u64> {
    match stability {
        Stability::Stable => None,
        Stability::Dynamic => Some(due_in),
        Stability::Live => Some(0),
    }
}

/// Wall-clock milliseconds since the Unix epoch, for a comparison with a time
/// that an upstream states.
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

//! Serving fed paths: the answering feed among the claims of an operation,
//! and when it answers before the operation is served from the projection.

use crate::Runtime;
use crate::cache::FeedState;
use crate::ops::feed::{FeedClaim, FeedStart};
use crate::ops::namespace::{ListOutcome, PageRequest};
use crate::view::LookupPayload;
use omnifs_core::path::Path;
use omnifs_wit::host::types as wit_types;

use super::error::{Result, TreeError};

/// The fact an operation on a fed path serves.
#[derive(Debug, Clone, Copy)]
pub(crate) enum FedFact {
    Lookup,
    Listing,
    File,
}

/// Have the answering feed among `claims` answer when the operation on
/// `path` needs it: the outer claim whose first answer ended, else the
/// innermost one. A lookup of a path that the projection lacks, under a fed
/// listing below the anchor of the answering claim that is not complete in
/// the projection, takes the answer of that listing first, with its claims,
/// and is then served from the projection: an operation reaches each path by
/// a lookup of each segment from the root, a filehandle that it held too, so
/// that a child that only the listing brings is found. A lookup under a
/// complete listing, or under one at or above that anchor, keeps the rules
/// below. After the end of the first answer of a feed of full listings, only a
/// listing of its anchor and a lookup of an entry of that listing, on a
/// starting path of the feed, without its positive lookup take an answer,
/// once the feed is past its deadline, outside the rate-limit window of the
/// mount, with `due` and their path for an operation on a starting path of
/// the feed, and each other operation is served from the projection.
/// Else a read or a listing that starts that feed without its fact in the
/// projection, and a lookup that starts it without its positive lookup before
/// the end of the first answer of the feed, or below the anchor of an own
/// feed, take an answer with `lacks` and their path under the anchor of the
/// innermost claim as `start`; after that end, a lookup is served from the
/// projection, as not found for a path that it lacks. Any other operation
/// takes an answer once the feed is past its deadline, outside the rate-limit
/// window of the mount, with `due` and its path for an operation on a
/// starting path of the feed. A feed with kept parts of a later answer
/// continues that answer by the same rules, with the start of the operation,
/// and for an operation that those rules give no answer, without a start,
/// whatever its deadline, outside the rate-limit window of the mount. An
/// operation that waited on another answer of the same feed, or whose feed
/// another answer moved before its flight, decides again after it.
///
/// A listing records its answering feed as the feed of the listing of its
/// directory ([`MountResources::listing_due`]), unless it is a feed of full
/// listings after the end of its first answer that the listing does not ask.
///
/// Once the answering claim needs no answer, or after its answer that the
/// operation ran, the operation asks the innermost own claim inner to the
/// answering one that starts it without its fact (see [`answer_own`]).
///
/// Returns the path of the fact of the operation in the projection, and the
/// path of the operation under the anchor of the innermost claim. The facts
/// of a fed object live under the anchor of its claim, with its facets at
/// their first choice, for every facet: an operation below an anchor of
/// another facet, or on its listing, has its fact at the same path under the
/// anchor of the claim. The lookup of an anchor has its fact at its own path,
/// in the listing of its parent.
pub(crate) async fn answer_claims(
    runtime: &Runtime,
    path: &Path,
    fact: FedFact,
    claims: &[FeedClaim],
) -> Result<(Path, Path)> {
    // The validation of a fed answer rejects one without claims.
    let anchor = &claims[claims.len() - 1].object.path;
    let under_anchor = &under(anchor, path)?;
    let at = fact_path(path, fact, anchor, under_anchor);
    let internal = |error: crate::cache::ProjectionError| TreeError::internal(error.to_string());
    let answering = loop {
        let states = claims
            .iter()
            .map(|claim| runtime.resources.feed_state(claim.object.id.as_bytes()))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(internal)?;
        let answering = answering_claim(claims, &states);
        let (claim, state) = (&claims[answering], &states[answering]);
        if matches!(fact, FedFact::Lookup)
            && let Some(listing) = incomplete_listing(runtime, at, &claim.object.path)?
            && let Some(claims) = listing_claims(runtime, &listing).await?
        {
            Box::pin(answer_claims(runtime, &listing, FedFact::Listing, &claims)).await?;
            return Ok((at.clone(), under_anchor.clone()));
        }
        let kept = state.as_ref().is_some_and(|state| state.kept > 0);
        let first_answer = state.as_ref().is_none_or(|state| !state.ended);
        let below_own = claim.own && path.segments().count() > claim.object.path.segments().count();
        let due = is_due(runtime, state.as_ref());
        let start = if claim.full_listings && !first_answer {
            (due && asks_listing(runtime, path, fact, at, claim)?)
                .then(|| claim.starts.then(|| FeedStart::Due(under_anchor.clone())))
        } else if claim.starts
            && (first_answer || below_own || !matches!(fact, FedFact::Lookup))
            && !has_fact(runtime, at, fact)?
        {
            Some(Some(FeedStart::Lacks(under_anchor.clone())))
        } else if due {
            Some(claim.starts.then(|| FeedStart::Due(under_anchor.clone())))
        } else {
            None
        };
        let Some(start) = start.or((kept && due).then_some(None)) else {
            break answering;
        };
        let lacks = matches!(start, Some(FeedStart::Lacks(_)));
        // The projection serves what it has of a feed whose first answer
        // ended, or of a path that is no listing: the operation that lacks
        // its fact, and the listing of a first answer that has not ended,
        // have nothing to be served.
        let served =
            !lacks && state.is_some() && !(first_answer && matches!(fact, FedFact::Listing));
        // Within the window that the upstream states, and after a failure
        // of the feed in this sync, the operation is served without a
        // request, or has the error: the feed is asked again after the
        // instant of the window, else in the next sync.
        if let Some(error) = held_back(runtime, claim)? {
            if served {
                break answering;
            }
            return Err(crate::EngineError::ProviderError(error).into());
        }
        match runtime
            .feed(
                &claim.object,
                start.as_ref(),
                state.as_ref(),
                lacks.then_some(at),
            )
            .await
        {
            Ok(true) => break answering,
            Ok(false) => {},
            Err(crate::EngineError::ProviderError(error)) if failed_for_now(&error) => {
                // No retry of an operation is a request of its own: the feed
                // is asked again in the next sync.
                fail_for_now(runtime, claim, &error)?;
                if !served {
                    return Err(crate::EngineError::ProviderError(error).into());
                }
                break answering;
            },
            Err(error) => return Err(error.into()),
        }
    };
    answer_own(runtime, path, fact, &claims[answering + 1..]).await?;
    Ok((at.clone(), under_anchor.clone()))
}

/// Have the innermost own claim of `inner`, the claims inner to the
/// answering one, that starts the operation on `path` without its fact
/// answer with `lacks` and the path of the operation under its anchor as
/// `start`, the lookup of its anchor only when the feed has no state or is
/// past its deadline outside the rate-limit window of the mount. An
/// operation that waited on another answer of the same feed, or whose feed
/// another answer moved before its flight, decides again after it.
async fn answer_own(
    runtime: &Runtime,
    path: &Path,
    fact: FedFact,
    inner: &[FeedClaim],
) -> Result<()> {
    loop {
        let mut asked = None;
        for claim in inner.iter().rev().filter(|claim| claim.own && claim.starts) {
            let under_anchor = under(&claim.object.path, path)?;
            let at = fact_path(path, fact, &claim.object.path, &under_anchor);
            if !has_fact(runtime, at, fact)? {
                asked = Some((claim, under_anchor));
                break;
            }
        }
        let Some((claim, under_anchor)) = asked else {
            return Ok(());
        };
        let state = runtime
            .resources
            .feed_state(claim.object.id.as_bytes())
            .map_err(|error| TreeError::internal(error.to_string()))?;
        if matches!(fact, FedFact::Lookup)
            && under_anchor == claim.object.path
            && state.is_some()
            && !is_due(runtime, state.as_ref())
        {
            return Ok(());
        }
        // The fact lacks: within the window of the mount, and after a failure
        // of the feed in this sync, the operation has the error without a
        // request.
        if let Some(error) = held_back(runtime, claim)? {
            return Err(crate::EngineError::ProviderError(error).into());
        }
        let at = fact_path(path, fact, &claim.object.path, &under_anchor).clone();
        let start = FeedStart::Lacks(under_anchor);
        match runtime
            .feed(&claim.object, Some(&start), state.as_ref(), Some(&at))
            .await
        {
            Ok(true) => return Ok(()),
            Ok(false) => {},
            Err(crate::EngineError::ProviderError(error)) if failed_for_now(&error) => {
                fail_for_now(runtime, claim, &error)?;
                return Err(crate::EngineError::ProviderError(error).into());
            },
            Err(error) => return Err(error.into()),
        }
    }
}

/// The answering claim among `claims` with their `states`: the innermost own
/// claim whose feed is unlisted, an object that no inventory of a feed above
/// it has; else the outer claim whose first answer ended; else the innermost
/// one.
fn answering_claim(claims: &[FeedClaim], states: &[Option<FeedState>]) -> usize {
    let unlisted = |index: &usize| {
        claims[*index].own && states[*index].as_ref().is_some_and(|state| state.unlisted)
    };
    (0..claims.len()).rev().find(unlisted).unwrap_or_else(|| {
        states
            .iter()
            .position(|state| state.as_ref().is_some_and(|state| state.ended))
            .unwrap_or(claims.len() - 1)
    })
}

/// Whether `error` is of a check that failed for now: at a rate limit, the
/// network, a timeout, or the upstream, which the SDK gives as the network.
fn failed_for_now(error: &wit_types::ProviderError) -> bool {
    matches!(
        error.kind,
        wit_types::ErrorKind::RateLimited
            | wit_types::ErrorKind::Network
            | wit_types::ErrorKind::Timeout
    )
}

/// Whether a feed of `state` is due: a feed whose first answer has not
/// ended, whose facts the projection lacks, whenever an operation asks; and
/// in a sync, once, a feed that was not asked in it. Outside a sync, a feed
/// whose first answer ended is served from the projection.
fn is_due(runtime: &Runtime, state: Option<&FeedState>) -> bool {
    let sync = runtime.resources.sync_now();
    state.is_none_or(|state| !state.ended || (sync != 0 && state.due_in <= sync))
}

/// The error that holds an answer of the feed of `claim` back: the one of
/// the window that the upstream of the mount stated, while it is open, or
/// the one of a check of the feed that failed in this sync.
fn held_back(runtime: &Runtime, claim: &FeedClaim) -> Result<Option<wit_types::ProviderError>> {
    if let Some((remaining, error)) = runtime.window() {
        return Ok(Some(wit_types::ProviderError {
            retry_after: Some(u32::try_from(remaining.as_secs().max(1)).unwrap_or(u32::MAX)),
            ..error
        }));
    }
    Ok(runtime.failure(claim.object.id.as_bytes())?)
}

/// Record the check of the feed of `claim` that failed for now with `error`:
/// it is due in the next sync, and not asked again in this one.
fn fail_for_now(
    runtime: &Runtime,
    claim: &FeedClaim,
    error: &wit_types::ProviderError,
) -> Result<()> {
    Ok(runtime.stand_failure(claim.object.id.as_bytes(), error)?)
}

/// Whether the operation on `path`, with its fact at `at`, asks the feed of
/// full listings of `claim` after the end of its first answer: a listing of
/// its anchor, or a lookup of an entry of that listing, on a starting path of
/// the feed, without its positive lookup.
fn asks_listing(
    runtime: &Runtime,
    path: &Path,
    fact: FedFact,
    at: &Path,
    claim: &FeedClaim,
) -> Result<bool> {
    let anchor = &claim.object.path;
    let under_anchor = under(anchor, path)?;
    Ok(match fact {
        FedFact::Listing => under_anchor == *anchor,
        FedFact::Lookup => {
            claim.starts
                && under_anchor.parent().as_ref() == Some(anchor)
                && !has_fact(runtime, at, FedFact::Lookup)?
        },
        FedFact::File => false,
    })
}

/// The path of the fact of an operation on `path` in the projection, for
/// the claim with `anchor`, `path` under the anchor of the claim being
/// `under_anchor`: the lookup of the anchor has its fact at its own path.
fn fact_path<'a>(path: &'a Path, fact: FedFact, anchor: &Path, under_anchor: &'a Path) -> &'a Path {
    if matches!(fact, FedFact::Lookup) && under_anchor == anchor {
        path
    } else {
        under_anchor
    }
}

/// The listing of the parent of `at` for a lookup of `at` that the projection
/// lacks, when that listing is below `anchor`, fed, and not complete in the
/// projection.
fn incomplete_listing(runtime: &Runtime, at: &Path, anchor: &Path) -> Result<Option<Path>> {
    let Some((listing, _)) = at.parent_and_name() else {
        return Ok(None);
    };
    if listing == *anchor
        || !listing.has_prefix(anchor)
        || has_fact(runtime, at, FedFact::Lookup)?
        || complete_listing(runtime, &listing)?
        || !runtime
            .resources
            .is_fed(&listing)
            .map_err(|error| TreeError::internal(error.to_string()))?
    {
        return Ok(None);
    }
    Ok(Some(listing))
}

/// The claims of the feeds that answer for `listing`, none for a listing that
/// no feed answers.
/// In a sync, have the feed of the fed directory `path` answer once, as its
/// listing would: the attributes of a directory that a sync reads carry what
/// the sync changed, so that a client that keeps a listing by the change of
/// its directory lists it again after a change alone. Outside a sync, and
/// for a feed already asked in the sync, nothing is asked.
pub(crate) async fn answer_in_sync(runtime: &Runtime, path: &Path) -> Result<()> {
    if runtime.resources.sync_now() == 0 {
        return Ok(());
    }
    let Some(claims) = listing_claims(runtime, path).await? else {
        return Ok(());
    };
    answer_claims(runtime, path, FedFact::Listing, &claims).await?;
    Ok(())
}

async fn listing_claims(runtime: &Runtime, listing: &Path) -> Result<Option<Vec<FeedClaim>>> {
    let request = PageRequest {
        cursor: None,
        validator: None,
    };
    match runtime
        .list_children(listing, request, runtime.resources.current_epoch())
        .await?
    {
        ListOutcome::Feed(claims) => Ok(Some(claims)),
        ListOutcome::Entries(_) | ListOutcome::Subtree(_) | ListOutcome::Unchanged => Ok(None),
    }
}

/// `path`, below an anchor of the object of `anchor` or at it, under
/// `anchor`: the path with the leading segments of its anchor replaced by
/// `anchor`.
fn under(anchor: &Path, path: &Path) -> Result<Path> {
    path.segments()
        .skip(anchor.segments().count())
        .try_fold(anchor.clone(), |at, segment| {
            at.join(segment)
                .map_err(|error| TreeError::internal(error.to_string()))
        })
}

/// Whether the projection has the fact an operation on `path` serves for
/// the start of an answer: its positive lookup, or its file or its complete
/// listing before the time after which the part that wrote it counts it as
/// lacking.
fn has_fact(runtime: &Runtime, path: &Path, fact: FedFact) -> Result<bool> {
    let resources = &runtime.resources;
    let found = match fact {
        FedFact::Lookup => {
            return resources
                .lookup_payload(path)
                .map(|lookup| matches!(lookup, Some(LookupPayload::Positive(_))))
                .map_err(|error| TreeError::internal(error.to_string()));
        },
        FedFact::Listing => complete_listing(runtime, path)?,
        // The record alone answers: the body of a file has no bound.
        FedFact::File => resources
            .file_body(path, None)
            .map(|body| body.is_some())
            .map_err(|error| TreeError::internal(error.to_string()))?,
    };
    let until = resources
        .fed_until(path)
        .map_err(|error| TreeError::internal(error.to_string()))?;
    Ok(found && !crate::cache::expired(until, resources.sync_now()))
}

/// Whether the projection has a complete listing of `path`.
fn complete_listing(runtime: &Runtime, path: &Path) -> Result<bool> {
    runtime
        .resources
        .dirents_payload(path)
        .map(|listing| listing.is_some_and(|listing| listing.is_complete_offline()))
        .map_err(|error| TreeError::internal(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{answering_claim, under};
    use crate::cache::FeedState;
    use crate::object_id::ObjectId;
    use crate::ops::feed::{FeedClaim, FeedObject};
    use omnifs_core::path::Path;

    #[test]
    fn an_unlisted_own_feed_answers_for_its_paths_whatever_feed_above_it_ended() {
        let claim = |path: &str, own: bool| FeedClaim {
            object: FeedObject {
                id: ObjectId::from_bytes(path.as_bytes().to_vec()),
                path: Path::parse(path).unwrap(),
            },
            starts: true,
            own,
            full_listings: false,
        };
        let state = |ended: bool, unlisted: bool| {
            Some(FeedState {
                version: 1,
                ended,
                due_in: 1,
                token: [0; 32],
                kept: 0,
                unlisted,
            })
        };
        let claims = [claim("/drive", false), claim("/drive/files/x", true)];
        // The feed above ended: it answers for an item that its inventory
        // has, and the own feed of an item that no inventory has answers
        // itself.
        assert_eq!(
            answering_claim(&claims, &[state(true, false), state(true, false)]),
            0
        );
        assert_eq!(
            answering_claim(&claims, &[state(true, false), state(true, true)]),
            1
        );
        // Before any feed above ended, the innermost claim answers.
        assert_eq!(answering_claim(&claims, &[state(false, false), None]), 1);
        // A feed that is not an own feed is never unlisted.
        let plain = [claim("/shelf", false), claim("/shelf/books/1", false)];
        assert_eq!(
            answering_claim(&plain, &[state(true, false), state(true, true)]),
            0
        );
    }

    #[test]
    fn a_path_below_an_anchor_of_another_facet_is_under_the_anchor_of_the_claim() {
        let anchor = Path::parse("/octocat/hello/issues/all/3").unwrap();
        for (path, expected) in [
            (
                "/octocat/hello/issues/open/3/comments/7",
                "/octocat/hello/issues/all/3/comments/7",
            ),
            (
                "/octocat/hello/issues/open/3",
                "/octocat/hello/issues/all/3",
            ),
            (
                "/octocat/hello/issues/all/3/title",
                "/octocat/hello/issues/all/3/title",
            ),
        ] {
            assert_eq!(
                under(&anchor, &Path::parse(path).unwrap())
                    .unwrap()
                    .as_str(),
                expected,
                "{path}"
            );
        }
    }
}

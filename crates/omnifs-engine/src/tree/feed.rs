//! Serving fed paths: the answering feed among the claims of an operation,
//! and when it answers before the operation is served from the projection.

use crate::Runtime;
use crate::cache::{FeedState, RecordKind};
use crate::clock::now_millis;
use crate::ops::feed::{FeedClaim, FeedStart};
use crate::ops::namespace::{ListOutcome, PageRequest};
use crate::view::LookupPayload;
use omnifs_core::path::Path;

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
/// below. A feed with kept parts of a later answer continues that
/// answer, whatever its deadline, outside the rate-limit window of the mount.
/// Else a read or a listing that starts that feed without its fact in the
/// projection, and a lookup that starts it without its positive lookup before
/// the end of the first answer of the feed, or below the anchor of an own
/// feed, take an answer with `lacks` and their path under the anchor of the
/// innermost claim as `start`; after that end, a lookup is served from the
/// projection, as not found for a path that it lacks. Any other operation
/// takes an answer once the feed is past its deadline, outside the rate-limit
/// window of the mount, with `due` and its path for an operation on a
/// starting path of the feed. An operation that waited on another answer of
/// the same feed, or whose feed another answer moved before its flight,
/// decides again after it.
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
    let answering = loop {
        let states = claims
            .iter()
            .map(|claim| runtime.resources.feed_state(claim.object.id.as_bytes()))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| TreeError::internal(error.to_string()))?;
        let answering = states
            .iter()
            .position(|state| state.as_ref().is_some_and(|state| state.ended))
            .unwrap_or(claims.len() - 1);
        let (claim, state) = (&claims[answering], &states[answering]);
        if matches!(fact, FedFact::Lookup)
            && let Some(listing) = incomplete_listing(runtime, at, &claim.object.path)?
            && let Some(claims) = listing_claims(runtime, &listing).await?
        {
            Box::pin(answer_claims(runtime, &listing, FedFact::Listing, &claims)).await?;
            return Ok((at.clone(), under_anchor.clone()));
        }
        let kept = runtime
            .resources
            .has_kept_parts(claim.object.id.as_bytes())
            .map_err(|error| TreeError::internal(error.to_string()))?;
        let first_answer = state.as_ref().is_none_or(|state| !state.ended);
        let below_own = claim.own && path.segments().count() > claim.object.path.segments().count();
        let start = if kept {
            runtime.rate_limited_until().is_none().then_some(None)
        } else if claim.starts
            && (first_answer || below_own || !matches!(fact, FedFact::Lookup))
            && !has_fact(runtime, at, fact)?
        {
            Some(Some(FeedStart::Lacks(under_anchor.clone())))
        } else if runtime.rate_limited_until().is_none() && past_deadline(state.as_ref()) {
            Some(claim.starts.then(|| FeedStart::Due(under_anchor.clone())))
        } else {
            None
        };
        let Some(start) = start else {
            break answering;
        };
        if runtime
            .feed(&claim.object, start.as_ref(), state.as_ref())
            .await?
        {
            break answering;
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
            && !(runtime.rate_limited_until().is_none() && past_deadline(state.as_ref()))
        {
            return Ok(());
        }
        let start = FeedStart::Lacks(under_anchor);
        if runtime
            .feed(&claim.object, Some(&start), state.as_ref())
            .await?
        {
            return Ok(());
        }
    }
}

/// Whether a feed has no state or is past its deadline.
fn past_deadline(state: Option<&FeedState>) -> bool {
    state.is_none_or(|state| state.deadline <= now_millis())
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
        FedFact::File => resources
            .cache_get(path, RecordKind::File, None)
            .map(|record| record.is_some())
            .map_err(|error| TreeError::internal(error.to_string()))?,
    };
    let until = resources
        .fed_until(path)
        .map_err(|error| TreeError::internal(error.to_string()))?;
    Ok(found && until.is_none_or(|until| now_millis() < until))
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
    use super::under;
    use omnifs_core::path::Path;

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

//! The helpers of a test of a provider over the namespace of a
//! [`RuntimeHarness`]: an operation with the answers of the upstream to the
//! callouts it makes, and the lookup, the names, and the bytes of a path.
//!
//! A path is absolute from the root of the namespace, with its mount first:
//! `/notion/pages`.

use std::future::Future;
use std::time::Duration;

use omnifs_core::path::Path;
use omnifs_engine::test_support::PendingTestCallout;
use omnifs_engine::{DirCursor, LookupAnswer, Namespace, NsError};
use omnifs_wit::host::types::{BlobFetchRequest, Callout, CalloutResult, HttpRequest};

use crate::RuntimeHarness;

/// How long [`answering`] waits for an operation and its callouts.
const ANSWERED_WITHIN: Duration = Duration::from_secs(10);

/// The answer of the upstream of a test to one callout.
pub enum Answer {
    /// The answer to a fetch, whose body the provider reads.
    Fetch(CalloutResult),
    /// The status and the body of the answer to a blob fetch, whose body
    /// the host holds.
    Blob(u16, Vec<u8>),
}

/// The request of a callout that [`answering`] answered.
#[derive(Debug, Clone)]
pub enum Request {
    Fetch(HttpRequest),
    Blob(BlobFetchRequest),
}

/// The next callout of the mount of `harness`, which waits for its answer.
pub async fn next_callout(harness: &RuntimeHarness) -> PendingTestCallout {
    loop {
        match harness.runtime.try_recv_test_callout() {
            Some(callout) => break callout,
            None => tokio::time::sleep(Duration::from_millis(2)).await,
        }
    }
}

/// Run `operation`, answer the callouts it makes in order with `answers`,
/// and return its output with the requests of those callouts. The test
/// fails for an operation that makes fewer callouts, a callout of another
/// kind than its answer, or an operation that does not end with its last
/// answer, as one that makes a callout more.
pub async fn answering<T>(
    harness: &RuntimeHarness,
    operation: impl Future<Output = T>,
    answers: impl IntoIterator<Item = Answer>,
) -> (T, Vec<Request>) {
    let answer = async {
        let mut requests = Vec::new();
        for answer in answers {
            let callout = next_callout(harness).await;
            match (callout.callout().clone(), answer) {
                (Callout::Fetch(request), Answer::Fetch(result)) => {
                    requests.push(Request::Fetch(request));
                    callout.answer(result);
                },
                (Callout::FetchBlob(request), Answer::Blob(status, body)) => {
                    requests.push(Request::Blob(request));
                    harness
                        .runtime
                        .answer_test_blob(callout, status, &body)
                        .await;
                },
                (callout, _) => panic!("another callout than its answer: {callout:?}"),
            }
        }
        requests
    };
    tokio::time::timeout(ANSWERED_WITHIN, async { tokio::join!(operation, answer) })
        .await
        .expect("the operation makes exactly the answered callouts")
}

/// Run `operation`, which makes no callout.
pub async fn served<T>(harness: &RuntimeHarness, operation: impl Future<Output = T>) -> T {
    answering(harness, operation, []).await.0
}

/// Look up `path` a segment at a time, to its first missing segment.
pub async fn lookup(harness: &RuntimeHarness, path: &str) -> Result<LookupAnswer, NsError> {
    let path = Path::parse(path).expect("a test names a protocol path");
    let mut segments = path.segments();
    let mount = segments.next().expect("a test names a path under a mount");
    let mut answer = harness.namespace.lookup(Path::root(), mount).await?;
    for segment in segments {
        if answer.is_missing() {
            break;
        }
        answer = harness.namespace.lookup(answer.path, segment).await?;
    }
    Ok(answer)
}

/// The names of the directory at `path`, sorted.
pub async fn names(harness: &RuntimeHarness, path: &str) -> Result<Vec<String>, NsError> {
    let dir = lookup(harness, path).await?;
    let page = harness
        .namespace
        .readdir(dir.path, DirCursor::start(), 0)
        .await?;
    let mut names = page
        .entries
        .into_iter()
        .map(|entry| entry.name)
        .collect::<Vec<_>>();
    names.sort();
    Ok(names)
}

/// The whole of the file at `path`, as text.
pub async fn read(harness: &RuntimeHarness, path: &str) -> Result<String, NsError> {
    let file = lookup(harness, path).await?;
    let read = harness.namespace.read(file.path, 0, u32::MAX).await?;
    Ok(String::from_utf8(read.bytes).expect("a test reads a file of text"))
}

/// The text of the part `part` of a query of a URL: each `%XX` as its byte
/// and each `+` as a space.
pub fn decoded(part: &str) -> String {
    let bytes = part.as_bytes();
    let mut text = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'%' => {
                let hex = std::str::from_utf8(&bytes[at + 1..at + 3])
                    .expect("a percent escape has two hexadecimal digits");
                text.push(
                    u8::from_str_radix(hex, 16)
                        .expect("a percent escape has two hexadecimal digits"),
                );
                at += 3;
            },
            b'+' => {
                text.push(b' ');
                at += 1;
            },
            byte => {
                text.push(byte);
                at += 1;
            },
        }
    }
    String::from_utf8(text).expect("a part of a query is UTF-8")
}

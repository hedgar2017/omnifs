//! Slack provider route-test helpers.

use std::future::Future;
use std::time::Duration;

use omnifs_core::path::Path;
use omnifs_engine::{DirCursor, LookupAnswer, Namespace, NsError};
use omnifs_itest::{RuntimeHarness, make_initialized_runtime};
use omnifs_wit::host::types::{
    Callout, CalloutError, CalloutResult, ErrorKind, Header, HttpRequest, HttpResponse,
};
use serde_json::{Value, json};

/// Past the deadline of a feed, `DYNAMIC_TTL_MILLIS` after its last answer.
pub const PAST_DEADLINE: Duration = Duration::from_millis(3_200);

pub fn slack_harness() -> RuntimeHarness {
    make_initialized_runtime(
        r#"
        {
            "provider": "omnifs_provider_slack.wasm",
            "mount": "slack",
            "auth": {
                "type": "static-token",
                "scheme": "token"
            }
        }
    "#,
    )
}

/// The name under which a request of a fixture has the version of the bytes
/// that a blob fetch asks for.
const VERSION: &str = "blob-version";

/// The answer of Slack to a request of a fixture.
pub enum Answer {
    /// The answer to a request whose body the provider reads.
    Fetch(CalloutResult),
    /// The status and the body of the answer to a request whose body the
    /// host holds as a blob.
    Blob(u16, Vec<u8>),
}

/// The answer `status` with `body` to a request of the bytes of a file.
pub fn blob(status: u16, body: &[u8]) -> Answer {
    Answer::Blob(status, body.to_vec())
}

/// Run a namespace operation, answer the callouts it makes in order with
/// `answers`, and return its output with the requests of those callouts. An
/// operation that makes another number of callouts times out.
pub async fn answering<T>(
    harness: &RuntimeHarness,
    operation: impl Future<Output = T>,
    answers: Vec<Answer>,
) -> (T, Vec<HttpRequest>) {
    let runtime = &harness.runtime;
    let answer = async {
        let mut requests = Vec::new();
        for answer in answers {
            let callout = loop {
                match runtime.try_recv_test_callout() {
                    Some(callout) => break callout,
                    None => tokio::time::sleep(Duration::from_millis(2)).await,
                }
            };
            match (callout.callout(), answer) {
                (Callout::Fetch(request), Answer::Fetch(result)) => {
                    requests.push(request.clone());
                    callout.answer(result);
                },
                (Callout::FetchBlob(request), Answer::Blob(status, body)) => {
                    // The version of the bytes of the request, as a header
                    // of the request of the fixture.
                    let mut headers = request.headers.clone();
                    headers.extend(request.version.clone().map(|value| Header {
                        name: VERSION.to_owned(),
                        value,
                    }));
                    requests.push(HttpRequest {
                        method: request.method.clone(),
                        url: request.url.clone(),
                        headers,
                        body: request.body.clone(),
                    });
                    runtime.answer_test_blob(callout, status, &body);
                },
                (callout, _) => panic!("another callout than its answer: {callout:?}"),
            }
        }
        requests
    };
    let (output, requests) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(operation, answer)
    })
    .await
    .expect("the operation makes exactly the answered requests");
    (output, requests)
}

/// Run `operation`, which makes no request.
pub async fn served<T>(harness: &RuntimeHarness, operation: impl Future<Output = T>) -> T {
    answering(harness, operation, Vec::new()).await.0
}

fn decoded(part: &str) -> String {
    let bytes = part.as_bytes();
    let mut text = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'%' => {
                let hex = std::str::from_utf8(&bytes[at + 1..at + 3]).unwrap();
                text.push(u8::from_str_radix(hex, 16).unwrap());
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
    String::from_utf8(text).unwrap()
}

/// Check that `request` is a `GET` of the method `method` of the Web API of
/// Slack with the parameters `query` in their order and then `cursor`.
fn expect(request: &HttpRequest, method: &str, query: &[(&str, &str)], cursor: Option<&str>) {
    assert_eq!(request.method, "GET", "{}", request.url);
    let (url, sent) = request.url.split_once('?').unwrap_or((&request.url, ""));
    assert_eq!(url, format!("https://slack.com/api/{method}"));
    let sent = sent
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap();
            (decoded(name), decoded(value))
        })
        .collect::<Vec<_>>();
    let query = query
        .iter()
        .copied()
        .chain(cursor.map(|cursor| ("cursor", cursor)))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect::<Vec<_>>();
    assert_eq!(sent, query, "{}", request.url);
}

/// Check that `request` is that of the page of the users from `cursor`.
pub fn expect_users(request: &HttpRequest, cursor: Option<&str>) {
    expect(request, "users.list", &[("limit", "999")], cursor);
}

/// Check that `request` is that of the page of the channels from `cursor`.
pub fn expect_channels(request: &HttpRequest, cursor: Option<&str>) {
    expect(
        request,
        "users.conversations",
        &[
            ("types", "public_channel,private_channel"),
            ("exclude_archived", "false"),
            ("limit", "999"),
        ],
        cursor,
    );
}

/// Check that `request` is that of the page of the history of `channel` from
/// `cursor`, of its new messages after `oldest`.
pub fn expect_history(
    request: &HttpRequest,
    channel: &str,
    oldest: Option<&str>,
    cursor: Option<&str>,
) {
    let mut query = vec![("channel", channel)];
    query.extend(oldest.map(|oldest| ("oldest", oldest)));
    query.push(("limit", "999"));
    expect(request, "conversations.history", &query, cursor);
}

/// Check that `request` is that of the message `ts` of `channel` by its
/// `ts`.
pub fn expect_message(request: &HttpRequest, channel: &str, ts: &str) {
    expect(
        request,
        "conversations.history",
        &[
            ("channel", channel),
            ("latest", ts),
            ("oldest", ts),
            ("inclusive", "true"),
            ("limit", "1"),
        ],
        None,
    );
}

/// Check that `request` is that of the page of the replies of the message
/// `ts` of `channel` from `cursor`.
pub fn expect_replies(request: &HttpRequest, channel: &str, ts: &str, cursor: Option<&str>) {
    expect(
        request,
        "conversations.replies",
        &[("channel", channel), ("ts", ts), ("limit", "1000")],
        cursor,
    );
}

/// Check that `request` is that of the bytes of the file `id` with `name`,
/// for the host to hold, with `version`.
pub fn expect_file(request: &HttpRequest, id: &str, name: &str, version: Option<&str>) {
    assert_eq!(request.method, "GET", "{}", request.url);
    assert_eq!(request.url, download(id, name));
    let asked = request
        .headers
        .iter()
        .find(|header| header.name == VERSION)
        .map(|header| header.value.as_str());
    assert_eq!(asked, version, "{}", request.url);
}

/// An answer of Slack with `status`, `headers`, and `body`.
fn answer(status: u16, headers: &[(&str, &str)], body: &Value) -> Answer {
    Answer::Fetch(CalloutResult::HttpResponse(HttpResponse {
        status,
        headers: headers
            .iter()
            .map(|(name, value)| Header {
                name: (*name).to_string(),
                value: (*value).to_string(),
            })
            .collect(),
        body: body.to_string().into_bytes(),
    }))
}

/// An answer `200` with `body`.
pub fn ok(body: &Value) -> Answer {
    answer(200, &[("Content-Type", "application/json")], body)
}

/// An answer of Slack with `ok` of `false` and `error`.
pub fn failure(error: &str) -> Answer {
    ok(&json!({ "ok": false, "error": error }))
}

/// An answer `429` of Slack with the seconds `retry_after` of its
/// `Retry-After`.
pub fn rate_limited(retry_after: &str) -> Answer {
    answer(
        429,
        &[
            ("Content-Type", "application/json"),
            ("Retry-After", retry_after),
        ],
        &json!({ "ok": false, "error": "ratelimited" }),
    )
}

/// A failure of the network.
pub fn network() -> Answer {
    Answer::Fetch(CalloutResult::CalloutError(CalloutError {
        kind: ErrorKind::Network,
        message: "the network of a fixture failed".to_string(),
        retryable: true,
    }))
}

/// A page of a method of Slack with `objects` under `key`, with more after
/// them for `next_cursor`.
pub fn page(key: &str, objects: &[Value], next_cursor: Option<&str>) -> Value {
    let mut page = json!({
        "ok": true,
        "response_metadata": { "next_cursor": next_cursor.unwrap_or_default() },
    });
    page[key] = objects.into();
    page
}

/// The `ts` of a fixture `n`.
pub fn ts(n: u32) -> String {
    format!("17000000{n:02}.000100")
}

/// A user `id` with the `real_name` `name`, none for `null`.
pub fn user(id: &str, name: Option<&str>) -> Value {
    json!({
        "id": id,
        "team_id": "T0FIXTURE",
        "name": id.to_lowercase(),
        "deleted": false,
        "profile": { "real_name": name, "display_name": "" },
    })
}

/// A channel `id` with `name`.
pub fn channel(id: &str, name: &str) -> Value {
    json!({
        "id": id,
        "name": name,
        "is_channel": true,
        "is_private": false,
        "is_archived": false,
    })
}

/// A message `n` by `user`, none for a message without one, with `text`.
pub fn message(n: u32, user: Option<&str>, text: &str) -> Value {
    let mut message = json!({ "type": "message", "ts": ts(n), "text": text });
    if let Some(user) = user {
        message["user"] = user.into();
    }
    message
}

/// `message` as the parent of a thread with `replies` and the latest reply
/// `latest`.
pub fn thread(mut message: Value, replies: u64, latest: u32) -> Value {
    message["thread_ts"] = message["ts"].clone();
    message["reply_count"] = replies.into();
    message["latest_reply"] = ts(latest).into();
    message
}

/// A reply `n` of the message `parent` by `user` with `text`.
pub fn reply(n: u32, parent: u32, user: &str, text: &str) -> Value {
    let mut reply = message(n, Some(user), text);
    reply["thread_ts"] = ts(parent).into();
    reply["parent_user_id"] = "U0FIXTURE1".into();
    reply
}

/// `message` with `files`.
pub fn with_files(mut message: Value, files: &[Value]) -> Value {
    message["files"] = files.into();
    message
}

/// The `url_private_download` of the file `id` with `name`.
pub fn download(id: &str, name: &str) -> String {
    format!("https://files.slack.com/files-pri/T0FIXTURE-{id}/download/{name}")
}

/// A file `id` with `name`, edited at `edited`.
pub fn file(id: &str, name: &str, edited: u64) -> Value {
    json!({
        "id": id,
        "name": name,
        "mimetype": "text/plain",
        "url_private": format!("https://files.slack.com/files-pri/T0FIXTURE-{id}/{name}"),
        "url_private_download": download(id, name),
        "edit_timestamp": edited,
    })
}

/// A file `id` that the workspace hides, without its bytes.
pub fn hidden(id: &str) -> Value {
    json!({ "id": id, "mode": "hidden_by_limit" })
}

/// Look up `path` under the mount a segment at a time, to its first missing
/// segment.
pub async fn lookup(harness: &RuntimeHarness, path: &str) -> Result<LookupAnswer, NsError> {
    let mut answer = harness.namespace.lookup(Path::root(), "slack").await?;
    for segment in path.split('/') {
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
    listed(harness, dir.path).await
}

/// The names of the directory at `path` under the mount by its path alone,
/// without a lookup, as by a filehandle held across a change of the
/// projection, sorted.
pub async fn names_at(harness: &RuntimeHarness, path: &str) -> Result<Vec<String>, NsError> {
    listed(harness, Path::parse(&format!("/slack/{path}")).unwrap()).await
}

async fn listed(harness: &RuntimeHarness, dir: Path) -> Result<Vec<String>, NsError> {
    let page = harness
        .namespace
        .readdir(dir, DirCursor::start(), 0)
        .await?;
    let mut names = page
        .entries
        .into_iter()
        .map(|entry| entry.name)
        .collect::<Vec<_>>();
    names.sort();
    Ok(names)
}

pub async fn read(harness: &RuntimeHarness, path: &str) -> Result<String, NsError> {
    let file = lookup(harness, path).await?;
    let read = harness.namespace.read(file.path, 0, u32::MAX).await?;
    Ok(String::from_utf8(read.bytes).unwrap())
}

//! Google Drive provider route-test helpers.

use std::future::Future;
use std::time::Duration;

use omnifs_core::path::Path;
use omnifs_engine::{DirCursor, LookupAnswer, Namespace, NsError};
use omnifs_itest::{RuntimeHarness, make_initialized_runtime};
use omnifs_wit::host::types::{Callout, CalloutResult, Header, HttpRequest, HttpResponse};
use serde_json::{Value, json};

/// Past the deadline of a feed, `DYNAMIC_TTL_MILLIS` after its last answer.
pub const PAST_DEADLINE: Duration = Duration::from_millis(3_200);

/// The fields of a file or a folder that the provider asks for.
pub const FIELDS: &str = "id,name,mimeType,description,parents,driveId,trashed,createdTime,modifiedTime,size,md5Checksum,headRevisionId,webViewLink,owners(displayName,emailAddress),lastModifyingUser(displayName,emailAddress)";

pub fn drive_harness() -> RuntimeHarness {
    make_initialized_runtime(
        r#"
        {
            "provider": "omnifs_provider_google_drive.wasm",
            "mount": "google-drive",
            "auth": {
                "type": "oauth",
                "scheme": "service-account"
            }
        }
    "#,
    )
}

/// The answer of Google Drive to a request of a fixture.
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
                    requests.push(HttpRequest {
                        method: request.method.clone(),
                        url: request.url.clone(),
                        headers: request.headers.clone(),
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

/// Check that `request` is a `GET` of `path` of the API of Google Drive
/// with the parameters `query` in their order.
pub fn expect(request: &HttpRequest, path: &str, query: &[(&str, &str)]) {
    assert_eq!(request.method, "GET", "{}", request.url);
    let (url, sent) = request.url.split_once('?').unwrap_or((&request.url, ""));
    assert_eq!(url, format!("https://www.googleapis.com/drive/v3{path}"));
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
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect::<Vec<_>>();
    assert_eq!(sent, query, "{}", request.url);
}

/// Check that `request` is that of the token of the changes after now.
pub fn expect_start(request: &HttpRequest) {
    expect(
        request,
        "/changes/startPageToken",
        &[("supportsAllDrives", "true")],
    );
}

/// Check that `request` is that of a full listing from the token `page`.
pub fn expect_listing(request: &HttpRequest, page: Option<&str>) {
    let fields = format!("nextPageToken,incompleteSearch,files({FIELDS})");
    let mut query = vec![
        ("corpora", "allDrives"),
        ("includeItemsFromAllDrives", "true"),
        ("supportsAllDrives", "true"),
        ("q", "trashed = false"),
        ("pageSize", "1000"),
        ("fields", fields.as_str()),
    ];
    if let Some(page) = page {
        query.push(("pageToken", page));
    }
    expect(request, "/files", &query);
}

/// Check that `request` is that of the changes from the token `page`.
pub fn expect_changes(request: &HttpRequest, page: &str) {
    let fields = format!(
        "nextPageToken,newStartPageToken,changes(changeType,fileId,removed,file({FIELDS}))"
    );
    expect(
        request,
        "/changes",
        &[
            ("pageToken", page),
            ("includeItemsFromAllDrives", "true"),
            ("supportsAllDrives", "true"),
            ("includeRemoved", "true"),
            ("pageSize", "1000"),
            ("fields", fields.as_str()),
        ],
    );
}

/// Check that `request` is that of the file or the folder `id` by its ID.
pub fn expect_item(request: &HttpRequest, id: &str) {
    expect(
        request,
        &format!("/files/{id}"),
        &[("supportsAllDrives", "true"), ("fields", FIELDS)],
    );
}

/// Check that `request` is that of the bytes of the revision `revision` of
/// the uploaded file `id`.
pub fn expect_bytes(request: &HttpRequest, id: &str, revision: &str) {
    expect(
        request,
        &format!("/files/{id}/revisions/{revision}"),
        &[("alt", "media")],
    );
}

/// Check that `request` is that of the export of the file `id` of Google
/// with `mime_type`.
pub fn expect_export(request: &HttpRequest, id: &str, mime_type: &str) {
    expect(
        request,
        &format!("/files/{id}/export"),
        &[("mimeType", mime_type)],
    );
}

/// An answer of Google Drive with `status` and the bytes `body`.
pub fn bytes(status: u16, body: &[u8]) -> Answer {
    Answer::Fetch(CalloutResult::HttpResponse(HttpResponse {
        status,
        headers: vec![Header {
            name: "Content-Type".to_owned(),
            value: "application/octet-stream".to_owned(),
        }],
        body: body.to_vec(),
    }))
}

/// An answer `200` with `body`.
pub fn ok(body: &Value) -> Answer {
    bytes(200, body.to_string().as_bytes())
}

/// An error answer of Google Drive with `status` and `reason`.
pub fn failure(status: u16, reason: &str) -> Answer {
    bytes(
        status,
        json!({
            "error": {
                "code": status,
                "message": format!("the {reason} of a fixture"),
                "errors": [{ "domain": "global", "reason": reason, "message": "a fixture" }],
            }
        })
        .to_string()
        .as_bytes(),
    )
}

/// An uploaded file `id` with `name`, the head revision `revision` and
/// `size` bytes.
pub fn uploaded(id: &str, name: &str, revision: &str, size: u64) -> Value {
    json!({
        "id": id,
        "name": name,
        "mimeType": "text/markdown",
        "parents": ["folder-1"],
        "trashed": false,
        "createdTime": "2026-11-21T09:00:00.000Z",
        "modifiedTime": "2026-11-21T10:00:00.000Z",
        "size": size.to_string(),
        "headRevisionId": revision,
    })
}

/// An item `id` of the type `kind` of Google with `name`, modified at
/// `modified`.
pub fn google(id: &str, kind: &str, name: &str, modified: &str) -> Value {
    json!({
        "id": id,
        "name": name,
        "mimeType": format!("application/vnd.google-apps.{kind}"),
        "trashed": false,
        "createdTime": "2026-11-21T09:00:00.000Z",
        "modifiedTime": modified,
    })
}

/// `object` in the trash.
pub fn trashed(mut object: Value) -> Value {
    object["trashed"] = true.into();
    object
}

/// A page of a full listing with `files` and the token of its next page.
pub fn files(files: &[Value], next: Option<&str>) -> Value {
    let mut list = json!({ "files": files, "incompleteSearch": false });
    if let Some(next) = next {
        list["nextPageToken"] = next.into();
    }
    list
}

/// A change that brings `object`.
pub fn changed(object: &Value) -> Value {
    json!({ "changeType": "file", "fileId": object["id"], "removed": false, "file": object })
}

/// A change of the item `id` that the account no longer has.
pub fn removed(id: &str) -> Value {
    json!({ "changeType": "file", "fileId": id, "removed": true })
}

/// The last page of the changes with `changes` and the token `next` of the
/// changes after it.
pub fn changes(changes: &[Value], next: &str) -> Value {
    json!({ "changes": changes, "newStartPageToken": next })
}

/// Look up `path` under the mount a segment at a time, to its first missing
/// segment.
pub async fn lookup(harness: &RuntimeHarness, path: &str) -> Result<LookupAnswer, NsError> {
    let mut answer = harness
        .namespace
        .lookup(Path::root(), "google-drive")
        .await?;
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

pub async fn read(harness: &RuntimeHarness, path: &str) -> Result<String, NsError> {
    let file = lookup(harness, path).await?;
    let read = harness.namespace.read(file.path, 0, u32::MAX).await?;
    assert!(read.eof, "{path}");
    Ok(String::from_utf8(read.bytes).unwrap())
}

/// At most `len` bytes of the file at `path` from `offset`, and whether
/// they reach its end.
pub async fn read_range(
    harness: &RuntimeHarness,
    path: &str,
    offset: u64,
    len: u32,
) -> Result<(String, bool), NsError> {
    let file = lookup(harness, path).await?;
    let read = harness.namespace.read(file.path, offset, len).await?;
    Ok((String::from_utf8(read.bytes).unwrap(), read.eof))
}

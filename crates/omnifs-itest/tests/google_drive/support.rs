//! The requests of the mount `google-drive` that its tests answer, and the
//! objects of Google Drive of those answers.

use std::time::Duration;

use omnifs_engine::{Namespace, NsError};
use omnifs_itest::support::{Answer, Request, decoded, lookup};
use omnifs_itest::{RuntimeHarness, make_initialized_runtime};
use omnifs_wit::host::types::{CalloutResult, Header, HttpResponse};
use serde_json::{Value, json};

/// Past the deadline of a feed, `DYNAMIC_TTL_MILLIS` after its last answer.
pub const PAST_DEADLINE: Duration = Duration::from_millis(3_200);

/// The fields of an item that the provider asks for.
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

/// The path of `path` under the mount.
pub fn at(path: &str) -> String {
    format!("/google-drive/{path}")
}

/// The answer `status` with `body` to a blob fetch.
pub fn blob(status: u16, body: &[u8]) -> Answer {
    Answer::Blob(status, body.to_vec())
}

/// At most `len` bytes of the file at `path` under the mount from `offset`,
/// and whether they reach its end.
pub async fn read_range(
    harness: &RuntimeHarness,
    path: &str,
    offset: u64,
    len: u32,
) -> Result<(String, bool), NsError> {
    let file = lookup(harness, &at(path)).await?;
    let read = harness.namespace.read(file.path, offset, len).await?;
    Ok((String::from_utf8(read.bytes).unwrap(), read.eof))
}

/// The method, the URL, and whether the host follows its redirects, of a
/// request.
fn sent(request: &Request) -> (&str, &str, bool) {
    match request {
        Request::Fetch(request) => (&request.method, &request.url, request.follow_redirects),
        Request::Blob(request) => (&request.method, &request.url, request.follow_redirects),
    }
}

/// Check that `request` is a `GET` of `path` of the API of Google Drive
/// with the parameters `query` in their order, which follows redirects as
/// `follows` says.
fn expect(request: &Request, path: &str, query: &[(&str, &str)], follows: bool) {
    let (method, url, follow_redirects) = sent(request);
    assert_eq!(method, "GET", "{url}");
    assert_eq!(follow_redirects, follows, "{url}");
    let (base, sent) = url.split_once('?').unwrap_or((url, ""));
    assert_eq!(base, format!("https://www.googleapis.com/drive/v3{path}"));
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
    assert_eq!(sent, query, "{url}");
}

/// Check that `request` is that of the token of the changes after now.
pub fn expect_start(request: &Request) {
    expect(
        request,
        "/changes/startPageToken",
        &[("supportsAllDrives", "true")],
        false,
    );
}

/// The corpus of a request of a full listing.
#[derive(Clone, Copy)]
pub enum Corpus<'a> {
    User,
    Drive(&'a str),
}

/// Check that `request` is that of a page of a full listing of `corpus`
/// from the token `page`.
pub fn expect_listing(request: &Request, corpus: Corpus<'_>, page: Option<&str>) {
    let fields = format!("nextPageToken,incompleteSearch,files({FIELDS})");
    let mut query = match corpus {
        Corpus::User => vec![("corpora", "user")],
        Corpus::Drive(id) => vec![("corpora", "drive"), ("driveId", id)],
    };
    query.extend([
        ("includeItemsFromAllDrives", "true"),
        ("supportsAllDrives", "true"),
        ("q", "trashed = false"),
        ("pageSize", "1000"),
        ("fields", fields.as_str()),
    ]);
    if let Some(page) = page {
        query.push(("pageToken", page));
    }
    expect(request, "/files", &query, false);
}

/// Check that `request` is that of a page of the shared drives from the
/// token `page`.
pub fn expect_drives(request: &Request, page: Option<&str>) {
    let mut query = vec![("pageSize", "100"), ("fields", "nextPageToken,drives(id)")];
    if let Some(page) = page {
        query.push(("pageToken", page));
    }
    expect(request, "/drives", &query, false);
}

/// Check that `request` is that of the changes from the token `page`.
pub fn expect_changes(request: &Request, page: &str) {
    let fields = format!(
        "nextPageToken,newStartPageToken,changes(changeType,fileId,driveId,removed,file({FIELDS}))"
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
        false,
    );
}

/// Check that `request` is that of the item `id` by its ID.
pub fn expect_item(request: &Request, id: &str) {
    expect(
        request,
        &format!("/files/{id}"),
        &[("supportsAllDrives", "true"), ("fields", FIELDS)],
        false,
    );
}

/// The MD5 and the version of the bytes of a blob fetch.
fn pinned(request: &Request) -> (Option<&str>, Option<&str>) {
    let Request::Blob(request) = request else {
        panic!("a request of bytes is a blob fetch: {request:?}");
    };
    (request.md5.as_deref(), request.version.as_deref())
}

/// Check that `request` is the blob fetch of the bytes of the uploaded file
/// `id` with the MD5 `md5`.
pub fn expect_bytes(request: &Request, id: &str, md5: &str) {
    expect(
        request,
        &format!("/files/{id}"),
        &[("alt", "media"), ("supportsAllDrives", "true")],
        true,
    );
    assert_eq!(pinned(request), (Some(md5), None));
}

/// Check that `request` is the blob fetch of the bytes of the uploaded file
/// `id` of the revision `revision`, without an MD5.
pub fn expect_revision(request: &Request, id: &str, revision: &str) {
    expect(
        request,
        &format!("/files/{id}"),
        &[("alt", "media"), ("supportsAllDrives", "true")],
        true,
    );
    assert_eq!(pinned(request), (None, Some(revision)));
}

/// Check that `request` is the request of the first byte of the uploaded
/// file `id`, whose answer has the reason of a refusal of its bytes.
pub fn expect_first_byte(request: &Request, id: &str) {
    expect(
        request,
        &format!("/files/{id}"),
        &[("alt", "media"), ("supportsAllDrives", "true")],
        true,
    );
    let Request::Fetch(request) = request else {
        panic!("the request of a first byte is inline: {request:?}");
    };
    let range = request
        .headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("range"))
        .map(|header| header.value.as_str());
    assert_eq!(range, Some("bytes=0-0"), "{}", request.url);
}

/// Check that `request` is the blob fetch of the export of the file `id` of
/// Google with `mime_type`, modified at `modified`.
pub fn expect_export(request: &Request, id: &str, mime_type: &str, modified: &str) {
    expect(
        request,
        &format!("/files/{id}/export"),
        &[("mimeType", mime_type)],
        true,
    );
    assert_eq!(pinned(request), (None, Some(modified)));
}

/// Check that `request` is the inline request of the export of the file
/// `id` of Google with `mime_type`, whose answer has the reason of a refusal
/// of its export.
pub fn expect_export_refusal(request: &Request, id: &str, mime_type: &str) {
    assert!(matches!(request, Request::Fetch(_)), "{request:?}");
    expect(
        request,
        &format!("/files/{id}/export"),
        &[("mimeType", mime_type)],
        true,
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
        redirected: None,
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

/// The MD5 of the bytes `body` of a fixture.
pub fn md5(body: &str) -> &'static str {
    match body {
        "notes" => "4358b5009c67d0e31d7fbf1663fcd3bf",
        "notes!" => "c4366dc0b69a2618897dc1d57e976d94",
        "kept" => "4d8b6084f3d167b76cac66a22a91be02",
        "bytes" => "4b3a6218bb3e3a7303e8a171a60fcf92",
        "shared" => "9e81e7b963c71363e2fb3eefcfecfc0e",
        "old" => "149603e6c03516362a8da23f624db945",
        "new" => "22af645d1859cb5ca6da0c484f1f37ea",
        "gone" => "50c1f58be7f5e47e0f53d64c094783c2",
        "busy" => "8bc1b2f84252c3df4edd53e4aad097a7",
        "" => "d41d8cd98f00b204e9800998ecf8427e",
        body => panic!("no MD5 of {body}"),
    }
}

/// An uploaded file `id` with `name` and the bytes `body`.
pub fn uploaded(id: &str, name: &str, body: &str) -> Value {
    sized(id, name, md5(body), body.len() as u64)
}

/// An uploaded file `id` with `name`, the checksum `md5`, the revision of
/// its bytes, and `size` bytes.
pub fn sized(id: &str, name: &str, md5: &str, size: u64) -> Value {
    json!({
        "id": id,
        "name": name,
        "mimeType": "text/markdown",
        "parents": ["folder-1"],
        "trashed": false,
        "createdTime": "2026-11-21T09:00:00.000Z",
        "modifiedTime": "2026-11-21T10:00:00.000Z",
        "size": size.to_string(),
        "md5Checksum": md5,
        "headRevisionId": format!("revision-{}", &md5[..8]),
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

/// `object` without `field`.
pub fn without(mut object: Value, field: &str) -> Value {
    object.as_object_mut().unwrap().remove(field);
    object
}

/// `object` in the shared drive `drive`.
pub fn in_drive(mut object: Value, drive: &str) -> Value {
    object["driveId"] = drive.into();
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

/// A page of the shared drives `ids` and the token of its next page.
pub fn drives(ids: &[&str], next: Option<&str>) -> Value {
    let mut list = json!({
        "drives": ids.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>(),
    });
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

//! Notion provider route-test helpers.

use omnifs_core::path::Path;
use omnifs_engine::{Namespace, NsError};
use omnifs_itest::support::{Answer, Request};
use omnifs_itest::{RuntimeHarness, make_initialized_runtime};
use omnifs_wit::host::types::{
    CalloutError, CalloutResult, ErrorKind, Header, HttpRequest, HttpResponse,
};
use serde_json::{Value, json};

/// The `last_edited_time` of a fixture, by the minute as Notion gives it.
pub const EDITED: &str = "2026-11-21T10:00:00.000Z";

/// The `Notion-Version` of each request of the provider.
pub const VERSION: &str = "2026-03-11";

/// A `Date` of an answer of Notion after the minute of `EDITED`.
pub const LATER: &str = "Sat, 21 Nov 2026 11:00:00 GMT";

pub fn notion_harness() -> RuntimeHarness {
    make_initialized_runtime(
        r#"
        {
            "provider": "omnifs_provider_notion.wasm",
            "mount": "notion",
            "auth": {
                "type": "static-token",
                "scheme": "token"
            }
        }
    "#,
    )
}

/// The value of header `name` of `request`.
pub fn request_header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case(name))
        .map(|header| header.value.as_str())
}

/// Check that `request` is `method` of `path` of the API of Notion with
/// `body`, at the version of the provider.
pub fn expect(request: &Request, method: &str, path: &str, body: Option<&Value>) {
    let Request::Fetch(request) = request else {
        panic!("a request of Notion is a fetch: {request:?}");
    };
    assert!(!request.follow_redirects, "{}", request.url);
    assert_eq!(request.method, method, "{}", request.url);
    assert_eq!(
        request.url,
        format!("https://api.notion.com{path}"),
        "{method}"
    );
    assert_eq!(
        request_header(request, "notion-version"),
        Some(VERSION),
        "{}",
        request.url
    );
    let sent = request
        .body
        .as_deref()
        .map(|bytes| serde_json::from_slice::<Value>(bytes).unwrap());
    assert_eq!(sent.as_ref(), body, "{}", request.url);
}

/// Check that `request` is the search with `page_size` from `cursor`.
pub fn expect_search(request: &Request, page_size: usize, cursor: Option<&str>) {
    let mut body = json!({
        "sort": { "direction": "descending", "timestamp": "last_edited_time" },
        "page_size": page_size,
    });
    if let Some(cursor) = cursor {
        body["start_cursor"] = cursor.into();
    }
    expect(request, "POST", "/v1/search", Some(&body));
}

/// An answer of Notion with `status`, `headers`, and `body`.
pub fn answer(status: u16, headers: &[(&str, &str)], body: &Value) -> Answer {
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
        redirected: None,
    }))
}

/// An answer `200` with `body`, of a `Date` after the minute of `EDITED`.
pub fn ok(body: &Value) -> Answer {
    answer(200, &[("Date", LATER)], body)
}

/// An error answer of Notion with `status` and `code`.
pub fn failure(status: u16, code: &str) -> Answer {
    answer(
        status,
        &[("Date", LATER)],
        &json!({
            "object": "error",
            "status": status,
            "code": code,
            "message": format!("the {code} of a fixture"),
        }),
    )
}

/// An answer `429` of a rate limit, with a `Retry-After` of one second.
pub fn rate_limited() -> Answer {
    answer(
        429,
        &[("Retry-After", "1")],
        &json!({ "object": "error", "status": 429, "code": "rate_limited", "message": "slow down" }),
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

/// The ID of a fixture `n`, as a path names it.
pub fn id(n: u32) -> String {
    format!("{n:032x}")
}

/// The ID of a fixture `n`, as Notion gives it.
pub fn notion_id(n: u32) -> String {
    let id = id(n);
    format!(
        "{}-{}-{}-{}-{}",
        &id[..8],
        &id[8..12],
        &id[12..16],
        &id[16..20],
        &id[20..]
    )
}

fn rich_text(text: &str) -> Value {
    json!([{
        "type": "text",
        "text": { "content": text, "link": null },
        "plain_text": text,
        "href": null,
    }])
}

/// A page `n` with `title`, edited at `edited`, its title in two items.
pub fn page(n: u32, title: &str, edited: &str) -> Value {
    let (first, rest) = title.split_at(title.len() / 2);
    json!({
        "object": "page",
        "id": notion_id(n),
        "last_edited_time": edited,
        "in_trash": false,
        "parent": { "type": "workspace", "workspace": true },
        "properties": {
            "title": {
                "id": "title",
                "type": "title",
                "title": [
                    { "type": "text", "plain_text": first, "href": null },
                    { "type": "text", "plain_text": rest, "href": null },
                ],
            },
        },
    })
}

/// A row `n` of the data source `source`, with `title` in its property
/// `Name` beside other properties.
pub fn row(n: u32, source: u32, title: &str, edited: &str) -> Value {
    json!({
        "object": "page",
        "id": notion_id(n),
        "last_edited_time": edited,
        "in_trash": false,
        "parent": { "type": "data_source_id", "data_source_id": notion_id(source) },
        "properties": {
            "Status": { "id": "s", "type": "select", "select": { "name": "Done" } },
            "Name": { "id": "title", "type": "title", "title": rich_text(title) },
        },
    })
}

/// A data source `n` with `title`.
pub fn data_source(n: u32, title: &str, edited: &str) -> Value {
    json!({
        "object": "data_source",
        "id": notion_id(n),
        "last_edited_time": edited,
        "in_trash": false,
        "title": rich_text(title),
        "properties": {
            "Name": { "id": "title", "name": "Name", "type": "title", "title": {} },
        },
    })
}

/// `object` in the trash.
pub fn trashed(mut object: Value) -> Value {
    object["in_trash"] = true.into();
    object
}

/// A comment `n` by `author`, `null` for none, with `body`.
pub fn comment(n: u32, author: Option<&str>, body: &str) -> Value {
    json!({
        "object": "comment",
        "id": notion_id(n),
        "discussion_id": notion_id(n + 1),
        "rich_text": rich_text(body),
        "display_name": { "type": "user", "resolved_name": author },
    })
}

/// A list of `results`, with more after them for `next_cursor`.
pub fn list(results: &[Value], next_cursor: Option<&str>) -> Value {
    json!({
        "object": "list",
        "results": results,
        "next_cursor": next_cursor,
        "has_more": next_cursor.is_some(),
    })
}

/// The markdown of a page or a block, with its unknown blocks.
pub fn markdown(text: &str, unknown: &[u32]) -> Value {
    json!({
        "object": "page_markdown",
        "markdown": text,
        "truncated": !unknown.is_empty(),
        "unknown_block_ids": unknown.iter().map(|n| notion_id(*n)).collect::<Vec<_>>(),
    })
}

/// The whole of the file at `path` by its path alone, without a lookup, as
/// by a filehandle held across a change of the projection.
pub async fn read_at(harness: &RuntimeHarness, path: &str) -> Result<String, NsError> {
    let file = Path::parse(path).unwrap();
    let read = harness.namespace.read(file, 0, u32::MAX).await?;
    Ok(String::from_utf8(read.bytes).unwrap())
}

//! The feeds of the issues, pull requests, and comments of a repository: its
//! first full listing, a stop of it and its continuation, the checks after
//! it, and the feed of an item before it ends.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use omnifs_core::path::Path;
use omnifs_engine::{DirCursor, Namespace, NsError};
use omnifs_itest::RuntimeHarness;
use omnifs_itest::support::{Answer, Request, answering, lookup, names, read, served};
use omnifs_wit::host::types::{CalloutResult, Header, HttpResponse};
use serde_json::{Value, json};

use super::support::{github_harness, http, request_header};

/// The repository of the tests under the mount.
const REPO: &str = "/github/octocat/hello";

/// The comments of the repository in the order of `updated_at`.
const COMMENTS: &str =
    "/repos/octocat/hello/issues/comments?sort=updated&direction=asc&per_page=100";

/// The REST count of the comments of the repository.
const REMOTE: &str = "/repos/octocat/hello/issues/comments?sort=created&direction=desc&per_page=1";

fn api(path: &str) -> String {
    format!("https://api.github.com{path}")
}

fn at(path: &str) -> String {
    format!("{REPO}/{path}")
}

fn header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request_header(http(request), name)
}

fn answer(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Answer {
    Answer::Fetch(CalloutResult::HttpResponse(HttpResponse {
        status,
        headers: headers
            .iter()
            .map(|(name, value)| Header {
                name: (*name).to_string(),
                value: (*value).to_string(),
            })
            .collect(),
        body: body.to_vec(),
        redirected: None,
    }))
}

fn json_answer(status: u16, headers: &[(&str, &str)], body: &Value) -> Answer {
    answer(status, headers, body.to_string().as_bytes())
}

fn ok(body: &Value) -> Answer {
    json_answer(200, &[], body)
}

fn empty(status: u16, headers: &[(&str, &str)]) -> Answer {
    answer(status, headers, b"")
}

fn not_modified() -> Answer {
    empty(304, &[])
}

/// An answer of GitHub GraphQL about the repository, with `fields` beside its
/// name.
fn graphql(fields: &Value) -> Answer {
    graphql_named("octocat/hello", fields)
}

fn graphql_named(name: &str, fields: &Value) -> Answer {
    let mut repository = fields.clone();
    repository["nameWithOwner"] = json!(name);
    ok(&json!({ "data": { "repository": repository } }))
}

/// An answer of GitHub GraphQL with `null` for each item it cannot resolve,
/// with an error of the type `NOT_FOUND`.
fn graphql_not_found(fields: &Value) -> Answer {
    let mut repository = fields.clone();
    repository["nameWithOwner"] = json!("octocat/hello");
    ok(&json!({
        "data": { "repository": repository },
        "errors": [{ "type": "NOT_FOUND", "message": "Could not resolve to an issue or pull request." }],
    }))
}

/// The query and the variables of a request of GitHub GraphQL.
fn graphql_of(request: &Request) -> (String, Value) {
    let request = http(request);
    assert_eq!(request.method, "POST");
    assert_eq!(request.url, api("/graphql"));
    let body: Value = serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
    assert_eq!(body["variables"]["owner"], "octocat");
    assert_eq!(body["variables"]["name"], "hello");
    (
        body["query"].as_str().unwrap().to_owned(),
        body["variables"].clone(),
    )
}

/// An item as a node of GitHub GraphQL; `author` is `null` for a deleted
/// account.
fn node(number: u64, title: &str, state: &str, updated: &str, comments: u64) -> Value {
    json!({
        "number": number,
        "title": title,
        "body": format!("{title} body"),
        "state": state,
        "author": { "login": "octocat" },
        "updatedAt": updated,
        "comments": { "totalCount": comments },
    })
}

/// A page of the nodes of a kind, with the count of the kind and the cursor
/// of its last node, `next` while a page follows.
fn rows(nodes: &[Value], total: u64, end: Option<&str>, next: bool) -> Value {
    json!({
        "totalCount": total,
        "nodes": nodes,
        "pageInfo": { "hasNextPage": next, "endCursor": end },
    })
}

fn comment(id: u64, number: u64, pull: bool, body: &str, updated: &str) -> Value {
    let kind = if pull { "pull" } else { "issues" };
    json!({
        "id": id,
        "user": { "login": "hubot" },
        "body": body,
        "html_url": format!("https://github.com/octocat/hello/{kind}/{number}#issuecomment-{id}"),
        "updated_at": updated,
    })
}

/// A page of the counts of the comments of the items of a connection.
fn counted(items: &[(u64, u64)], next: Option<&str>) -> Value {
    json!({
        "page": {
            "totalCount": items.len(),
            "nodes": items
                .iter()
                .map(|(number, comments)| json!({ "number": number, "comments": { "totalCount": comments } }))
                .collect::<Vec<_>>(),
            "pageInfo": { "hasNextPage": next.is_some(), "endCursor": next },
        },
    })
}

/// A page of the numbers of the items of a connection.
fn numbered(numbers: &[u64], next: Option<&str>) -> Value {
    json!({
        "page": {
            "totalCount": numbers.len(),
            "nodes": numbers.iter().map(|number| json!({ "number": number })).collect::<Vec<_>>(),
            "pageInfo": { "hasNextPage": next.is_some(), "endCursor": next },
        },
    })
}

/// Item `number` of the repository with a page of the ids of its comments.
fn ids(number: u64, ids: &[(u64, &str)], next: Option<&str>) -> Value {
    json!({
        "number": number,
        "repository": { "nameWithOwner": "octocat/hello" },
        "comments": {
            "nodes": ids
                .iter()
                .map(|(id, updated_at)| json!({ "databaseId": id, "updatedAt": updated_at }))
                .collect::<Vec<_>>(),
            "pageInfo": { "hasNextPage": next.is_some(), "endCursor": next },
        },
    })
}

/// The answer of GitHub GraphQL for item `number` by its number: of `kind`,
/// `Issue` or `PullRequest`, in `repository` with `answered` as its number.
fn by_number(number: u64, kind: &str, repository: &str, answered: u64, item: &Value) -> Value {
    json!({
        format!("t{number}"): {
            "__typename": kind,
            "number": answered,
            "repository": { "nameWithOwner": repository },
        },
        format!("n{number}"): item,
    })
}

/// The REST count of the comments of the repository, `total`, with its newest
/// comment `newest`, as one row a page.
fn remote(total: u64, newest: Option<u64>) -> Answer {
    let rows = newest.map_or(json!([]), |id| json!([{ "id": id }]));
    if total < 2 {
        return ok(&rows);
    }
    let links = format!(
        "<https://api.github.com/repositories/1/issues/comments?sort=created&direction=desc&per_page=1&page=2>; rel=\"next\", \
         <https://api.github.com/repositories/1/issues/comments?sort=created&direction=desc&per_page=1&page={total}>; rel=\"last\""
    );
    json_answer(200, &[("Link", &links)], &rows)
}

/// The comments of the repository since a second before `time`.
fn comments_since(time: &str) -> String {
    api(&format!("{COMMENTS}&since={time}"))
}

/// The first full listing of the repository: one page of `issues` and of
/// `pulls`, whose cursors are `i-1` and `p-1`, one page of `comments`, the
/// counts of the comments of the nodes, which the comments give, and the REST
/// count of the comments.
async fn first_full_listing(
    harness: &RuntimeHarness,
    issues: &[Value],
    pulls: &[Value],
    comments: &[Value],
) {
    let counts = |nodes: &[Value]| {
        nodes
            .iter()
            .map(|node| {
                (
                    node["number"].as_u64().unwrap(),
                    node["comments"]["totalCount"].as_u64().unwrap(),
                )
            })
            .collect::<Vec<_>>()
    };
    let newest = comments.iter().filter_map(|row| row["id"].as_u64()).max();
    let (listing, _) = answering(
        harness,
        names(harness, &at("issues/all")),
        [
            graphql(&json!({ "issues": rows(issues, issues.len() as u64, Some("i-1"), false) })),
            graphql(&json!({ "pulls": rows(pulls, pulls.len() as u64, Some("p-1"), false) })),
            ok(&Value::Array(comments.to_vec())),
            graphql(&counted(&counts(issues), None)),
            graphql(&counted(&counts(pulls), None)),
            remote(comments.len() as u64, newest),
        ],
    )
    .await;
    listing.unwrap();
}

/// The `POST` of the rows of a check that brings no node, with the counts of
/// the issues and of the pull requests.
fn no_rows(issues: u64, pulls: u64) -> Answer {
    graphql(&json!({
        "issues": rows(&[], issues, None, false),
        "pulls": rows(&[], pulls, None, false),
    }))
}

/// The first full listing takes the issues by their cursor in the order of
/// `updatedAt`, then the pull requests, then every comment by its link
/// `next`, then the counts of the comments: an item with another count than
/// its `comments/` takes its IDs, and a comment that the comments lack comes
/// by its ID before the next page of the IDs; then the REST count of the
/// comments. `item.json` and `item.md` are of the node that the listing took,
/// served without a request, and an author that GitHub deleted is an empty
/// `user` or `author`.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_first_full_listing_takes_the_items_then_the_comments_then_the_counts() {
    let harness = github_harness();
    let mut one = node(1, "One", "OPEN", "2026-11-18T09:00:00Z", 3);
    one["author"] = Value::Null;
    let three = node(3, "Three", "CLOSED", "2026-11-18T08:00:00Z", 0);
    let four = node(4, "Four", "OPEN", "2026-11-18T09:30:00Z", 0);
    let two = node(2, "Two", "OPEN", "2026-11-18T10:00:00Z", 1);
    let mut ten = comment(10, 1, false, "first", "2026-11-18T08:10:00Z");
    ten["user"] = Value::Null;
    let eleven = comment(11, 1, false, "second", "2026-11-18T08:15:00Z");
    let next = r#"<https://api.github.com/repositories/1/issues/comments?page=2>; rel="next""#;
    let (listing, requests) = answering(
        &harness,
        names(&harness, &at("issues/all")),
        [
            graphql(&json!({ "issues": rows(&[three.clone(), one.clone()], 3, Some("i-1"), true) })),
            graphql(&json!({ "issues": rows(&[four], 3, Some("i-2"), false) })),
            graphql(&json!({ "pulls": rows(std::slice::from_ref(&two), 1, Some("p-1"), false) })),
            json_answer(
                200,
                &[("Link", next)],
                &json!([ten, comment(50, 2, true, "on the pull", "2026-11-18T08:20:00Z")]),
            ),
            ok(&json!([comment(12, 1, false, "third", "2026-11-18T08:30:00Z")])),
            graphql(&counted(&[(3, 0), (1, 3), (4, 0)], None)),
            graphql(&json!({ "item": ids(1, &[(10, "2026-11-18T08:10:00Z"), (11, "2026-11-18T08:15:00Z")], Some("c-2")) })),
            ok(&eleven),
            graphql(&json!({ "item": ids(1, &[(12, "2026-11-18T08:30:00Z")], None) })),
            graphql(&counted(&[(2, 1)], None)),
            remote(4, Some(50)),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), ["1", "3", "4"]);
    let (query, variables) = graphql_of(&requests[0]);
    assert!(
        query.contains(
            "issues: issues(first: 100, after: $issues, orderBy: { field: UPDATED_AT, direction: ASC })"
        ),
        "{query}"
    );
    assert_eq!(variables["issues"], Value::Null);
    assert_eq!(graphql_of(&requests[1]).1["issues"], "i-1");
    assert!(graphql_of(&requests[2]).0.contains("pulls: pullRequests("));
    assert_eq!(http(&requests[3]).url, api(COMMENTS));
    assert_eq!(
        http(&requests[4]).url,
        api("/repositories/1/issues/comments?page=2")
    );
    assert!(
        graphql_of(&requests[5])
            .0
            .contains("page: issues(first: 100, after: $after)")
    );
    for (request, after) in [(&requests[6], Value::Null), (&requests[8], json!("c-2"))] {
        let (query, variables) = graphql_of(request);
        assert!(
            query.contains("item: issueOrPullRequest(number: $number)"),
            "{query}"
        );
        assert_eq!(variables["number"], 1);
        assert_eq!(variables["after"], after);
    }
    assert_eq!(
        http(&requests[7]).url,
        api("/repos/octocat/hello/issues/comments/11")
    );
    assert!(graphql_of(&requests[9]).0.contains("page: pullRequests("));
    assert_eq!(http(&requests[10]).url, api(REMOTE));

    for (folder, expected) in [
        ("issues/open", vec!["1", "4"]),
        ("pulls/all", vec!["2"]),
        ("pulls/open", vec!["2"]),
        ("issues/all/1/comments", vec!["10", "11", "12"]),
        ("issues/open/1/comments", vec!["10", "11", "12"]),
        ("issues/all/3/comments", vec![]),
        ("pulls/all/2/comments", vec!["50"]),
    ] {
        assert_eq!(
            served(&harness, names(&harness, &at(folder)))
                .await
                .unwrap(),
            expected,
            "{folder}"
        );
    }
    for (file, expected) in [
        ("issues/all/1/title", "One".to_owned()),
        ("issues/all/1/body", "One body".to_owned()),
        ("issues/all/1/user", String::new()),
        ("issues/all/3/state", "CLOSED".to_owned()),
        ("issues/all/1/item.json", one.to_string()),
        (
            "issues/open/1/item.md",
            "# One\n\n- Number: 1\n- State: OPEN\n- User: \n\nOne body\n".to_owned(),
        ),
        ("pulls/open/2/item.json", two.to_string()),
        ("issues/all/1/comments/10/author", String::new()),
        ("issues/all/1/comments/11/body.md", "second".to_owned()),
    ] {
        assert_eq!(
            served(&harness, read(&harness, &at(file))).await.unwrap(),
            expected,
            "{file}"
        );
    }
    assert!(
        served(&harness, lookup(&harness, &at("issues/open/3")))
            .await
            .unwrap()
            .is_missing()
    );
}

/// The attributes of each file and folder of an issue and of a pull request,
/// as `stat`, `ls -l`, and `find` ask for them, take no request.
#[tokio::test(flavor = "multi_thread")]
async fn the_attributes_of_the_files_of_an_issue_and_of_a_pull_request_take_no_request() {
    let harness = github_harness();
    first_full_listing(
        &harness,
        &[node(1, "One", "OPEN", "2026-11-18T09:00:00Z", 1)],
        &[node(2, "Two", "OPEN", "2026-11-18T10:00:00Z", 0)],
        &[comment(10, 1, false, "first", "2026-11-18T08:10:00Z")],
    )
    .await;
    for folder in ["issues/all/1", "issues/open/1/comments/10", "pulls/all/2"] {
        let entries = served(&harness, names(&harness, &at(folder)))
            .await
            .unwrap();
        assert!(!entries.is_empty(), "{folder}");
        for entry in entries {
            let path = at(&format!("{folder}/{entry}"));
            let found = served(&harness, lookup(&harness, &path)).await.unwrap();
            assert!(!found.is_missing(), "{path}");
            served(&harness, harness.namespace.getattr(found.path))
                .await
                .unwrap();
        }
    }
}

/// A check asks for the nodes of each kind after its cursor and for the REST
/// count of the comments, and for the comments since a second before the last
/// one, so that it takes them whether GitHub's `since` includes that second
/// or not: the rows of that second come once, and the next check asks with
/// the `ETag` of their answer, which `304` keeps.
#[tokio::test(flavor = "multi_thread")]
async fn a_check_asks_after_the_cursors_and_for_the_comments_since_a_second_before_the_last_one() {
    let harness = github_harness();
    let last = comment(10, 1, false, "first", "2026-11-18T09:00:00Z");
    first_full_listing(
        &harness,
        &[node(1, "One", "OPEN", "2026-11-18T09:00:00Z", 1)],
        &[],
        std::slice::from_ref(&last),
    )
    .await;

    harness.begin_sync();
    let (listing, requests) = answering(
        &harness,
        names(&harness, &at("issues/all")),
        [
            no_rows(1, 0),
            remote(1, Some(10)),
            json_answer(200, &[("ETag", r#""comments-1""#)], &json!([last])),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), ["1"]);
    let (query, variables) = graphql_of(&requests[0]);
    assert!(query.contains("issues: issues("), "{query}");
    assert!(query.contains("pulls: pullRequests("), "{query}");
    assert_eq!(variables["issues"], "i-1");
    assert_eq!(variables["pulls"], "p-1");
    assert_eq!(http(&requests[1]).url, api(REMOTE));
    assert_eq!(
        http(&requests[2]).url,
        comments_since("2026-11-18T08:59:59Z")
    );
    assert_eq!(header(&requests[2], "if-none-match"), None);

    harness.begin_sync();
    let (listing, requests) = answering(
        &harness,
        names(&harness, &at("issues/all")),
        [no_rows(1, 0), remote(1, Some(10)), not_modified()],
    )
    .await;
    assert_eq!(listing.unwrap(), ["1"]);
    assert_eq!(
        http(&requests[2]).url,
        comments_since("2026-11-18T08:59:59Z")
    );
    assert_eq!(
        header(&requests[2], "if-none-match"),
        Some(r#""comments-1""#)
    );
}

/// A check takes the changed items, the comments since its time, an edit of
/// one and a new one, and the IDs of each changed item whose comments are
/// other than its count, which remove a deleted comment without a request of
/// a comment it has. A count of the issues other than that of `issues/all/`
/// with the new ones takes the numbers of the issues, which remove the issue
/// that left.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn a_check_takes_the_changed_items_the_comments_since_and_the_ids_of_an_item_with_another_count()
 {
    let harness = github_harness();
    first_full_listing(
        &harness,
        &[
            node(4, "Four", "OPEN", "2026-11-18T08:00:00Z", 0),
            node(1, "One", "OPEN", "2026-11-18T09:00:00Z", 2),
        ],
        &[],
        &[
            comment(10, 1, false, "first", "2026-11-18T08:00:00Z"),
            comment(12, 1, false, "second", "2026-11-18T08:30:00Z"),
        ],
    )
    .await;

    harness.begin_sync();
    let (listing, requests) = answering(
        &harness,
        names(&harness, &at("issues/all")),
        [
            graphql(&json!({
                "issues": rows(
                    &[
                        node(1, "One again", "CLOSED", "2026-11-18T11:00:00Z", 3),
                        node(5, "Five", "OPEN", "2026-11-18T11:30:00Z", 0),
                    ],
                    2,
                    Some("i-2"),
                    false,
                ),
                "pulls": rows(&[], 0, None, false),
            })),
            remote(3, Some(13)),
            ok(&json!([
                comment(12, 1, false, "second, edited", "2026-11-18T10:30:00Z"),
                comment(11, 1, false, "third", "2026-11-18T11:00:00Z"),
                comment(13, 1, false, "fourth", "2026-11-18T11:05:00Z"),
            ])),
            graphql(&json!({
                "i1": ids(
                    1,
                    &[
                        (11, "2026-11-18T11:00:00Z"),
                        (12, "2026-11-18T10:30:00Z"),
                        (13, "2026-11-18T11:05:00Z"),
                    ],
                    None,
                ),
            })),
            graphql(&numbered(&[1, 5], None)),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), ["1", "5"]);
    assert_eq!(
        http(&requests[2]).url,
        comments_since("2026-11-18T08:29:59Z")
    );
    let (query, _) = graphql_of(&requests[3]);
    assert!(
        query.contains("i1: issueOrPullRequest(number: 1)"),
        "{query}"
    );
    assert!(!query.contains("(number: 5)"), "{query}");
    let (query, _) = graphql_of(&requests[4]);
    assert!(
        query.contains("page: issues(first: 100, after: $after) { totalCount nodes { number }"),
        "{query}"
    );

    assert_eq!(
        served(&harness, names(&harness, &at("issues/all/1/comments")))
            .await
            .unwrap(),
        ["11", "12", "13"]
    );
    assert_eq!(
        served(&harness, names(&harness, &at("issues/open")))
            .await
            .unwrap(),
        ["5"],
        "a closed issue leaves issues/open"
    );
    for (file, expected) in [
        ("issues/all/1/title", "One again"),
        ("issues/all/1/comments/11/body.md", "third"),
        ("issues/all/1/comments/12/body.md", "second, edited"),
        ("issues/all/5/title", "Five"),
    ] {
        assert_eq!(
            served(&harness, read(&harness, &at(file))).await.unwrap(),
            expected,
            "{file}"
        );
    }
    for path in [
        "issues/all/4",
        "issues/open/4",
        "issues/open/1",
        "issues/all/1/comments/10",
    ] {
        assert!(
            served(&harness, lookup(&harness, &at(path)))
                .await
                .unwrap()
                .is_missing(),
            "{path}"
        );
    }
}

/// A comment deleted from an item whose node did not change leaves at the
/// next check: the REST count of the comments is less than the count the
/// projection holds, so the check asks for the counts of the comments of the
/// items until each item with another count took its IDs, without a request
/// of a comment it has, and asks for no page of them after that.
#[tokio::test(flavor = "multi_thread")]
async fn a_comment_deleted_from_an_unchanged_item_leaves_at_the_next_check() {
    let harness = github_harness();
    let last = comment(20, 2, true, "on the pull", "2026-11-18T09:00:00Z");
    first_full_listing(
        &harness,
        &[
            node(1, "One", "OPEN", "2026-11-18T08:00:00Z", 2),
            node(3, "Three", "OPEN", "2026-11-18T08:30:00Z", 0),
        ],
        &[node(2, "Two", "OPEN", "2026-11-18T09:00:00Z", 1)],
        &[
            comment(10, 1, false, "first", "2026-11-18T08:00:00Z"),
            comment(12, 1, false, "second", "2026-11-18T08:30:00Z"),
            last.clone(),
        ],
    )
    .await;

    harness.begin_sync();
    let (listing, requests) = answering(
        &harness,
        names(&harness, &at("issues/all")),
        [
            no_rows(2, 1),
            remote(2, Some(20)),
            ok(&json!([last])),
            graphql(&counted(&[(1, 1), (3, 0)], None)),
            graphql(&json!({ "i1": ids(1, &[(12, "2026-11-18T08:30:00Z")], None) })),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), ["1", "3"]);
    assert_eq!(requests.len(), 5);
    assert!(
        graphql_of(&requests[3])
            .0
            .contains("page: issues(first: 100, after: $after) { totalCount nodes { number comments { totalCount } }")
    );
    assert!(
        graphql_of(&requests[4])
            .0
            .contains("i1: issueOrPullRequest(number: 1)")
    );
    assert_eq!(
        served(&harness, names(&harness, &at("issues/all/1/comments")))
            .await
            .unwrap(),
        ["12"]
    );

    harness.begin_sync();
    let (_, requests) = answering(
        &harness,
        names(&harness, &at("issues/all")),
        [no_rows(2, 1), remote(2, Some(20)), ok(&json!([]))],
    )
    .await;
    assert_eq!(requests.len(), 3);
}

/// A comment that comes again in one pass of the comments of a check moved
/// past a page while the pass paged, which shifts the rows after it: the
/// check asks for the comments since its time again, so that no row is lost.
#[tokio::test(flavor = "multi_thread")]
async fn a_comment_that_moved_while_the_comments_paged_asks_for_them_again() {
    let harness = github_harness();
    let last = comment(12, 1, false, "second", "2026-11-18T09:00:00Z");
    first_full_listing(
        &harness,
        &[node(1, "One", "OPEN", "2026-11-18T09:00:00Z", 3)],
        &[],
        &[
            comment(10, 1, false, "first", "2026-11-18T08:00:00Z"),
            comment(11, 1, false, "between", "2026-11-18T08:30:00Z"),
            last.clone(),
        ],
    )
    .await;

    let next = r#"<https://api.github.com/repositories/1/issues/comments?page=2>; rel="next""#;
    harness.begin_sync();
    let (listing, requests) = answering(
        &harness,
        names(&harness, &at("issues/all")),
        [
            no_rows(1, 0),
            remote(3, Some(12)),
            json_answer(
                200,
                &[("Link", next)],
                &json!([
                    last,
                    comment(10, 1, false, "edited", "2026-11-18T10:00:00Z")
                ]),
            ),
            ok(&json!([comment(
                10,
                1,
                false,
                "edited again",
                "2026-11-18T10:05:00Z"
            )])),
            ok(&json!([
                comment(11, 1, false, "between, edited", "2026-11-18T10:01:00Z"),
                comment(10, 1, false, "edited again", "2026-11-18T10:05:00Z"),
            ])),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), ["1"]);
    let since = comments_since("2026-11-18T08:59:59Z");
    assert_eq!(http(&requests[2]).url, since);
    assert_eq!(
        http(&requests[3]).url,
        api("/repositories/1/issues/comments?page=2")
    );
    assert_eq!(http(&requests[4]).url, since);
    for (file, expected) in [("10", "edited again"), ("11", "between, edited")] {
        assert_eq!(
            served(
                &harness,
                read(
                    &harness,
                    &at(&format!("issues/all/1/comments/{file}/body.md"))
                )
            )
            .await
            .unwrap(),
            expected
        );
    }
}

/// A count of the issues other than that of `issues/all/` with the changes
/// of the check takes each page of the numbers of the issues once: an issue
/// that they lack leaves, and each that the projection lacks comes by its
/// number, whatever its `updatedAt`. The next check, with the same count,
/// asks for no numbers.
#[tokio::test(flavor = "multi_thread")]
async fn a_check_takes_the_numbers_of_a_kind_with_another_count_once() {
    let harness = github_harness();
    first_full_listing(
        &harness,
        &[
            node(1, "One", "OPEN", "2026-11-18T08:00:00Z", 0),
            node(3, "Three", "OPEN", "2026-11-18T09:00:00Z", 0),
        ],
        &[],
        &[],
    )
    .await;

    harness.begin_sync();
    let nine = node(9, "Nine", "OPEN", "2026-11-17T08:00:00Z", 0);
    let ten = node(10, "Ten", "CLOSED", "2026-11-17T09:00:00Z", 0);
    let mut found = by_number(9, "Issue", "octocat/hello", 9, &nine);
    for (key, value) in by_number(10, "Issue", "octocat/hello", 10, &ten)
        .as_object()
        .unwrap()
    {
        found[key] = value.clone();
    }
    let (listing, requests) = answering(
        &harness,
        names(&harness, &at("issues/all")),
        [
            no_rows(3, 0),
            remote(0, None),
            json_answer(200, &[("ETag", r#""comments-0""#)], &json!([])),
            graphql(&numbered(&[1, 9], Some("n-1"))),
            graphql(&numbered(&[10], None)),
            graphql(&found),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), ["1", "10", "9"]);
    assert_eq!(http(&requests[2]).url, api(COMMENTS));
    assert_eq!(graphql_of(&requests[4]).1["after"], "n-1");
    let (query, _) = graphql_of(&requests[5]);
    assert!(
        query.contains("t9: issueOrPullRequest(number: 9)"),
        "{query}"
    );
    assert!(
        query.contains("n10: issueOrPullRequest(number: 10)"),
        "{query}"
    );
    assert_eq!(
        served(&harness, read(&harness, &at("issues/all/9/item.json")))
            .await
            .unwrap(),
        nine.to_string()
    );

    harness.begin_sync();
    let (listing, requests) = answering(
        &harness,
        names(&harness, &at("issues/all")),
        [no_rows(3, 0), remote(0, None), not_modified()],
    )
    .await;
    assert_eq!(listing.unwrap(), ["1", "10", "9"]);
    assert_eq!(requests.len(), 3);
}

/// An item that GitHub answers for its number from another repository or
/// with another number, as an issue that moved, left this repository: its
/// facts and its `item.json` are gone, before the end of the first full
/// listing and at the IDs of a check alike, and a `301` of its comments is
/// its removal too.
#[tokio::test(flavor = "multi_thread")]
async fn an_item_that_github_answers_from_elsewhere_left_this_repository() {
    let harness = github_harness();
    let moved = node(12, "Moved", "OPEN", "2026-11-18T09:00:00Z", 0);
    let (found, requests) = answering(
        &harness,
        lookup(&harness, &at("issues/all/7")),
        [graphql(&by_number(
            7,
            "Issue",
            "octocat/elsewhere",
            12,
            &moved,
        ))],
    )
    .await;
    assert!(found.unwrap().is_missing());
    assert!(
        graphql_of(&requests[0])
            .0
            .contains("t7: issueOrPullRequest(number: 7)")
    );
    assert!(
        served(&harness, lookup(&harness, &at("issues/all/7/item.json")))
            .await
            .unwrap()
            .is_missing()
    );

    let (title, _) = answering(
        &harness,
        read(&harness, &at("issues/all/8/title")),
        [graphql(&by_number(
            8,
            "Issue",
            "octocat/hello",
            8,
            &node(8, "Eight", "OPEN", "2026-11-18T09:00:00Z", 1),
        ))],
    )
    .await;
    assert_eq!(title.unwrap(), "Eight");
    let (comments, requests) = answering(
        &harness,
        names(&harness, &at("issues/all/8/comments")),
        [empty(
            301,
            &[(
                "Location",
                "https://api.github.com/repositories/2/issues/3/comments",
            )],
        )],
    )
    .await;
    assert!(comments.is_err(), "{comments:?}");
    assert_eq!(
        http(&requests[0]).url,
        api("/repos/octocat/hello/issues/8/comments?per_page=100")
    );
    assert!(
        served(&harness, lookup(&harness, &at("issues/all/8")))
            .await
            .unwrap()
            .is_missing()
    );

    let harness = github_harness();
    first_full_listing(
        &harness,
        &[node(1, "One", "OPEN", "2026-11-18T09:00:00Z", 0)],
        &[],
        &[],
    )
    .await;
    harness.begin_sync();
    let mut elsewhere = ids(1, &[], None);
    elsewhere["repository"]["nameWithOwner"] = json!("octocat/elsewhere");
    let (listing, _) = answering(
        &harness,
        names(&harness, &at("issues/all")),
        [
            graphql(&json!({
                "issues": rows(&[node(1, "One", "OPEN", "2026-11-18T10:00:00Z", 1)], 1, Some("i-2"), false),
                "pulls": rows(&[], 0, None, false),
            })),
            remote(0, None),
            ok(&json!([])),
            graphql(&json!({ "i1": elsewhere })),
            graphql(&numbered(&[], None)),
        ],
    )
    .await;
    assert!(listing.unwrap().is_empty());
}

/// A repository that GitHub GraphQL names otherwise, as one that moved, is not
/// at its path.
#[tokio::test(flavor = "multi_thread")]
async fn a_repository_that_github_names_otherwise_is_not_at_its_path() {
    let harness = github_harness();
    let (listing, _) = answering(
        &harness,
        names(&harness, &at("issues/all")),
        [graphql_named(
            "octocat/hello-world",
            &json!({ "issues": rows(&[], 0, None, false) }),
        )],
    )
    .await;
    assert_eq!(listing, Err(NsError::NotFound));
}

/// A first full listing that stops at a rate limit gives each read of the
/// four folders the error of the stop while the files it took serve, and the
/// next read after the window continues it from its next page, after the
/// cursor of its last node.
#[tokio::test(flavor = "multi_thread")]
async fn a_first_full_listing_stopped_at_a_rate_limit_continues_from_its_next_page() {
    let harness = github_harness();
    let (stopped, _) = answering(
        &harness,
        names(&harness, &at("issues/all")),
        [
            graphql(&json!({
                "issues": rows(&[node(1, "One", "OPEN", "2026-11-18T09:00:00Z", 0)], 2, Some("i-1"), true),
            })),
            empty(403, &[("x-ratelimit-remaining", "0"), ("retry-after", "2")]),
        ],
    )
    .await;
    assert!(
        matches!(stopped, Err(NsError::RateLimited { .. })),
        "{stopped:?}"
    );
    for folder in ["issues/all", "pulls/open"] {
        let listing = served(&harness, names(&harness, &at(folder))).await;
        assert!(
            matches!(listing, Err(NsError::RateLimited { .. })),
            "{folder}: {listing:?}"
        );
    }
    assert_eq!(
        served(&harness, read(&harness, &at("issues/all/1/title")))
            .await
            .unwrap(),
        "One"
    );

    tokio::time::sleep(Duration::from_millis(2_200)).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, &at("pulls/all")),
        [
            graphql(&json!({
                "issues": rows(&[node(3, "Three", "CLOSED", "2026-11-18T10:00:00Z", 0)], 2, Some("i-2"), false),
            })),
            graphql(&json!({ "pulls": rows(&[], 0, None, false) })),
            ok(&json!([])),
            graphql(&counted(&[(1, 0), (3, 0)], None)),
            graphql(&counted(&[], None)),
            remote(0, None),
        ],
    )
    .await;
    assert!(listing.unwrap().is_empty());
    assert_eq!(graphql_of(&requests[0]).1["issues"], "i-1");
    assert_eq!(
        served(&harness, names(&harness, &at("issues/all")))
            .await
            .unwrap(),
        ["1", "3"]
    );
}

/// A check stopped by the network serves a read what the projection has, as
/// does each read in its window, without a request, and the next read after
/// the window continues it with its next request alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_check_stopped_by_the_network_serves_a_read_what_the_projection_has() {
    let harness = github_harness();
    first_full_listing(
        &harness,
        &[node(1, "One", "OPEN", "2026-11-18T09:00:00Z", 0)],
        &[],
        &[],
    )
    .await;

    harness.begin_sync();
    let (title, requests) = answering(
        &harness,
        read(&harness, &at("issues/all/1/title")),
        [
            graphql(&json!({
                "issues": rows(&[node(1, "One again", "OPEN", "2026-11-18T10:00:00Z", 0)], 1, Some("i-2"), false),
                "pulls": rows(&[], 0, None, false),
            })),
            empty(502, &[]),
        ],
    )
    .await;
    assert_eq!(title.unwrap(), "One");
    assert_eq!(requests.len(), 2);
    assert_eq!(
        served(&harness, read(&harness, &at("issues/all/1/title")))
            .await
            .unwrap(),
        "One"
    );

    harness.begin_sync();
    let (title, requests) = answering(
        &harness,
        read(&harness, &at("issues/all/1/title")),
        [remote(0, None), ok(&json!([]))],
    )
    .await;
    assert_eq!(title.unwrap(), "One again");
    assert_eq!(http(&requests[0]).url, api(REMOTE));
}

/// A `RATE_LIMITED` answer of GitHub GraphQL stops a check with the window
/// of its `x-ratelimit-reset`: its listing serves what the projection has,
/// and the first read after the window continues the check with that `POST`.
#[tokio::test(flavor = "multi_thread")]
async fn a_check_stopped_by_the_rate_limit_of_github_graphql_continues_with_its_post() {
    let harness = github_harness();
    first_full_listing(
        &harness,
        &[node(1, "One", "OPEN", "2026-11-18T09:00:00Z", 0)],
        &[],
        &[],
    )
    .await;

    harness.begin_sync();
    let reset = (SystemTime::now().duration_since(UNIX_EPOCH).unwrap() + Duration::from_secs(2))
        .as_secs()
        .to_string();
    let (stopped, requests) = answering(
        &harness,
        names(&harness, &at("issues/all")),
        [json_answer(
            200,
            &[
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", &reset),
            ],
            &json!({
                "data": null,
                "errors": [{ "type": "RATE_LIMITED", "message": "API rate limit exceeded" }],
            }),
        )],
    )
    .await;
    assert_eq!(stopped.unwrap(), ["1"]);
    assert_eq!(requests.len(), 1);

    tokio::time::sleep(Duration::from_millis(2_200)).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, &at("issues/all")),
        [no_rows(1, 0), remote(0, None), ok(&json!([]))],
    )
    .await;
    assert_eq!(listing.unwrap(), ["1"]);
    assert_eq!(graphql_of(&requests[0]).1["issues"], "i-1");
}

/// Before the first full listing ends, an item takes its own answers: its
/// node by its number, and its `comments/` the pages of its comments by their
/// link `next`; an item of the other kind is not found. A changed answer
/// takes each page of its comments since its last `updatedAt` by their link
/// `next`, then its IDs.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn an_item_before_the_end_of_the_first_full_listing_takes_its_own_answers() {
    let harness = github_harness();
    let (title, requests) = answering(
        &harness,
        read(&harness, &at("issues/all/3/title")),
        [graphql(&by_number(
            3,
            "Issue",
            "octocat/hello",
            3,
            &node(3, "Three", "OPEN", "2026-11-18T09:00:00Z", 2),
        ))],
    )
    .await;
    assert_eq!(title.unwrap(), "Three");
    assert_eq!(requests.len(), 1);
    let (pull, _) = answering(
        &harness,
        lookup(&harness, &at("pulls/all/5")),
        [graphql(&by_number(
            5,
            "Issue",
            "octocat/hello",
            5,
            &node(5, "Five", "OPEN", "2026-11-18T09:00:00Z", 0),
        ))],
    )
    .await;
    assert!(pull.unwrap().is_missing(), "an issue is not a pull request");

    let next = r#"<https://api.github.com/repositories/1/issues/3/comments?page=2>; rel="next""#;
    let (comments, requests) = answering(
        &harness,
        names(&harness, &at("issues/all/3/comments")),
        [
            json_answer(
                200,
                &[("Link", next)],
                &json!([comment(30, 3, false, "first", "2026-11-18T08:00:00Z")]),
            ),
            ok(&json!([comment(
                31,
                3,
                false,
                "second",
                "2026-11-18T08:30:00Z"
            )])),
        ],
    )
    .await;
    assert_eq!(comments.unwrap(), ["30", "31"]);
    assert_eq!(
        http(&requests[0]).url,
        api("/repos/octocat/hello/issues/3/comments?per_page=100")
    );
    assert_eq!(
        http(&requests[1]).url,
        api("/repositories/1/issues/3/comments?page=2")
    );

    harness.begin_sync();
    let next =
        r#"<https://api.github.com/repositories/1/issues/3/comments?since=x&page=2>; rel="next""#;
    let (title, requests) = answering(
        &harness,
        read(&harness, &at("issues/all/3/title")),
        [
            graphql(&by_number(
                3,
                "Issue",
                "octocat/hello",
                3,
                &node(3, "Three again", "CLOSED", "2026-11-18T10:00:00Z", 3),
            )),
            json_answer(
                200,
                &[("Link", next)],
                &json!([comment(
                    31,
                    3,
                    false,
                    "second, edited",
                    "2026-11-18T09:30:00Z"
                )]),
            ),
            ok(&json!([comment(
                32,
                3,
                false,
                "third",
                "2026-11-18T10:00:00Z"
            )])),
            graphql(&json!({
                "item": ids(
                    3,
                    &[
                        (30, "2026-11-18T08:00:00Z"),
                        (31, "2026-11-18T09:30:00Z"),
                        (32, "2026-11-18T10:00:00Z"),
                    ],
                    None,
                ),
            })),
        ],
    )
    .await;
    assert_eq!(title.unwrap(), "Three again");
    assert_eq!(
        http(&requests[1]).url,
        api("/repos/octocat/hello/issues/3/comments?per_page=100&since=2026-11-18T08:59:59Z")
    );
    assert_eq!(
        http(&requests[2]).url,
        api("/repositories/1/issues/3/comments?since=x&page=2")
    );
    assert_eq!(graphql_of(&requests[3]).1["number"], 3);
    assert_eq!(
        served(&harness, names(&harness, &at("issues/all/3/comments")))
            .await
            .unwrap(),
        ["30", "31", "32"]
    );
    assert_eq!(
        served(
            &harness,
            read(&harness, &at("issues/all/3/comments/31/body.md"))
        )
        .await
        .unwrap(),
        "second, edited"
    );
}

/// An issue that a later answer of its own feed closes leaves `open/`, before
/// the end of the first full listing too, whether the answer takes its
/// comments in later parts or not.
#[tokio::test(flavor = "multi_thread")]
async fn an_issue_that_its_own_feed_closes_leaves_open() {
    for comments in [0, 1] {
        let harness = github_harness();
        let (title, _) = answering(
            &harness,
            read(&harness, &at("issues/all/3/title")),
            [graphql(&by_number(
                3,
                "Issue",
                "octocat/hello",
                3,
                &node(3, "Three", "OPEN", "2026-11-18T09:00:00Z", 0),
            ))],
        )
        .await;
        assert_eq!(title.unwrap(), "Three");
        let mut closed = vec![graphql(&by_number(
            3,
            "Issue",
            "octocat/hello",
            3,
            &node(3, "Three", "CLOSED", "2026-11-18T10:00:00Z", comments),
        ))];
        if comments == 1 {
            closed.push(ok(&json!([comment(
                31,
                3,
                false,
                "late",
                "2026-11-18T10:00:00Z"
            )])));
            closed.push(graphql(&json!({
                "item": ids(3, &[(31, "2026-11-18T10:00:00Z")], None),
            })));
        }

        harness.begin_sync();
        let (title, _) =
            answering(&harness, read(&harness, &at("issues/all/3/title")), closed).await;
        assert_eq!(title.unwrap(), "Three");
        for path in ["issues/open/3", "issues/open/3/item.json"] {
            assert!(
                served(&harness, lookup(&harness, &at(path)))
                    .await
                    .unwrap()
                    .is_missing(),
                "{comments}: {path}"
            );
        }
    }
}

/// A later answer of an item stopped by the network at the page of its
/// comments since its time serves the facts from before it, and the next
/// read after the window continues it from that page alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_later_answer_of_an_item_stopped_by_the_network_continues_from_its_next_request() {
    let harness = github_harness();
    let (title, _) = answering(
        &harness,
        read(&harness, &at("issues/all/7/title")),
        [graphql(&by_number(
            7,
            "Issue",
            "octocat/hello",
            7,
            &node(7, "Seven", "OPEN", "2026-11-18T09:00:00Z", 0),
        ))],
    )
    .await;
    assert_eq!(title.unwrap(), "Seven");
    assert!(
        served(&harness, names(&harness, &at("issues/all/7/comments")))
            .await
            .unwrap()
            .is_empty()
    );

    harness.begin_sync();
    let since =
        api("/repos/octocat/hello/issues/7/comments?per_page=100&since=2026-11-18T08:59:59Z");
    let (stopped, requests) = answering(
        &harness,
        read(&harness, &at("issues/all/7/title")),
        [
            graphql(&by_number(
                7,
                "Issue",
                "octocat/hello",
                7,
                &node(7, "Seven again", "OPEN", "2026-11-18T10:00:00Z", 1),
            )),
            empty(502, &[]),
        ],
    )
    .await;
    assert_eq!(stopped.unwrap(), "Seven");
    assert_eq!(http(&requests[1]).url, since);

    harness.begin_sync();
    let (title, requests) = answering(
        &harness,
        read(&harness, &at("issues/all/7/title")),
        [
            ok(&json!([comment(
                70,
                7,
                false,
                "late",
                "2026-11-18T10:00:00Z"
            )])),
            graphql(&json!({ "item": ids(7, &[(70, "2026-11-18T10:00:00Z")], None) })),
        ],
    )
    .await;
    assert_eq!(title.unwrap(), "Seven again");
    assert_eq!(http(&requests[0]).url, since);
}

/// The first full listing removes an item with another count than its
/// `comments/` that GitHub GraphQL cannot resolve at its IDs.
#[tokio::test(flavor = "multi_thread")]
async fn the_first_full_listing_removes_an_item_that_github_graphql_cannot_resolve() {
    let harness = github_harness();
    let (listing, requests) = answering(
        &harness,
        names(&harness, &at("issues/all")),
        [
            graphql(&json!({ "issues": rows(
                &[
                    node(1, "One", "OPEN", "2026-11-18T08:00:00Z", 2),
                    node(2, "Two", "OPEN", "2026-11-18T09:00:00Z", 0),
                ],
                2,
                Some("i-1"),
                false,
            ) })),
            graphql(&json!({ "pulls": rows(&[], 0, None, false) })),
            ok(&json!([comment(
                10,
                1,
                false,
                "first",
                "2026-11-18T08:00:00Z"
            )])),
            graphql(&counted(&[(1, 2), (2, 0)], None)),
            graphql_not_found(&json!({ "item": null })),
            graphql(&counted(&[], None)),
            remote(0, None),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), ["2"]);
    assert_eq!(graphql_of(&requests[4]).1["number"], 1);
    for path in ["issues/all/1", "issues/open/1"] {
        assert!(
            served(&harness, lookup(&harness, &at(path)))
                .await
                .unwrap()
                .is_missing(),
            "{path}"
        );
    }
}

/// A check asks GitHub GraphQL for the IDs of at most 100 changed items with
/// another count of comments a `POST`.
#[tokio::test(flavor = "multi_thread")]
async fn a_check_asks_github_graphql_for_a_hundred_changed_items_a_post() {
    let harness = github_harness();
    first_full_listing(
        &harness,
        &[node(200, "Two hundred", "OPEN", "2026-11-18T09:00:00Z", 0)],
        &[],
        &[],
    )
    .await;

    let changed = (1..=101)
        .map(|number| node(number, "Item", "CLOSED", "2026-11-18T10:00:00Z", 1))
        .collect::<Vec<_>>();
    let page = |numbers: std::ops::RangeInclusive<u64>| {
        Value::Object(
            numbers
                .map(|number| {
                    (
                        format!("i{number}"),
                        ids(number, &[(1_000 + number, "2026-11-18T10:00:00Z")], None),
                    )
                })
                .collect(),
        )
    };
    let fetched = |number: u64| {
        ok(&comment(
            1_000 + number,
            number,
            false,
            "a comment",
            "2026-11-18T10:00:00Z",
        ))
    };
    let mut answers = vec![
        graphql(&json!({
            "issues": rows(&changed[..100], 102, Some("i-2"), true),
            "pulls": rows(&[], 0, None, false),
        })),
        graphql(&json!({ "issues": rows(&changed[100..], 102, Some("i-3"), false) })),
        remote(0, None),
        ok(&json!([])),
        graphql(&page(1..=100)),
    ];
    answers.extend((1..=100).map(fetched));
    answers.push(graphql(&page(101..=101)));
    answers.push(fetched(101));
    harness.begin_sync();
    let (listing, requests) =
        answering(&harness, names(&harness, &at("issues/all")), answers).await;
    assert_eq!(listing.unwrap().len(), 102);
    let (query, _) = graphql_of(&requests[4]);
    assert!(
        query.contains("i100: issueOrPullRequest(number: 100)"),
        "{query}"
    );
    assert!(!query.contains("(number: 101)"), "{query}");
    let (query, _) = graphql_of(&requests[105]);
    assert!(
        query.contains("i101: issueOrPullRequest(number: 101)"),
        "{query}"
    );
}

/// A rate limit of GitHub whose window does not parse fails as an invalid
/// answer, at the REST API and at GitHub GraphQL, as does a primary limit
/// without its reset; one whose reset passed has a window of zero, and a
/// secondary one without `Retry-After` has a window of a minute, whatever the
/// reset of the primary limit.
#[tokio::test(flavor = "multi_thread")]
async fn a_rate_limit_whose_window_does_not_parse_fails_as_an_invalid_answer() {
    let harness = github_harness();
    let rate_limited = |headers: &[(&str, &str)]| {
        json_answer(
            200,
            headers,
            &json!({
                "data": null,
                "errors": [{ "type": "RATE_LIMITED", "message": "API rate limit exceeded" }],
            }),
        )
    };
    let passed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .checked_sub(Duration::from_secs(60))
        .unwrap()
        .as_secs()
        .to_string();
    let later = (SystemTime::now().duration_since(UNIX_EPOCH).unwrap() + Duration::from_mins(50))
        .as_secs()
        .to_string();
    for (answer, expected) in [
        (
            empty(
                403,
                &[("x-ratelimit-remaining", "0"), ("retry-after", "soon")],
            ),
            Err(NsError::Invalid),
        ),
        (
            rate_limited(&[
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", "later"),
            ]),
            Err(NsError::Invalid),
        ),
        (
            rate_limited(&[("x-ratelimit-remaining", "0")]),
            Err(NsError::Invalid),
        ),
        (
            rate_limited(&[
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", &passed),
            ]),
            Err(NsError::RateLimited {
                retry_after: Some(Duration::ZERO),
            }),
        ),
        (
            json_answer(
                403,
                &[
                    ("x-ratelimit-remaining", "4000"),
                    ("x-ratelimit-reset", &later),
                ],
                &json!({ "message": "You have exceeded a secondary rate limit." }),
            ),
            Err(NsError::RateLimited {
                retry_after: Some(Duration::from_secs(60)),
            }),
        ),
    ] {
        let harness = github_harness();
        let (stopped, _) = answering(&harness, names(&harness, &at("issues/all")), [answer]).await;
        assert_eq!(stopped, expected);
    }
    drop(harness);
}

/// The repository under another spelling of its owner is a repository of its
/// own path: its first full listing asks for it as its path spells it, and
/// each spelling serves its own facts.
#[tokio::test(flavor = "multi_thread")]
async fn an_owner_in_another_spelling_names_another_repository() {
    let harness = github_harness();
    for (owner, number) in [("OctoCat", 1), ("octocat", 2)] {
        let (listing, requests) = answering(
            &harness,
            names(&harness, &format!("/github/{owner}/hello/issues/all")),
            [
                graphql_named(
                    &format!("{owner}/hello"),
                    &json!({ "issues": rows(&[node(number, "An item", "OPEN", "2026-11-18T09:00:00Z", 0)], 1, Some("i-1"), false) }),
                ),
                graphql_named(
                    &format!("{owner}/hello"),
                    &json!({ "pulls": rows(&[], 0, None, false) }),
                ),
                ok(&json!([])),
                graphql_named(&format!("{owner}/hello"), &counted(&[(number, 0)], None)),
                graphql_named(&format!("{owner}/hello"), &counted(&[], None)),
                remote(0, None),
            ],
        )
        .await;
        assert_eq!(listing.unwrap(), [number.to_string()]);
        let body: Value =
            serde_json::from_slice(http(&requests[0]).body.as_deref().unwrap()).unwrap();
        assert_eq!(body["variables"]["owner"], owner);
    }
    for (owner, number) in [("OctoCat", "1"), ("octocat", "2")] {
        assert_eq!(
            served(
                &harness,
                names(&harness, &format!("/github/{owner}/hello/issues/all"))
            )
            .await
            .unwrap(),
            [number]
        );
    }
}

/// `open/` of a mount without its provider serves the facts of `all/` from
/// the projection.
#[tokio::test(flavor = "multi_thread")]
async fn open_of_a_mount_without_its_provider_serves_the_facts_of_all() {
    let harness = github_harness();
    first_full_listing(
        &harness,
        &[node(1, "One", "OPEN", "2026-11-18T09:00:00Z", 1)],
        &[],
        &[comment(10, 1, false, "first", "2026-11-18T08:00:00Z")],
    )
    .await;

    let namespace = harness.without_providers().unwrap();
    let path = |path: &'static str| {
        let namespace = Arc::clone(&namespace);
        async move {
            let mut answer = namespace.lookup(Path::root(), "github").await.unwrap();
            for segment in path.split('/') {
                answer = namespace.lookup(answer.path, segment).await.unwrap();
            }
            answer.path
        }
    };
    let title = namespace
        .read(path("octocat/hello/issues/open/1/title").await, 0, 64)
        .await
        .unwrap();
    assert_eq!(title.bytes, b"One");
    let comments = namespace
        .readdir(
            path("octocat/hello/issues/open/1/comments").await,
            DirCursor::start(),
            0,
        )
        .await
        .unwrap();
    assert_eq!(
        comments
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["10"]
    );
}

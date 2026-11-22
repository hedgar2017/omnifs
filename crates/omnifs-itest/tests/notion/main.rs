#![cfg(not(target_os = "wasi"))]

//! The requests of the mount `notion` to Notion: the full listings and the
//! changes of the feed of the workspace, and the pages, data sources,
//! content, and comments of their own feeds.

mod support;

use std::time::Duration;

use omnifs_engine::{LookupAnswer, NsError};
use omnifs_itest::RuntimeHarness;
use omnifs_wit::host::types::CalloutResult;
use serde_json::{Value, json};
use support::{
    EDITED, PAST_DEADLINE, VERSION, answer, answering, comment, data_source, expect, expect_search,
    failure, id, list, lookup, markdown, names, network, notion_harness, notion_id, ok, page,
    rate_limited, read, read_at, row, served, trashed,
};

/// Earlier `last_edited_time`s than `EDITED`, from the latest.
const EARLIER: &str = "2026-11-21T09:00:00.000Z";
const EARLIEST: &str = "2026-11-21T08:00:00.000Z";

/// A later `last_edited_time` than `EDITED`.
const EDITED_LATER: &str = "2026-11-21T10:05:00.000Z";

/// A `Date` five seconds before the end of the minute of `EDITED`.
const WITHIN: &str = "Sat, 21 Nov 2026 10:00:55 GMT";

/// Past the end of the minute of `EDITED` after an answer of the `Date`
/// `WITHIN`, and past the deadline of a feed.
const PAST_WITHIN: Duration = Duration::from_millis(5_200);

/// The `Date` of the end of the minute of `EDITED`.
const AT_END: &str = "Sat, 21 Nov 2026 10:01:00 GMT";

fn names_of(ids: &[u32]) -> Vec<String> {
    let mut names = ids.iter().map(|n| id(*n)).collect::<Vec<_>>();
    names.sort();
    names
}

/// The first full listing of one answer of the search with `results`.
async fn first_full_listing(harness: &RuntimeHarness, results: Vec<Value>) {
    let (listing, requests) = answering(
        harness,
        names(harness, "pages"),
        vec![ok(&list(&results, None))],
    )
    .await;
    listing.unwrap();
    expect_search(&requests[0], 100, None);
}

/// The path of the `GET` of the objects of `folder` by their IDs.
fn request_of(folder: &str) -> &'static str {
    match folder {
        "pages" => "/v1/pages",
        "databases" => "/v1/data_sources",
        _ => panic!("no objects in {folder}"),
    }
}

/// Look up the object `n` of `folder` by its ID, with `answer` to its `GET`.
async fn by_id(
    harness: &RuntimeHarness,
    folder: &str,
    n: u32,
    answer: CalloutResult,
) -> Result<LookupAnswer, NsError> {
    let (found, requests) = answering(
        harness,
        lookup(harness, &format!("{folder}/{}", id(n))),
        vec![answer],
    )
    .await;
    expect(
        &requests[0],
        "GET",
        &format!("{}/{}", request_of(folder), id(n)),
        None,
    );
    found
}

/// Whether the object `n` of `folder` by its ID is found, with `object` as
/// the answer to its `GET`.
async fn found_by_id(harness: &RuntimeHarness, folder: &str, n: u32, object: &Value) -> bool {
    !by_id(harness, folder, n, ok(object))
        .await
        .unwrap()
        .is_missing()
}

/// The path of the request of the comments of the page `n` from `cursor`.
fn comments_of(n: u32, cursor: Option<&str>) -> String {
    let path = format!("/v1/comments?block_id={}&page_size=100", id(n));
    match cursor {
        Some(cursor) => format!("{path}&start_cursor={cursor}"),
        None => path,
    }
}

/// The path of the request of the markdown of a page or a block by its ID.
fn markdown_of(id: &str) -> String {
    format!("/v1/pages/{id}/markdown?markdown_version=v2")
}

/// The first full listing at the first read of `pages/`: the answers of the
/// search by `next_cursor`, with pages, rows, and data sources, taken at each
/// answer; a `429` and a failure of the network in it give each read of
/// `pages/` and `databases/` their error while the files that it took are
/// served, and the next read continues from the next request; then each page
/// and data source of the projection that it lacks by its ID, with the
/// removal for `404` and for `in_trash`, of a result of the search too.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_first_full_listing_takes_the_search_with_the_ids_that_it_lacks() {
    let harness = notion_harness();
    for (folder, n, object) in [
        ("pages", 7, page(7, "Deleted page", EDITED)),
        ("pages", 8, page(8, "Trashed page", EDITED)),
        ("databases", 9, data_source(9, "Unsearched", EDITED)),
        ("pages", 10, page(10, "Trashed in the search", EDITED)),
        ("databases", 11, data_source(11, "Trashed too", EDITED)),
    ] {
        assert!(found_by_id(&harness, folder, n, &object).await, "{n}");
    }

    let first = list(
        &[
            page(1, "Plans", EDITED),
            trashed(page(10, "Trashed in the search", EDITED)),
            row(2, 3, "A task", EARLIER),
            data_source(3, "Tasks", EARLIER),
            trashed(data_source(11, "Trashed too", EARLIER)),
        ],
        Some("cursor-2"),
    );
    let (stopped, requests) = answering(
        &harness,
        names(&harness, "pages"),
        vec![ok(&first), rate_limited()],
    )
    .await;
    assert!(
        matches!(stopped, Err(NsError::RateLimited { .. })),
        "{stopped:?}"
    );
    expect_search(&requests[0], 100, None);
    expect_search(&requests[1], 100, Some("cursor-2"));

    for folder in ["pages", "databases"] {
        let listing = served(&harness, names(&harness, folder)).await;
        assert!(
            matches!(listing, Err(NsError::RateLimited { .. })),
            "{folder}: {listing:?}"
        );
    }
    for (file, expected) in [
        (format!("pages/{}/title", id(1)), "Plans"),
        (format!("pages/{}/title", id(2)), "A task"),
        (format!("databases/{}/title", id(3)), "Tasks"),
    ] {
        assert_eq!(
            served(&harness, read(&harness, &file)).await.unwrap(),
            expected,
            "{file}"
        );
    }

    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let (stopped, requests) =
        answering(&harness, names(&harness, "databases"), vec![network()]).await;
    assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");
    expect_search(&requests[0], 100, Some("cursor-2"));

    let (listing, requests) = answering(
        &harness,
        names(&harness, "pages"),
        vec![
            ok(&list(&[page(4, "Notes", EARLIEST)], None)),
            failure(404, "object_not_found"),
            ok(&trashed(page(8, "Trashed page", EDITED_LATER))),
            ok(&data_source(9, "Unsearched", EDITED)),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), names_of(&[1, 2, 4]));
    expect_search(&requests[0], 100, Some("cursor-2"));
    expect(&requests[1], "GET", &format!("/v1/pages/{}", id(7)), None);
    expect(&requests[2], "GET", &format!("/v1/pages/{}", id(8)), None);
    expect(
        &requests[3],
        "GET",
        &format!("/v1/data_sources/{}", id(9)),
        None,
    );

    assert_eq!(
        served(&harness, names(&harness, "databases"))
            .await
            .unwrap(),
        names_of(&[3, 9])
    );
    assert_eq!(
        served(&harness, names(&harness, &format!("pages/{}", id(1))))
            .await
            .unwrap(),
        ["body.md", "comments", "page.json", "title"]
    );
    let page_json: Value = serde_json::from_str(
        &served(
            &harness,
            read(&harness, &format!("pages/{}/page.json", id(2))),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(page_json, row(2, 3, "A task", EARLIER));
    let schema: Value = serde_json::from_str(
        &served(
            &harness,
            read(&harness, &format!("databases/{}/schema.json", id(9))),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(schema, data_source(9, "Unsearched", EDITED));
    for gone in [7, 8, 10] {
        assert!(
            served(&harness, lookup(&harness, &format!("pages/{}", id(gone))))
                .await
                .unwrap()
                .is_missing(),
            "{gone}"
        );
    }
}

/// A full listing again at a read of `pages/` and of `databases/` past the
/// deadline of its last answer, taken after its last part, with the removal
/// of what it lacks after its `GET`, the stopped content of a removed page
/// too, and from its first request after a `400` to its `start_cursor`.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn a_full_listing_again_past_its_deadline_is_taken_after_its_last_part() {
    let harness = notion_harness();
    first_full_listing(
        &harness,
        vec![
            page(1, "One", EDITED),
            page(2, "Two", EARLIER),
            data_source(3, "Tasks", EARLIER),
        ],
    )
    .await;
    // A content stopped after its first part, of a page that the next full
    // listing removes.
    let (stopped, requests) = answering(
        &harness,
        read(&harness, &format!("pages/{}/body.md", id(2))),
        vec![ok(&markdown("Two body", &[21])), network()],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");
    expect(&requests[0], "GET", &markdown_of(&id(2)), None);
    expect(&requests[1], "GET", &markdown_of(&id(21)), None);

    tokio::time::sleep(PAST_DEADLINE).await;
    let (stopped, requests) = answering(
        &harness,
        names(&harness, "pages"),
        vec![
            ok(&list(
                &[page(1, "One again", EDITED_LATER)],
                Some("cursor-2"),
            )),
            rate_limited(),
        ],
    )
    .await;
    assert!(
        matches!(stopped, Err(NsError::RateLimited { .. })),
        "{stopped:?}"
    );
    expect_search(&requests[0], 100, None);
    expect_search(&requests[1], 100, Some("cursor-2"));
    // The kept part waits for the last part of its answer.
    assert_eq!(
        served(&harness, read(&harness, &format!("pages/{}/title", id(1))))
            .await
            .unwrap(),
        "One"
    );
    assert_eq!(
        served(&harness, names(&harness, "pages")).await.unwrap(),
        names_of(&[1, 2])
    );

    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, "pages"),
        vec![
            ok(&list(&[data_source(3, "Tasks", EARLIER)], None)),
            failure(404, "object_not_found"),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), names_of(&[1]));
    expect_search(&requests[0], 100, Some("cursor-2"));
    expect(&requests[1], "GET", &format!("/v1/pages/{}", id(2)), None);
    assert_eq!(
        served(&harness, read(&harness, &format!("pages/{}/title", id(1))))
            .await
            .unwrap(),
        "One again"
    );
    // The removal leaves nothing of the stopped content of the page.
    assert!(
        served(&harness, lookup(&harness, &format!("pages/{}", id(2))))
            .await
            .unwrap()
            .is_missing()
    );

    // A later full listing whose cursor Notion refuses begins again from its
    // first request.
    tokio::time::sleep(PAST_DEADLINE).await;
    let (stopped, requests) = answering(
        &harness,
        names(&harness, "databases"),
        vec![
            ok(&list(
                &[page(2, "Two again", EDITED_LATER)],
                Some("cursor-2"),
            )),
            failure(400, "validation_error"),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Invalid)), "{stopped:?}");
    expect_search(&requests[0], 100, None);
    expect_search(&requests[1], 100, Some("cursor-2"));
    let (listing, requests) = answering(
        &harness,
        names(&harness, "databases"),
        vec![
            ok(&list(
                &[
                    page(2, "Two again", EDITED_LATER),
                    page(1, "One again", EDITED_LATER),
                ],
                None,
            )),
            failure(404, "object_not_found"),
        ],
    )
    .await;
    assert!(listing.unwrap().is_empty());
    expect_search(&requests[0], 100, None);
    expect(
        &requests[1],
        "GET",
        &format!("/v1/data_sources/{}", id(3)),
        None,
    );
    // The page that a full listing removed and a later one brings again
    // takes its content from its first request.
    let (body, requests) = answering(
        &harness,
        read(&harness, &format!("pages/{}/body.md", id(2))),
        vec![ok(&markdown("Two again body", &[]))],
    )
    .await;
    assert_eq!(body.unwrap(), "Two again body");
    expect(&requests[0], "GET", &markdown_of(&id(2)), None);
}

/// The changes at a read of a file of a page past the deadline of the last
/// answer: the `page_size` of the count of the latest time and one more, then
/// `100` by `next_cursor`, until an older result, from their next request
/// after a failure; a page with another `last_edited_time` takes its `body.md`
/// and its `comments/` once more at their next first read, and a page in the
/// trash is removed.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_changes_take_the_search_until_an_older_result() {
    let harness = notion_harness();
    first_full_listing(
        &harness,
        vec![
            page(1, "One", EDITED),
            page(2, "Two", EDITED),
            page(3, "Three", EARLIER),
        ],
    )
    .await;
    let (body, requests) = answering(
        &harness,
        read(&harness, &format!("pages/{}/body.md", id(1))),
        vec![ok(&markdown("One body", &[]))],
    )
    .await;
    assert_eq!(body.unwrap(), "One body");
    expect(&requests[0], "GET", &markdown_of(&id(1)), None);
    let (comments, requests) = answering(
        &harness,
        names(&harness, &format!("pages/{}/comments", id(1))),
        vec![ok(&list(&[comment(11, Some("Ada"), "Hi")], None))],
    )
    .await;
    assert_eq!(comments.unwrap(), names_of(&[11]));
    expect(&requests[0], "GET", &comments_of(1, None), None);
    // A content and comments stopped after their first parts: each read takes
    // what it asks for, from the next request of its own.
    let body = format!("pages/{}/body.md", id(3));
    let comments = format!("pages/{}/comments", id(3));
    let (stopped, requests) = answering(
        &harness,
        read(&harness, &body),
        vec![ok(&markdown("Three body", &[31])), network()],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");
    expect(&requests[0], "GET", &markdown_of(&id(3)), None);
    expect(&requests[1], "GET", &markdown_of(&id(31)), None);
    let (stopped, requests) = answering(
        &harness,
        names(&harness, &comments),
        vec![
            ok(&list(
                &[comment(33, Some("Ada"), "First")],
                Some("cursor-2"),
            )),
            network(),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");
    expect(&requests[0], "GET", &comments_of(3, None), None);
    expect(&requests[1], "GET", &comments_of(3, Some("cursor-2")), None);
    let (read_body, requests) = answering(
        &harness,
        read(&harness, &body),
        vec![ok(&markdown("Block 31", &[]))],
    )
    .await;
    assert_eq!(read_body.unwrap(), "Three body\n\nBlock 31");
    expect(&requests[0], "GET", &markdown_of(&id(31)), None);
    let (listing, requests) = answering(
        &harness,
        names(&harness, &comments),
        vec![ok(&list(&[comment(35, Some("Grace"), "Second")], None))],
    )
    .await;
    assert_eq!(listing.unwrap(), names_of(&[33, 35]));
    expect(&requests[0], "GET", &comments_of(3, Some("cursor-2")), None);

    // A content stopped after its first part keeps that part until the page
    // changes.
    let (stopped, requests) = answering(
        &harness,
        read(&harness, &format!("pages/{}/body.md", id(2))),
        vec![ok(&markdown("Two body", &[21])), rate_limited()],
    )
    .await;
    assert!(
        matches!(stopped, Err(NsError::RateLimited { .. })),
        "{stopped:?}"
    );
    expect(&requests[0], "GET", &markdown_of(&id(2)), None);
    expect(&requests[1], "GET", &markdown_of(&id(21)), None);

    tokio::time::sleep(PAST_DEADLINE).await;
    let (stopped, requests) = answering(
        &harness,
        read(&harness, &format!("pages/{}/title", id(2))),
        vec![
            ok(&list(
                &[
                    page(1, "One edited", EDITED_LATER),
                    page(2, "Two edited", EDITED_LATER),
                    page(4, "Four", EDITED),
                ],
                Some("cursor-2"),
            )),
            network(),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");
    expect_search(&requests[0], 3, None);
    expect_search(&requests[1], 100, Some("cursor-2"));
    let (title, requests) = answering(
        &harness,
        read(&harness, &format!("pages/{}/title", id(2))),
        vec![ok(&list(
            &[page(5, "Five", EDITED), page(3, "Three", EARLIER)],
            Some("cursor-3"),
        ))],
    )
    .await;
    assert_eq!(title.unwrap(), "Two edited");
    expect_search(&requests[0], 100, Some("cursor-2"));
    assert_eq!(
        served(&harness, names(&harness, "pages")).await.unwrap(),
        names_of(&[1, 2, 3, 4, 5])
    );
    assert_eq!(
        served(&harness, read(&harness, &format!("pages/{}/title", id(1))))
            .await
            .unwrap(),
        "One edited"
    );

    let (body, requests) = answering(
        &harness,
        read(&harness, &format!("pages/{}/body.md", id(1))),
        vec![ok(&markdown("One body edited", &[]))],
    )
    .await;
    assert_eq!(body.unwrap(), "One body edited");
    expect(&requests[0], "GET", &markdown_of(&id(1)), None);
    let (comments, requests) = answering(
        &harness,
        names(&harness, &format!("pages/{}/comments", id(1))),
        vec![ok(&list(&[], None))],
    )
    .await;
    assert!(comments.unwrap().is_empty());
    expect(&requests[0], "GET", &comments_of(1, None), None);
    // The change drops the part of the content stopped before it.
    let (body, requests) = answering(
        &harness,
        read(&harness, &format!("pages/{}/body.md", id(2))),
        vec![ok(&markdown("Two body edited", &[]))],
    )
    .await;
    assert_eq!(body.unwrap(), "Two body edited");
    expect(&requests[0], "GET", &markdown_of(&id(2)), None);

    // The next changes start from the latest time of these, with its count.
    tokio::time::sleep(PAST_DEADLINE).await;
    let (_, requests) = answering(
        &harness,
        read(&harness, &format!("pages/{}/title", id(2))),
        vec![ok(&list(
            &[
                trashed(page(5, "Five", EDITED_LATER)),
                page(1, "One edited", EDITED_LATER),
                page(2, "Two edited", EDITED_LATER),
            ],
            None,
        ))],
    )
    .await;
    expect_search(&requests[0], 3, None);
    assert_eq!(
        served(&harness, names(&harness, "pages")).await.unwrap(),
        names_of(&[1, 2, 3, 4])
    );
}

/// An object by its ID of 32 lowercase hexadecimal digits, a page or a data
/// source: its `GET` at its first lookup without a listing, again at a read
/// past its deadline before the end of the first full listing, none after it,
/// but for one that the projection lacks at its lookup past its deadline, as
/// after a `404` or in the trash; its folder in its listing. A result of the
/// search with an earlier `last_edited_time` leaves it.
async fn an_object_by_its_id(folder: &str, object: fn(u32, &str, &str) -> Value) {
    let harness = notion_harness();
    let at = |n: u32| format!("{folder}/{}", id(n));
    let get = |n: u32| format!("{}/{}", request_of(folder), id(n));
    assert!(found_by_id(&harness, folder, 1, &object(1, "First", EDITED)).await);
    assert_eq!(
        served(&harness, read(&harness, &format!("{}/title", at(1))))
            .await
            .unwrap(),
        "First"
    );

    tokio::time::sleep(PAST_DEADLINE).await;
    let (title, requests) = answering(
        &harness,
        read(&harness, &format!("{}/title", at(1))),
        vec![ok(&object(1, "First renamed", EDITED_LATER))],
    )
    .await;
    assert_eq!(title.unwrap(), "First renamed");
    expect(&requests[0], "GET", &get(1), None);

    // A result of the search with an earlier `last_edited_time` than the
    // object of the projection leaves it.
    let (listing, requests) = answering(
        &harness,
        names(&harness, folder),
        vec![ok(&list(
            &[object(1, "First", EDITED), object(2, "Second", EDITED)],
            None,
        ))],
    )
    .await;
    assert_eq!(listing.unwrap(), names_of(&[1, 2]));
    expect_search(&requests[0], 100, None);
    assert_eq!(
        served(&harness, read(&harness, &format!("{}/title", at(1))))
            .await
            .unwrap(),
        "First renamed"
    );
    // An ID with its hyphens or with uppercase digits names no folder.
    for name in [notion_id(1), id(0xabc).to_uppercase()] {
        assert!(
            served(&harness, lookup(&harness, &format!("{folder}/{name}")))
                .await
                .unwrap()
                .is_missing(),
            "{name}"
        );
    }

    // After the end of the first full listing the changes answer for it.
    tokio::time::sleep(PAST_DEADLINE).await;
    let (title, requests) = answering(
        &harness,
        read(&harness, &format!("{}/title", at(1))),
        vec![ok(&list(
            &[
                object(1, "First renamed", EDITED_LATER),
                object(2, "Second", EDITED),
            ],
            None,
        ))],
    )
    .await;
    assert_eq!(title.unwrap(), "First renamed");
    expect_search(&requests[0], 3, None);

    // One that the projection lacks, at its lookup past its deadline.
    let found = by_id(&harness, folder, 5, failure(404, "object_not_found")).await;
    assert!(found.unwrap().is_missing());
    let in_trash = trashed(object(6, "Sixth", EDITED));
    assert!(!found_by_id(&harness, folder, 6, &in_trash).await);
    assert!(
        served(&harness, lookup(&harness, &at(5)))
            .await
            .unwrap()
            .is_missing()
    );
    tokio::time::sleep(PAST_DEADLINE).await;
    let (found, requests) = answering(
        &harness,
        lookup(&harness, &at(5)),
        vec![
            ok(&list(
                &[
                    object(1, "First renamed", EDITED_LATER),
                    object(2, "Second", EDITED),
                ],
                None,
            )),
            ok(&object(5, "Fifth", EDITED)),
        ],
    )
    .await;
    assert!(!found.unwrap().is_missing());
    expect_search(&requests[0], 2, None);
    expect(&requests[1], "GET", &get(5), None);
    assert_eq!(
        served(&harness, names(&harness, folder)).await.unwrap(),
        names_of(&[1, 2, 5])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_by_its_id() {
    an_object_by_its_id("pages", page).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_data_source_by_its_id() {
    an_object_by_its_id("databases", data_source).await;
}

/// The content at the first read of `body.md`, by a lookup and by its path
/// alone: the markdown of the page and of its unknown blocks in turn, each
/// once, a block with `404` without its markdown, and the removal of the page for its
/// `404`; a content that Notion may have read within the minute of its
/// `last_edited_time`, by its `Date` less the time of its request, taken once
/// more at the first read after the end of that minute.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_content_takes_the_markdown_of_the_page_and_of_its_unknown_blocks() {
    let harness = notion_harness();
    for n in [1, 2, 3, 4, 5, 6] {
        assert!(found_by_id(&harness, "pages", n, &page(n, &format!("Page {n}"), EDITED)).await);
    }

    let (body, requests) = answering(
        &harness,
        read(&harness, &format!("pages/{}/body.md", id(1))),
        vec![
            ok(&markdown("# Page 1", &[21, 22])),
            ok(&markdown("Block 21", &[23, 21, 22])),
            failure(404, "object_not_found"),
            ok(&markdown("Block 23", &[])),
        ],
    )
    .await;
    assert_eq!(body.unwrap(), "# Page 1\n\nBlock 21\n\nBlock 23");
    expect(&requests[0], "GET", &markdown_of(&id(1)), None);
    for (request, n) in requests[1..].iter().zip([21, 22, 23]) {
        expect(request, "GET", &markdown_of(&id(n)), None);
    }
    assert_eq!(
        served(
            &harness,
            read(&harness, &format!("pages/{}/body.md", id(1)))
        )
        .await
        .unwrap(),
        "# Page 1\n\nBlock 21\n\nBlock 23"
    );

    let (body, requests) = answering(
        &harness,
        read_at(&harness, &format!("pages/{}/body.md", id(2))),
        vec![ok(&markdown("Page 2 body", &[]))],
    )
    .await;
    assert_eq!(body.unwrap(), "Page 2 body");
    expect(&requests[0], "GET", &markdown_of(&id(2)), None);

    let (body, requests) = answering(
        &harness,
        read(&harness, &format!("pages/{}/body.md", id(3))),
        vec![failure(404, "object_not_found")],
    )
    .await;
    assert!(matches!(body, Err(NsError::NotFound)), "{body:?}");
    expect(&requests[0], "GET", &markdown_of(&id(3)), None);
    assert!(
        served(&harness, lookup(&harness, &format!("pages/{}", id(3))))
            .await
            .unwrap()
            .is_missing()
    );

    // An answer without a `Date` is the error of the read.
    let (body, requests) = answering(
        &harness,
        read(&harness, &format!("pages/{}/body.md", id(5))),
        vec![answer(200, &[], &markdown("Page 5 body", &[]))],
    )
    .await;
    assert!(matches!(body, Err(NsError::Invalid)), "{body:?}");
    expect(&requests[0], "GET", &markdown_of(&id(5)), None);

    // A content of a `Date` at the end of the minute of its
    // `last_edited_time`, which Notion may have read within that minute, is
    // taken once more at the next read, here the read after its lookup.
    let (body, requests) = answering(
        &harness,
        read(&harness, &format!("pages/{}/body.md", id(6))),
        vec![
            answer(200, &[("Date", AT_END)], &markdown("Page 6 body", &[])),
            ok(&markdown("Page 6 body, edited", &[])),
        ],
    )
    .await;
    assert_eq!(body.unwrap(), "Page 6 body, edited");
    for request in &requests {
        expect(request, "GET", &markdown_of(&id(6)), None);
    }
    assert_eq!(
        served(
            &harness,
            read(&harness, &format!("pages/{}/body.md", id(6)))
        )
        .await
        .unwrap(),
        "Page 6 body, edited"
    );

    let (body, requests) = answering(
        &harness,
        read(&harness, &format!("pages/{}/body.md", id(4))),
        vec![answer(
            200,
            &[("Date", WITHIN)],
            &markdown("Page 4 body", &[]),
        )],
    )
    .await;
    assert_eq!(body.unwrap(), "Page 4 body");
    expect(&requests[0], "GET", &markdown_of(&id(4)), None);
    assert_eq!(
        served(
            &harness,
            read(&harness, &format!("pages/{}/body.md", id(4)))
        )
        .await
        .unwrap(),
        "Page 4 body"
    );
    tokio::time::sleep(PAST_WITHIN).await;
    let (body, requests) = answering(
        &harness,
        read(&harness, &format!("pages/{}/body.md", id(4))),
        vec![
            ok(&page(4, "Page 4", EDITED)),
            ok(&markdown("Page 4 body, edited in its minute", &[])),
        ],
    )
    .await;
    assert_eq!(body.unwrap(), "Page 4 body, edited in its minute");
    expect(&requests[0], "GET", &format!("/v1/pages/{}", id(4)), None);
    expect(&requests[1], "GET", &markdown_of(&id(4)), None);
    assert_eq!(
        served(
            &harness,
            read(&harness, &format!("pages/{}/body.md", id(4)))
        )
        .await
        .unwrap(),
        "Page 4 body, edited in its minute"
    );

    // Past its deadline, a read by its path alone of the content of a page
    // with another `last_edited_time` takes the page and then its content.
    let (body, requests) = answering(
        &harness,
        read_at(&harness, &format!("pages/{}/body.md", id(2))),
        vec![
            ok(&page(2, "Page 2", EDITED_LATER)),
            ok(&markdown("Page 2 body, edited", &[])),
        ],
    )
    .await;
    assert_eq!(body.unwrap(), "Page 2 body, edited");
    expect(&requests[0], "GET", &format!("/v1/pages/{}", id(2)), None);
    expect(&requests[1], "GET", &markdown_of(&id(2)), None);

    // A result of the search with an earlier `last_edited_time` than the page
    // of the projection leaves its content.
    first_full_listing(
        &harness,
        vec![
            page(1, "Page 1", EDITED),
            page(2, "Page 2", EDITED),
            page(4, "Page 4", EDITED),
            page(5, "Page 5", EDITED),
            page(6, "Page 6", EDITED),
        ],
    )
    .await;
    assert_eq!(
        served(
            &harness,
            read(&harness, &format!("pages/{}/body.md", id(2)))
        )
        .await
        .unwrap(),
        "Page 2 body, edited"
    );
}

/// The comments at the first read of `comments/` and of a file of a comment
/// by its path alone: their answers by `next_cursor`, the files of each
/// comment, an empty `author` for `resolved_name` `null`, the removal of the
/// page for `404`, and the comments once more after the end of the minute of
/// the `last_edited_time` of the page, as the content.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_comments_take_their_answers_by_next_cursor() {
    let harness = notion_harness();
    for n in [1, 2, 3, 4, 5, 6, 7] {
        assert!(found_by_id(&harness, "pages", n, &page(n, &format!("Page {n}"), EDITED)).await);
    }

    let (comments, requests) = answering(
        &harness,
        names(&harness, &format!("pages/{}/comments", id(1))),
        vec![
            ok(&list(
                &[
                    comment(11, Some("Ada"), "First"),
                    comment(13, None, "Second"),
                ],
                Some("cursor-2"),
            )),
            ok(&list(&[comment(15, Some("Grace"), "Third")], None)),
        ],
    )
    .await;
    assert_eq!(comments.unwrap(), names_of(&[11, 13, 15]));
    expect(&requests[0], "GET", &comments_of(1, None), None);
    expect(&requests[1], "GET", &comments_of(1, Some("cursor-2")), None);
    let at = format!("pages/{}/comments/{}", id(1), id(13));
    assert_eq!(
        served(&harness, names(&harness, &at)).await.unwrap(),
        ["author", "body.md", "comment.json"]
    );
    for (file, expected) in [("author", ""), ("body.md", "Second")] {
        assert_eq!(
            served(&harness, read(&harness, &format!("{at}/{file}")))
                .await
                .unwrap(),
            expected,
            "{file}"
        );
    }
    let object: Value = serde_json::from_str(
        &served(&harness, read(&harness, &format!("{at}/comment.json")))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(object, comment(13, None, "Second"));
    assert_eq!(
        served(
            &harness,
            read(
                &harness,
                &format!("pages/{}/comments/{}/author", id(1), id(15))
            )
        )
        .await
        .unwrap(),
        "Grace"
    );
    // A lookup of a comment that the complete `comments/` lacks asks nothing.
    for _ in 0..2 {
        let absent = format!("pages/{}/comments/{}", id(1), id(17));
        assert!(
            served(&harness, lookup(&harness, &absent))
                .await
                .unwrap()
                .is_missing()
        );
    }

    let (body, requests) = answering(
        &harness,
        read_at(
            &harness,
            &format!("pages/{}/comments/{}/body.md", id(2), id(21)),
        ),
        vec![ok(&list(&[comment(21, Some("Linus"), "Hello")], None))],
    )
    .await;
    assert_eq!(body.unwrap(), "Hello");
    expect(&requests[0], "GET", &comments_of(2, None), None);

    let (comments, requests) = answering(
        &harness,
        names(&harness, &format!("pages/{}/comments", id(3))),
        vec![failure(404, "object_not_found")],
    )
    .await;
    assert!(matches!(comments, Err(NsError::NotFound)), "{comments:?}");
    expect(&requests[0], "GET", &comments_of(3, None), None);
    assert!(
        served(&harness, lookup(&harness, &format!("pages/{}", id(3))))
            .await
            .unwrap()
            .is_missing()
    );

    // The comments that Notion restricts for the connection are no comments,
    // and the page stays.
    let (comments, requests) = answering(
        &harness,
        names(&harness, &format!("pages/{}/comments", id(5))),
        vec![failure(403, "restricted_resource")],
    )
    .await;
    assert_eq!(comments.unwrap(), Vec::<String>::new());
    expect(&requests[0], "GET", &comments_of(5, None), None);
    assert!(
        !served(&harness, lookup(&harness, &format!("pages/{}", id(5))))
            .await
            .unwrap()
            .is_missing()
    );

    // Another refusal of the comments is the error of the read.
    let (comments, requests) = answering(
        &harness,
        names(&harness, &format!("pages/{}/comments", id(6))),
        vec![failure(403, "unauthorized")],
    )
    .await;
    assert!(matches!(comments, Err(NsError::Permission)), "{comments:?}");
    expect(&requests[0], "GET", &comments_of(6, None), None);

    for n in [4, 7] {
        let (comments, requests) = answering(
            &harness,
            names(&harness, &format!("pages/{}/comments", id(n))),
            vec![answer(
                200,
                &[("Date", WITHIN)],
                &list(&[comment(41, Some("Ada"), "Early")], None),
            )],
        )
        .await;
        assert_eq!(comments.unwrap(), names_of(&[41]), "{n}");
        expect(&requests[0], "GET", &comments_of(n, None), None);
    }
    tokio::time::sleep(PAST_WITHIN).await;
    // The comments that Notion restricts once more are no comments.
    let (comments, requests) = answering(
        &harness,
        names(&harness, &format!("pages/{}/comments", id(7))),
        vec![
            ok(&page(7, "Page 7", EDITED)),
            failure(403, "restricted_resource"),
        ],
    )
    .await;
    assert_eq!(comments.unwrap(), Vec::<String>::new());
    expect(&requests[0], "GET", &format!("/v1/pages/{}", id(7)), None);
    expect(&requests[1], "GET", &comments_of(7, None), None);
    let (comments, requests) = answering(
        &harness,
        names(&harness, &format!("pages/{}/comments", id(4))),
        vec![
            ok(&page(4, "Page 4", EDITED)),
            ok(&list(
                &[
                    comment(41, Some("Ada"), "Early"),
                    comment(43, Some("Grace"), "In the same minute"),
                ],
                None,
            )),
        ],
    )
    .await;
    assert_eq!(comments.unwrap(), names_of(&[41, 43]));
    expect(&requests[0], "GET", &format!("/v1/pages/{}", id(4)), None);
    expect(&requests[1], "GET", &comments_of(4, None), None);

    // Past its deadline, a read by its path alone of a comment of a page with
    // another `last_edited_time` takes the page and then its comments.
    let (body, requests) = answering(
        &harness,
        read_at(
            &harness,
            &format!("pages/{}/comments/{}/body.md", id(1), id(13)),
        ),
        vec![
            ok(&page(1, "Page 1", EDITED_LATER)),
            ok(&list(&[comment(13, None, "Second, edited")], None)),
        ],
    )
    .await;
    assert_eq!(body.unwrap(), "Second, edited");
    expect(&requests[0], "GET", &format!("/v1/pages/{}", id(1)), None);
    expect(&requests[1], "GET", &comments_of(1, None), None);
    assert_eq!(
        served(
            &harness,
            names(&harness, &format!("pages/{}/comments", id(1)))
        )
        .await
        .unwrap(),
        names_of(&[13])
    );
}

/// A `529` gives a read the error of a rate limit and Notion no request in
/// the seconds of its `Retry-After`; a `429` of a blocked connection gives a
/// read its error at once and asks again at the next read.
#[tokio::test(flavor = "multi_thread")]
async fn a_529_waits_its_retry_after_and_a_blocked_connection_does_not() {
    let harness = notion_harness();
    let found = by_id(
        &harness,
        "pages",
        1,
        answer(
            529,
            &[("Retry-After", "1")],
            &json!({ "object": "error", "status": 529, "code": "service_overload", "message": "overloaded" }),
        ),
    )
    .await;
    assert!(
        matches!(found, Err(NsError::RateLimited { .. })),
        "{found:?}"
    );
    let found = served(&harness, lookup(&harness, &format!("pages/{}", id(1)))).await;
    assert!(
        matches!(found, Err(NsError::RateLimited { .. })),
        "{found:?}"
    );
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert!(found_by_id(&harness, "pages", 1, &page(1, "Page 1", EDITED)).await);

    let found = by_id(
        &harness,
        "pages",
        2,
        answer(
            429,
            &[("Retry-After", "60")],
            &json!({
                "object": "error",
                "status": 429,
                "code": "rate_limited",
                "message": "This connection's API access has been restricted.",
                "additional_data": { "rate_limit_reason": "public_api_request_blocked" },
            }),
        ),
    )
    .await;
    assert!(matches!(found, Err(NsError::Permission)), "{found:?}");
    assert!(found_by_id(&harness, "pages", 2, &page(2, "Page 2", EDITED)).await);
}

/// A `400` to a request with a `start_cursor` of a full listing, of the
/// changes, and of the comments, and a `404` or a `restricted_resource` to one
/// of the comments, gives its read its error, and the next read begins from the first request, a read of
/// `pages/` after that of the changes with a full listing.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn a_refused_cursor_begins_again_from_the_first_request() {
    let harness = notion_harness();
    let (listing, requests) = answering(
        &harness,
        names(&harness, "pages"),
        vec![
            ok(&list(&[page(1, "One", EDITED)], Some("cursor-2"))),
            failure(400, "validation_error"),
        ],
    )
    .await;
    assert!(matches!(listing, Err(NsError::Invalid)), "{listing:?}");
    expect_search(&requests[0], 100, None);
    expect_search(&requests[1], 100, Some("cursor-2"));
    let (listing, requests) = answering(
        &harness,
        names(&harness, "pages"),
        vec![ok(&list(
            &[
                page(1, "One", EDITED),
                page(2, "Two", EDITED),
                page(3, "Three", EDITED),
                page(4, "Four", EDITED),
            ],
            None,
        ))],
    )
    .await;
    let all = [
        page(1, "One", EDITED),
        page(2, "Two", EDITED),
        page(3, "Three", EDITED),
        page(4, "Four", EDITED),
    ];
    assert_eq!(listing.unwrap(), names_of(&[1, 2, 3, 4]));
    expect_search(&requests[0], 100, None);

    let title = format!("pages/{}/title", id(1));
    let mut changed = vec![page(5, "Five", EDITED)];
    changed.extend(all.clone());
    for after in ["the changes", "a full listing"] {
        tokio::time::sleep(PAST_DEADLINE).await;
        let (stopped, requests) = answering(
            &harness,
            read(&harness, &title),
            vec![
                ok(&list(&changed, Some("cursor-3"))),
                failure(400, "validation_error"),
            ],
        )
        .await;
        assert!(
            matches!(stopped, Err(NsError::Invalid)),
            "{after}: {stopped:?}"
        );
        expect_search(&requests[0], 5, None);
        expect_search(&requests[1], 100, Some("cursor-3"));
        if after == "the changes" {
            let (read_title, requests) = answering(
                &harness,
                read(&harness, &title),
                vec![ok(&list(&all, None))],
            )
            .await;
            assert_eq!(read_title.unwrap(), "One");
            expect_search(&requests[0], 5, None);
        } else {
            // A read of `pages/` after the refusal takes a full listing.
            let (listing, requests) = answering(
                &harness,
                names(&harness, "pages"),
                vec![ok(&list(&all[..3], None)), failure(404, "object_not_found")],
            )
            .await;
            assert_eq!(listing.unwrap(), names_of(&[1, 2, 3]));
            expect_search(&requests[0], 100, None);
            expect(&requests[1], "GET", &format!("/v1/pages/{}", id(4)), None);
        }
    }

    // A `404` and a `restricted_resource` to a `start_cursor` of the comments
    // refuse the cursor too, and the page stays.
    for (n, refusal) in [
        (1, failure(400, "validation_error")),
        (2, failure(404, "object_not_found")),
        (3, failure(403, "restricted_resource")),
    ] {
        let comments = format!("pages/{}/comments", id(n));
        let (stopped, requests) = answering(
            &harness,
            names(&harness, &comments),
            vec![
                ok(&list(
                    &[comment(11, Some("Ada"), "First")],
                    Some("cursor-2"),
                )),
                refusal,
            ],
        )
        .await;
        assert!(
            matches!(
                stopped,
                Err(NsError::Invalid | NsError::NotFound | NsError::Permission)
            ),
            "{n}: {stopped:?}"
        );
        expect(&requests[0], "GET", &comments_of(n, None), None);
        expect(&requests[1], "GET", &comments_of(n, Some("cursor-2")), None);
        let (listing, requests) = answering(
            &harness,
            names(&harness, &comments),
            vec![ok(&list(&[comment(11, Some("Ada"), "First")], None))],
        )
        .await;
        assert_eq!(listing.unwrap(), names_of(&[11]), "{n}");
        expect(&requests[0], "GET", &comments_of(n, None), None);
    }
}

/// The first request of the changes has the `page_size` `1` without a latest
/// time, and at most `100`.
#[tokio::test(flavor = "multi_thread")]
async fn the_first_request_of_the_changes_has_a_page_size_from_1_to_100() {
    let harness = notion_harness();
    first_full_listing(&harness, Vec::new()).await;
    let pages = (1..=100)
        .map(|n| page(n, "Page", EDITED))
        .collect::<Vec<_>>();

    tokio::time::sleep(PAST_DEADLINE).await;
    let (found, requests) = answering(
        &harness,
        lookup(&harness, &format!("pages/{}", id(1))),
        vec![
            ok(&list(&pages[..1], Some("cursor-2"))),
            ok(&list(&pages[1..], None)),
        ],
    )
    .await;
    assert!(!found.unwrap().is_missing());
    expect_search(&requests[0], 1, None);
    expect_search(&requests[1], 100, Some("cursor-2"));

    tokio::time::sleep(PAST_DEADLINE).await;
    let (title, requests) = answering(
        &harness,
        read(&harness, &format!("pages/{}/title", id(1))),
        vec![ok(&list(&pages, None))],
    )
    .await;
    assert_eq!(title.unwrap(), "Page");
    expect_search(&requests[0], 100, None);
}

/// The validation of the token of the manifest sends the `Notion-Version` of
/// the requests.
#[test]
fn the_validation_of_the_token_has_the_version_of_the_requests() {
    let file = "omnifs_provider_notion.wasm";
    let wasm = std::fs::read(omnifs_itest::provider_wasm_path(file)).unwrap();
    let (_, manifest) = omnifs_provider::Artifact::from_bytes_with_manifest(file, wasm).unwrap();
    let manifest = serde_json::to_value(manifest).unwrap();
    assert_eq!(
        manifest.pointer("/auth/schemes/0/staticToken/validation/headers/Notion-Version"),
        Some(&json!(VERSION))
    );
}

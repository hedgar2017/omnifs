//! Namespace-visible pagination behavior.

use omnifs_core::path::Path;
use omnifs_engine::{DirCursor, Namespace};
use omnifs_itest::{RuntimeHarness, make_initialized_runtime};

const CONFIG: &str = r#"{"provider":"test_provider.wasm","mount":"test"}"#;

async fn hello_child(harness: &RuntimeHarness, name: &str) -> omnifs_engine::LookupAnswer {
    let mount = harness
        .namespace
        .lookup(Path::root(), "test")
        .await
        .unwrap();
    let hello = harness.namespace.lookup(mount.path, "hello").await.unwrap();
    harness.namespace.lookup(hello.path, name).await.unwrap()
}

async fn feed(harness: &RuntimeHarness) -> omnifs_engine::LookupAnswer {
    hello_child(harness, "feed").await
}

/// The provider entries of a directory, every page followed to its end.
async fn walk(harness: &RuntimeHarness, dir: &omnifs_engine::LookupAnswer) -> Vec<String> {
    let mut names = Vec::new();
    let mut cursor = DirCursor::start();
    loop {
        let page = harness
            .namespace
            .readdir(dir.path.clone(), cursor, 0)
            .await
            .unwrap();
        names.extend(
            page.entries
                .into_iter()
                .filter(|entry| !entry.name.starts_with('@'))
                .map(|entry| entry.name),
        );
        let Some(next) = page.next else {
            return names;
        };
        cursor = next;
    }
}

async fn validated_calls(harness: &RuntimeHarness) -> u64 {
    let calls = hello_child(harness, "validated-calls").await;
    let read = harness.namespace.read(calls.path, 0, 64).await.unwrap();
    String::from_utf8(read.bytes)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn first_page_carries_cursor_and_manual_cursor_expands_to_completion() {
    let harness = make_initialized_runtime(CONFIG);
    let feed = feed(&harness).await;
    let first = harness
        .namespace
        .readdir(feed.path.clone(), DirCursor::start(), 0)
        .await
        .unwrap();
    assert!(first.next.is_some());
    assert!(first.entries.iter().any(|entry| entry.name == "@next"));

    let mut cursor = first.next.expect("first page cursor");
    let mut names: Vec<_> = first
        .entries
        .into_iter()
        .filter(|entry| !entry.name.starts_with('@'))
        .map(|entry| entry.name)
        .collect();
    loop {
        let page = harness
            .namespace
            .readdir(feed.path.clone(), cursor, 0)
            .await
            .unwrap();
        names.extend(
            page.entries
                .into_iter()
                .filter(|entry| !entry.name.starts_with('@'))
                .map(|entry| entry.name),
        );
        let Some(next) = page.next else { break };
        cursor = next;
    }
    for name in ["item-0", "item-1", "item-2", "item-3", "item-4", "item-5"] {
        assert!(names.iter().any(|candidate| candidate == name));
    }
    assert_eq!(names.len(), 6);
    let sibling = harness.namespace.lookup(feed.path, "item-0").await.unwrap();
    assert_eq!(
        sibling.attrs().unwrap().kind,
        omnifs_engine::EntryKind::Directory
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pagination_preserves_siblings_and_non_paged_directories_have_no_next() {
    let harness = make_initialized_runtime(CONFIG);
    let feed = feed(&harness).await;
    let page = harness
        .namespace
        .readdir(feed.path.clone(), DirCursor::start(), 0)
        .await
        .unwrap();
    assert!(page.entries.iter().any(|entry| entry.name == "item-0"));
    assert!(page.entries.iter().any(|entry| entry.name == "@next"));
    let next = page.next.expect("first page cursor");
    let page = harness
        .namespace
        .readdir(feed.path.clone(), next, 0)
        .await
        .unwrap();
    let next = page.next.expect("second page cursor");
    harness
        .namespace
        .readdir(feed.path.clone(), next, 0)
        .await
        .unwrap();
    let sibling = harness
        .namespace
        .lookup(feed.path.clone(), "item-0")
        .await
        .unwrap();
    assert_eq!(
        sibling.attrs().unwrap().kind,
        omnifs_engine::EntryKind::Directory
    );

    let hello = harness
        .namespace
        .lookup(Path::root(), "test")
        .await
        .unwrap();
    let hello = harness.namespace.lookup(hello.path, "hello").await.unwrap();
    let listing = harness
        .namespace
        .readdir(hello.path, DirCursor::start(), 0)
        .await
        .unwrap();
    assert!(listing.next.is_none());
}

/// A listing of pages with validators serves from cache within the deadline of
/// its pages and asks for each page again with its validator in a sync: an
/// `unchanged` page stands to the next sync, a changed first page replaces
/// its entries and keeps the later page while its cursor stays, and drops it
/// for another cursor. A listing of pages without a validator keeps the
/// freshness it has, served from cache in those syncs.
#[tokio::test(flavor = "multi_thread")]
async fn pages_with_validators_are_asked_again_once_in_each_sync() {
    // The pages of `hello/validated` state no interval: each sync asks for
    // them again.

    let harness = make_initialized_runtime(CONFIG);
    let feed = feed(&harness).await;
    let feed_names = walk(&harness, &feed).await;
    let validated = hello_child(&harness, "validated").await;
    assert_eq!(walk(&harness, &validated).await, ["v-0", "v-1", "v-2"]);
    assert_eq!(validated_calls(&harness).await, 2);

    assert_eq!(walk(&harness, &validated).await, ["v-0", "v-1", "v-2"]);
    assert_eq!(validated_calls(&harness).await, 2, "served outside a sync");

    harness.begin_sync();
    assert_eq!(walk(&harness, &validated).await, ["v-0", "v-1", "v-2"]);
    assert_eq!(
        validated_calls(&harness).await,
        4,
        "each page is asked for with its validator once in a sync"
    );
    assert_eq!(walk(&harness, &validated).await, ["v-0", "v-1", "v-2"]);
    assert_eq!(
        validated_calls(&harness).await,
        4,
        "an unchanged page stands to the next sync"
    );

    let sync = harness.begin_sync();
    assert_eq!(walk(&harness, &validated).await, ["v-0", "v-1b", "v-2"]);
    assert_eq!(validated_calls(&harness).await, 6);

    // Outside a sync the listings are served from the projection, without
    // the provider.
    harness.end_sync(sync);
    harness.runtime.shutdown().unwrap();
    assert_eq!(walk(&harness, &feed).await, feed_names);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_page_without_its_next_cursor_drops_the_later_pages() {
    // The third revalidation of the pages of `hello/validated` answers a
    // complete first page.

    let harness = make_initialized_runtime(CONFIG);
    let validated = hello_child(&harness, "validated").await;
    assert_eq!(walk(&harness, &validated).await, ["v-0", "v-1", "v-2"]);
    for names in [["v-0", "v-1", "v-2"], ["v-0", "v-1b", "v-2"]] {
        harness.begin_sync();
        assert_eq!(walk(&harness, &validated).await, names);
    }

    harness.begin_sync();
    assert_eq!(walk(&harness, &validated).await, ["v-0"]);
    assert_eq!(
        validated_calls(&harness).await,
        7,
        "a complete first page drops the later page, which no call asks for"
    );
}

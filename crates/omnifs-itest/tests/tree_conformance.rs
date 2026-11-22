//! Public namespace conformance for the in-tree test provider.

#![cfg(not(target_os = "wasi"))]

use omnifs_engine::{DirCursor, Namespace};
use omnifs_itest::RuntimeHarness;
use omnifs_itest::support::{lookup, names};

#[tokio::test(flavor = "multi_thread")]
async fn lists_root_and_nested_directories() {
    let harness = RuntimeHarness::new(omnifs_itest::TEST_PROVIDER_CONFIG).unwrap();
    let root = names(&harness, "/test").await.unwrap();
    for expected in ["README.md", "items", "hello", "scoped", "dynamic"] {
        assert!(root.iter().any(|name| name == expected));
    }
    let hello = names(&harness, "/test/hello").await.unwrap();
    for expected in ["message", "greeting", "ranged", "bundle", "feed"] {
        assert!(hello.iter().any(|name| name == expected));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_readmes_are_visible_and_hidden_by_root_ignore_patterns() {
    let harness = RuntimeHarness::new(omnifs_itest::TEST_PROVIDER_CONFIG).unwrap();
    let root = lookup(&harness, "/test/README.md").await.unwrap();
    let root_bytes = harness
        .namespace
        .read(root.path, 0, u32::MAX)
        .await
        .unwrap()
        .bytes;
    let root_text = String::from_utf8(root_bytes).unwrap();
    assert!(root_text.contains("The keying schema is the path grammar below."));
    let ignore = lookup(&harness, "/test/.gitignore").await.unwrap();
    assert_eq!(ignore.attrs().unwrap().kind, omnifs_engine::EntryKind::File);
    let bytes = harness
        .namespace
        .read(ignore.path, 0, u32::MAX)
        .await
        .unwrap()
        .bytes;
    assert_eq!(bytes, b"@next\n@all\n/README.md\n/*/README.md\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn lazy_derived_face_returns_the_declared_leaf_bytes() {
    let harness = RuntimeHarness::new(omnifs_itest::TEST_PROVIDER_CONFIG).unwrap();
    let item = lookup(&harness, "/test/items/open/7").await.unwrap();
    harness
        .namespace
        .readdir(item.path, DirCursor::start(), 0)
        .await
        .unwrap();
    assert_eq!(
        harness
            .namespace
            .read(
                lookup(&harness, "/test/items/open/7/state")
                    .await
                    .unwrap()
                    .path,
                0,
                u32::MAX
            )
            .await
            .unwrap()
            .bytes,
        b"open"
    );
    assert_eq!(
        harness
            .namespace
            .read(
                lookup(&harness, "/test/items/open/7/title")
                    .await
                    .unwrap()
                    .path,
                0,
                u32::MAX
            )
            .await
            .unwrap()
            .bytes,
        b"Item 7"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn lists_cursored_pages() {
    let harness = RuntimeHarness::new(omnifs_itest::TEST_PROVIDER_CONFIG).unwrap();
    let feed = lookup(&harness, "/test/hello/feed").await.unwrap();
    let first = harness
        .namespace
        .readdir(feed.path.clone(), DirCursor::start(), 0)
        .await
        .unwrap();
    let warm = harness
        .namespace
        .readdir(feed.path.clone(), DirCursor::start(), 0)
        .await
        .unwrap();
    assert_eq!(
        warm.entries, first.entries,
        "warm listing preserves entries and controls"
    );
    assert_eq!(
        warm.next, first.next,
        "warm listing preserves the resume cursor"
    );
    let second = harness
        .namespace
        .readdir(feed.path, first.next.unwrap(), 0)
        .await
        .unwrap();
    assert!(second.entries.iter().any(|entry| entry.name == "item-2"));
    assert!(second.entries.iter().any(|entry| entry.name == "item-3"));
}

//! Read-policy regressions through the public `EngineNamespace` owner.

#![cfg(not(target_os = "wasi"))]

use omnifs_core::path::Path;
use omnifs_engine::{DirCursor, LookupAnswer, Namespace};
use omnifs_itest::make_runtime;

fn path(value: &str) -> Path {
    Path::parse(value).expect("valid test path")
}

async fn resolve(namespace: &dyn Namespace, value: &str) -> LookupAnswer {
    let attrs = namespace.getattr(Path::root()).await.unwrap();
    let mut answer = LookupAnswer::found(Path::root(), attrs);
    for segment in path(value).segments() {
        let parent = answer.path.clone();
        answer = namespace
            .lookup(parent.clone(), segment)
            .await
            .unwrap_or_else(|error| {
                panic!("lookup {segment:?} under {parent} while resolving {value}: {error:?}")
            });
    }
    answer
}

#[tokio::test(flavor = "multi_thread")]
async fn read_whole_file_second_read_hits_cache() {
    let harness = make_runtime();
    let namespace = harness.namespace.as_ref();
    let node = resolve(namespace, "/test/hello/lazy").await;
    assert_eq!(
        namespace
            .read(node.path.clone(), 0, u32::MAX)
            .await
            .unwrap()
            .bytes,
        b"lazy\n"
    );

    harness.runtime.shutdown().unwrap();
    let second = namespace
        .read(node.path.clone(), 0, u32::MAX)
        .await
        .unwrap();
    assert_eq!(second.bytes, b"lazy\n");
    assert!(second.eof);
}

#[tokio::test(flavor = "multi_thread")]
async fn preloaded_empty_file_returns_eof_without_host_callout() {
    let harness = make_runtime();
    let namespace = harness.namespace.as_ref();
    let hello = resolve(namespace, "/test/hello").await;
    namespace
        .readdir(hello.path, DirCursor::start(), 0)
        .await
        .expect("hello listing should publish the preloaded bundle shape");
    let node = resolve(namespace, "/test/hello/bundle/empty").await;
    let answer = namespace
        .read(node.path.clone(), 0, u32::MAX)
        .await
        .unwrap();
    assert!(answer.bytes.is_empty());
    assert!(answer.eof);
    assert!(harness.runtime.try_recv_test_callout().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn read_item_md_returns_stable_representation() {
    let harness = make_runtime();
    let namespace = harness.namespace.as_ref();
    namespace
        .readdir(path("/test/items/open"), DirCursor::start(), 0)
        .await
        .unwrap();
    let item = resolve(namespace, "/test/items/open/7/item.md").await;
    let read = namespace.read(item.path, 0, u32::MAX).await.unwrap();
    assert_eq!(read.bytes, b"# Item 7\n\nBody 7\n");
    assert_eq!(read.attrs.stability, omnifs_engine::Stability::Stable);
}

#[tokio::test(flavor = "multi_thread")]
async fn canonical_read_repeats_with_stable_bytes() {
    let harness = make_runtime();
    let namespace = harness.namespace.as_ref();
    let item = resolve(namespace, "/test/items/all/7/item.json").await;
    let first = namespace
        .read(item.path.clone(), 0, u32::MAX)
        .await
        .unwrap();
    harness.runtime.shutdown().unwrap();
    let second = namespace.read(item.path, 0, u32::MAX).await.unwrap();
    assert_eq!(first.bytes, second.bytes);
    assert_eq!(first.attrs.size, second.attrs.size);
}

/// Concurrent reads of two representations of one object share the load of
/// the object past its deadline, and each takes the bytes of its own path.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_reads_of_two_representations_take_the_bytes_of_their_own_paths() {
    let harness = make_runtime();
    let namespace = harness.namespace.as_ref();
    let markdown = resolve(namespace, "/test/memos/7/memo.md").await;
    let json = resolve(namespace, "/test/memos/7/memo.json").await;
    let read = |node: &LookupAnswer| {
        let path = node.path.clone();
        async move { namespace.read(path, 0, u32::MAX).await.unwrap().bytes }
    };
    assert_eq!(read(&markdown).await, b"# Memo 7\n");
    assert_eq!(read(&json).await, br#"{"memo":7}"#);
    for round in 0..3 {
        // Past the deadline of the object, each read asks the provider: the
        // second joins the flight of the first.
        tokio::time::sleep(std::time::Duration::from_millis(3_200)).await;
        let (first, second) = tokio::join!(read(&markdown), read(&json));
        assert_eq!(first, b"# Memo 7\n", "round {round}");
        assert_eq!(second, br#"{"memo":7}"#, "round {round}");
    }
}

/// Run `read` of a file of the test provider that fetches a blob, and answer
/// the fetch with `body` as its upstream. Returns the read and the request
/// of the fetch.
async fn read_with_blob<T>(
    harness: &omnifs_itest::RuntimeHarness,
    read: impl Future<Output = T>,
    body: &[u8],
) -> (T, omnifs_wit::host::types::BlobFetchRequest) {
    let answer = async {
        let callout = loop {
            match harness.runtime.try_recv_test_callout() {
                Some(callout) => break callout,
                None => tokio::time::sleep(std::time::Duration::from_millis(2)).await,
            }
        };
        let omnifs_wit::host::types::Callout::FetchBlob(request) = callout.callout().clone() else {
            panic!("expected a blob fetch, got {:?}", callout.callout());
        };
        harness.runtime.answer_test_blob(callout, 200, body).await;
        request
    };
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        tokio::join!(read, answer)
    })
    .await
    .expect("the read fetches one blob")
}

/// A file of a blob reads by ranges of its body in the body store, at any
/// size: no read has the whole of it in memory, and none is refused for
/// the bytes that the host takes in memory.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_of_a_blob_reads_by_ranges_above_the_bytes_of_the_host_in_memory() {
    const ABOVE: u64 = 64 * 1024 * 1024 + 5;
    let harness = make_runtime();
    let namespace = harness.namespace.as_ref();
    let mut body = vec![0_u8; usize::try_from(ABOVE).unwrap()];
    body[usize::try_from(ABOVE).unwrap() - 5..].copy_from_slice(b"tail!");
    let (tail, _) = read_with_blob(
        &harness,
        namespace.read(path("/test/hello/blob"), ABOVE - 5, 4096),
        &body,
    )
    .await;
    let tail = tail.expect("the tail of a blob above the bytes of the host in memory");
    assert_eq!(tail.bytes, b"tail!");
    assert!(tail.eof);

    // Each later read is a range of the body that the projection has,
    // without the provider.
    harness.runtime.shutdown().unwrap();
    let head = namespace
        .read(path("/test/hello/blob"), 0, 4)
        .await
        .unwrap();
    assert_eq!((head.bytes.as_slice(), head.eof), (&[0_u8; 4][..], false));
    let end = namespace
        .read(path("/test/hello/blob"), ABOVE - 2, 4096)
        .await
        .unwrap();
    assert_eq!((end.bytes.as_slice(), end.eof), (&b"l!"[..], true));
}

/// The answer of a blob fetch is of its request: the projection keeps it
/// from the end of the fetch, for a read that fails after it and for a read
/// whose answer the host refuses.
#[tokio::test(flavor = "multi_thread")]
async fn the_answer_of_a_blob_fetch_stays_whatever_its_read_does_after() {
    for file in [
        "/test/hello/blob-then-error",
        "/test/hello/blob-of-another-size",
    ] {
        let harness = make_runtime();
        let (read, request) = read_with_blob(
            &harness,
            harness.namespace.read(path(file), 0, 4096),
            b"the bytes of a blob",
        )
        .await;
        assert!(read.is_err(), "{file}: {read:?}");
        assert!(harness.runtime.keeps_test_blob(&request), "{file}");
    }

    // The read that takes its blob as its file keeps the answer too.
    let harness = make_runtime();
    let (read, request) = read_with_blob(
        &harness,
        harness.namespace.read(path("/test/hello/blob"), 0, 4096),
        b"the bytes of a blob",
    )
    .await;
    assert_eq!(read.unwrap().bytes, b"the bytes of a blob");
    assert!(harness.runtime.keeps_test_blob(&request));
}

/// The test harness answers a blob fetch as the host does an answer of its
/// upstream: bytes of another MD5 than the request asks for are the error of
/// the fetch, and the bytes of that MD5 are its file.
#[tokio::test(flavor = "multi_thread")]
async fn the_test_answer_of_a_blob_has_the_md5_that_its_request_asks_for() {
    let harness = make_runtime();
    let file = || {
        harness
            .namespace
            .read(path("/test/hello/blob-of-md5"), 0, 64)
    };
    let (read, request) = read_with_blob(&harness, file(), b"other").await;
    assert!(read.is_err(), "{read:?}");
    assert!(!harness.runtime.keeps_test_blob(&request));
    let (read, _) = read_with_blob(&harness, file(), b"hello").await;
    assert_eq!(read.unwrap().bytes, b"hello");
}

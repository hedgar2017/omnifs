//! Live-file growth through the production `EngineNamespace` surface.

#![cfg(not(target_os = "wasi"))]

use omnifs_core::path::Path;
use omnifs_engine::{EntryKind, LookupAnswer, Namespace, ReadStyle};
use omnifs_itest::RuntimeHarness;

async fn resolve(harness: &RuntimeHarness, value: &str) -> LookupAnswer {
    let namespace = harness.namespace.as_ref();
    let attrs = namespace.getattr(Path::root()).await.unwrap();
    let mut answer = LookupAnswer::found(Path::root(), attrs);
    for segment in Path::parse(value).unwrap().segments() {
        answer = namespace.lookup(answer.path, segment).await.unwrap();
    }
    answer
}

/// Nothing follows a live file: a read learns its growth. The first read
/// ends at the first extent; the next read past it, after the log grew,
/// takes the appended bytes, and no event announces them meanwhile.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_of_a_live_file_takes_the_bytes_that_it_grew_by() {
    let harness = RuntimeHarness::new(omnifs_itest::TEST_PROVIDER_CONFIG).unwrap();
    let namespace = harness.namespace.as_ref();
    let node = resolve(&harness, "/test/hello/live-log").await;
    assert_eq!(node.attrs().unwrap().kind, EntryKind::File);
    assert_eq!(node.attrs().unwrap().read_style, ReadStyle::Ranged);
    let mut events = namespace.subscribe();

    let first = namespace.read(node.path.clone(), 0, 128).await.unwrap();
    assert_eq!(first.bytes, b"line 000000\n");
    assert!(first.eof, "the first observed live extent is a bounded EOF");

    let second = namespace.read(node.path.clone(), 12, 128).await.unwrap();
    assert_eq!(second.bytes, b"line 000001\n");
    assert!(second.attrs.size >= 24, "the read learned the grown extent");
    assert!(
        events.try_recv().is_none(),
        "no event follows the live file"
    );
}

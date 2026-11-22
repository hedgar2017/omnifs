//! The calls of the provider behind the attributes of a fed directory: a
//! lookup or a getattr of a listed fed directory, and a lookup of a child
//! that the projection has, ask the provider for no listing.
//!
//! The test counts the provider operations by their spans, with a global
//! subscriber, so it is alone in its file.

use std::io;
use std::sync::{Arc, Mutex};

use omnifs_core::path::Path;
use omnifs_engine::{DirCursor, Namespace};
use omnifs_itest::make_initialized_runtime;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::FmtSpan;

const CONFIG: &str = r#"{"provider":"test_provider.wasm","mount":"test"}"#;

#[derive(Clone, Default)]
struct Spans(Arc<Mutex<Vec<u8>>>);

impl Spans {
    /// The count of the provider operations `method` so far.
    fn of(&self, method: &str) -> usize {
        let spans = self.0.lock().unwrap();
        String::from_utf8_lossy(&spans)
            .lines()
            .filter(|line| {
                line.contains("provider.operation")
                    && line.contains(&format!("method=\"{method}\""))
            })
            .count()
    }
}

impl io::Write for Spans {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Spans {
    type Writer = Spans;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_attributes_of_a_listed_fed_directory_ask_the_provider_for_no_listing() {
    let spans = Spans::default();
    tracing_subscriber::fmt()
        .with_writer(spans.clone())
        .with_span_events(FmtSpan::NEW)
        .with_ansi(false)
        .init();
    let harness = make_initialized_runtime(CONFIG);
    let namespace = harness.namespace.as_ref();
    let mut books = Path::root();
    for segment in ["test", "shelves", "oak", "books"] {
        books = namespace.lookup(books, segment).await.unwrap().path;
    }
    let shelf = Path::parse("/test/shelves/oak").unwrap();
    namespace
        .readdir(books.clone(), DirCursor::start(), 0)
        .await
        .unwrap();
    let listings = spans.of("list_children");
    assert!(
        listings > 0,
        "the spans of the provider operations are seen"
    );

    for _ in 0..5 {
        namespace.lookup(shelf.clone(), "books").await.unwrap();
        namespace.getattr(books.clone()).await.unwrap();
        namespace.lookup(books.clone(), "1").await.unwrap();
    }
    assert_eq!(spans.of("list_children"), listings);
}

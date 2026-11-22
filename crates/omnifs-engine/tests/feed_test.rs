//! Namespace-visible object feeds: the test provider's shelf feed answers for
//! the books of a shelf, the book feed for a title before the first answer of
//! its shelf ends.

use std::time::Duration;

use omnifs_core::path::Path;
use omnifs_engine::{DirCursor, Namespace, NsError};
use omnifs_itest::{RuntimeHarness, make_initialized_runtime};

const CONFIG: &str = r#"{"provider":"test_provider.wasm","mount":"test"}"#;

/// Past the deadline of a feed, `DYNAMIC_TTL_MILLIS` after its last answer.
const PAST_DEADLINE: Duration = Duration::from_millis(3_200);

async fn path(harness: &RuntimeHarness, segments: &[&str]) -> Path {
    let mut path = harness
        .namespace
        .lookup(Path::root(), "test")
        .await
        .unwrap()
        .path;
    for segment in segments {
        path = harness.namespace.lookup(path, segment).await.unwrap().path;
    }
    path
}

async fn names(harness: &RuntimeHarness, segments: &[&str]) -> Result<Vec<String>, NsError> {
    let dir = path(harness, segments).await;
    let page = harness
        .namespace
        .readdir(dir, DirCursor::start(), 0)
        .await?;
    Ok(page.entries.into_iter().map(|entry| entry.name).collect())
}

async fn read(harness: &RuntimeHarness, segments: &[&str]) -> String {
    let file = path(harness, segments).await;
    let read = harness.namespace.read(file, 0, u32::MAX).await.unwrap();
    String::from_utf8(read.bytes).unwrap()
}

/// The calls of the feeds of the test provider so far. A read gives the
/// version of the log whose size the lookup before it told, so the second
/// read gives the log as the first one took it.
async fn log(harness: &RuntimeHarness) -> Vec<String> {
    read(harness, &["hello", "shelf-log"]).await;
    read(harness, &["hello", "shelf-log"])
        .await
        .lines()
        .map(ToOwned::to_owned)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_feed_starts_from_its_starting_path_and_answers_once_past_its_deadline() {
    let harness = make_initialized_runtime(CONFIG);

    // The first listing of `books` starts the shelf feed, whose first
    // answer publishes its three parts before the listing serves.
    assert_eq!(
        names(&harness, &["shelves", "oak", "books"]).await.unwrap(),
        ["1", "2"]
    );
    assert_eq!(
        log(&harness).await,
        ["oak - books", "oak page-1 books", "oak page-2 books"]
    );

    // Fed facts answer filesystems with the zero lifetime of dynamic entries.
    let shelf = path(&harness, &["shelves", "oak"]).await;
    for (parent, name) in [
        (shelf.clone(), "books"),
        (path(&harness, &["shelves", "oak", "books"]).await, "1"),
        (
            path(&harness, &["shelves", "oak", "books", "1"]).await,
            "title",
        ),
    ] {
        let lookup = harness.namespace.lookup(parent, name).await.unwrap();
        assert_eq!(lookup.attrs().unwrap().ttl, Duration::ZERO, "{name}");
    }
    assert_eq!(
        read(&harness, &["shelves", "oak", "books", "1", "title"]).await,
        "Book 1"
    );
    // A fed anchor lists its faces.
    assert_eq!(
        names(&harness, &["shelves", "oak", "books", "1"])
            .await
            .unwrap(),
        ["book.json", "title"]
    );

    // Within its deadline the feed is not asked again; past it, an answer
    // with its token stands.
    tokio::time::sleep(PAST_DEADLINE).await;
    assert_eq!(
        names(&harness, &["shelves", "oak", "books"]).await.unwrap(),
        ["1", "2"]
    );
    assert_eq!(
        log(&harness).await[3..],
        ["oak version-0 ~books"],
        "one answer past the deadline, from the token of the first answer"
    );

    // An upstream change is taken by the next answer past the deadline: the
    // host gave the feed its listing of `books`, and the part renames, adds,
    // and removes books.
    read(&harness, &["hello", "shelf-change"]).await;
    assert_eq!(
        names(&harness, &["shelves", "oak", "books"]).await.unwrap(),
        ["1", "2"]
    );
    tokio::time::sleep(PAST_DEADLINE).await;
    assert_eq!(
        names(&harness, &["shelves", "oak", "books"]).await.unwrap(),
        ["1", "3"]
    );
    assert_eq!(read(&harness, &["shelves", "oak", "seen"]).await, "1,2");
    assert_eq!(
        read(&harness, &["shelves", "oak", "books", "1", "title"]).await,
        "Book 1 v1"
    );
    let books = path(&harness, &["shelves", "oak", "books"]).await;
    assert!(
        harness
            .namespace
            .lookup(books, "2")
            .await
            .unwrap()
            .is_missing()
    );
    assert_eq!(
        log(&harness).await[3..],
        [
            "oak version-0 ~books",
            "oak version-0 ~books",
            "oak change-1 ~books"
        ],
        "the change answered the next answer past the deadline, in two parts"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_feed_keeps_its_token_and_its_inner_states_answer_meanwhile() {
    let harness = make_initialized_runtime(CONFIG);

    // The first answer stops after its first part: the listing that started
    // it fails with the error of the stop.
    assert!(matches!(
        names(&harness, &["shelves", "flaky", "books"]).await,
        Err(NsError::Network)
    ));

    // The book of the published part serves from the projection, through
    // the inner state its row gave it, without a call of its feed.
    assert_eq!(
        read(&harness, &["shelves", "flaky", "books", "1", "title"]).await,
        "Book 1"
    );
    assert_eq!(log(&harness).await, ["flaky - books", "flaky page-1 books"]);

    // Past the deadline of that inner state, before its shelf feed ended,
    // the book's own feed answers for its title from the inner token.
    tokio::time::sleep(PAST_DEADLINE).await;
    assert_eq!(
        read(&harness, &["shelves", "flaky", "books", "1", "title"]).await,
        "Book 1 (own)"
    );
    assert_eq!(log(&harness).await.last().unwrap(), "flaky/1 row-1 -");

    // The next listing of `books` continues the first answer from the token
    // of its last published part.
    assert_eq!(
        names(&harness, &["shelves", "flaky", "books"])
            .await
            .unwrap(),
        ["1", "2"]
    );
    assert_eq!(
        log(&harness).await[3..],
        ["flaky page-1 books", "flaky page-2 books"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_later_answer_publishes_its_parts_with_its_last_part_and_continues_after_a_stop() {
    let harness = make_initialized_runtime(CONFIG);
    let segments = ["shelves", "wobbly", "books"];
    assert_eq!(names(&harness, &segments).await.unwrap(), ["1", "2"]);
    let shelf = path(&harness, &["shelves", "wobbly"]).await;
    let change = || async {
        let lookup = harness.namespace.lookup(shelf.clone(), "books").await;
        lookup.unwrap().attrs().unwrap().change
    };
    let listed = change().await;

    // A later answer keeps its first part and stops at a rate limit before
    // its last part: the check failed for now, so the operation is served
    // what the projection has, without the kept part.
    read(&harness, &["hello", "shelf-change"]).await;
    tokio::time::sleep(PAST_DEADLINE).await;
    let due = change().await;
    assert_ne!(due, listed);
    assert_eq!(names(&harness, &segments).await.unwrap(), ["1", "2"]);
    let calls = log(&harness).await.len();

    // While the window of the mount is open, each operation is served from
    // the projection without a call, and the attributes of the directory
    // stand where the deadline moved them.
    assert_eq!(names(&harness, &segments).await.unwrap(), ["1", "2"]);
    assert_eq!(
        read(&harness, &["shelves", "wobbly", "books", "1", "title"]).await,
        "Book 1"
    );
    assert_eq!(change().await, due);
    assert_eq!(log(&harness).await.len(), calls);

    // The check waits for the end of the window: the attributes of the
    // directory move once more then, and the listing that they bring
    // continues the answer from the token of its kept part, with what it
    // asks, and its last part publishes both parts.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let waited = change().await;
    assert!(waited != due && waited != listed);
    assert_eq!(names(&harness, &segments).await.unwrap(), ["1", "3"]);
    assert_eq!(
        read(&harness, &["shelves", "wobbly", "books", "1", "title"]).await,
        "Book 1 v1"
    );
    assert_eq!(read(&harness, &["shelves", "wobbly", "seen"]).await, "1,2");
    assert_eq!(
        log(&harness).await[3..],
        [
            "wobbly version-0 ~books",
            "wobbly change-1 ~books",
            "wobbly change-1 ~books"
        ]
    );
}

/// A check of a feed that fails for now at the network fails no operation
/// whose fact the projection has, and the next check waits as long as after
/// an answer; the operation that lacks its fact, and the listing of a first
/// answer that has not ended, have the error, within the wait without a call.
#[tokio::test(flavor = "multi_thread")]
async fn a_check_that_fails_for_now_serves_the_projection_and_the_next_check_waits() {
    let harness = make_initialized_runtime(CONFIG);
    let segments = ["shelves", "flaky", "books"];

    // The first answer stops after its first part: the listing that started
    // it has the error, and again within the wait, without a call.
    assert!(matches!(
        names(&harness, &segments).await,
        Err(NsError::Network)
    ));
    let calls = log(&harness).await.len();
    assert!(matches!(
        names(&harness, &segments).await,
        Err(NsError::Network)
    ));
    assert_eq!(log(&harness).await.len(), calls);

    // The book of the published part serves from the projection.
    assert_eq!(
        read(&harness, &["shelves", "flaky", "books", "1", "title"]).await,
        "Book 1"
    );
    assert_eq!(log(&harness).await.len(), calls);

    // After the wait the listing continues the first answer.
    tokio::time::sleep(PAST_DEADLINE).await;
    assert_eq!(names(&harness, &segments).await.unwrap(), ["1", "2"]);
}

/// The change of the attributes of a fed directory, which makes a filesystem
/// list it again: it stands within the deadline of the feed of its listing,
/// moves once past it, at a lookup and at a getattr alike, without a call of
/// the provider's feed, stands over the listing that asks the feed, and moves
/// with a change of the listing alone.
#[tokio::test(flavor = "multi_thread")]
async fn the_attributes_of_a_listed_fed_directory_change_once_past_the_deadline_of_its_feed() {
    let harness = make_initialized_runtime(CONFIG);
    let segments = ["shelves", "oak", "books"];
    assert_eq!(names(&harness, &segments).await.unwrap(), ["1", "2"]);
    let shelf = path(&harness, &["shelves", "oak"]).await;
    let books = path(&harness, &segments).await;
    let looked_up = || async {
        let lookup = harness.namespace.lookup(shelf.clone(), "books").await;
        lookup.unwrap().attrs().unwrap().change
    };
    let stat = || async {
        harness
            .namespace
            .getattr(books.clone())
            .await
            .unwrap()
            .change
    };
    let listed = looked_up().await;
    let calls = log(&harness).await.len();
    for _ in 0..3 {
        assert_eq!(looked_up().await, listed);
        assert_eq!(stat().await, listed);
        harness.namespace.lookup(books.clone(), "1").await.unwrap();
    }

    tokio::time::sleep(PAST_DEADLINE).await;
    let due = stat().await;
    assert_ne!(due, listed);
    assert_eq!(looked_up().await, due);
    assert_eq!(
        log(&harness).await.len(),
        calls,
        "no attribute asks the feed"
    );

    // The listing asks the feed, whose facts stand: no attribute moves.
    let title = path(&harness, &["shelves", "oak", "books", "1", "title"]).await;
    let unchanged = harness
        .namespace
        .getattr(title.clone())
        .await
        .unwrap()
        .change;
    assert_eq!(names(&harness, &segments).await.unwrap(), ["1", "2"]);
    assert_eq!(log(&harness).await.len(), calls + 1);
    assert_eq!(stat().await, due);
    assert_eq!(looked_up().await, due);
    assert_eq!(
        harness.namespace.getattr(title).await.unwrap().change,
        unchanged
    );

    // Past the next deadline the attributes move again, and a listing that
    // changes moves them once more.
    read(&harness, &["hello", "shelf-change"]).await;
    tokio::time::sleep(PAST_DEADLINE).await;
    let again = stat().await;
    assert_ne!(again, due);
    assert_eq!(names(&harness, &segments).await.unwrap(), ["1", "3"]);
    assert_ne!(stat().await, again);
}

/// A getattr of a file past its deadline reads nothing, and a read gives the
/// bytes of the size that the last attributes told, whole, whatever version
/// the read takes then: the next attributes tell the size of that version.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_gives_the_bytes_of_the_size_that_the_attributes_told() {
    let harness = make_initialized_runtime(CONFIG);
    let file = path(&harness, &["hello", "fresh-full"]).await;
    let read = || async {
        let read = harness.namespace.read(file.clone(), 0, u32::MAX).await;
        String::from_utf8(read.unwrap().bytes).unwrap()
    };
    let size = || async { harness.namespace.getattr(file.clone()).await.unwrap().size };
    // Each read of the provider gives another count, of another length from
    // the tenth on.
    while read().await != "fresh-full-9\n" {}
    let told = size().await;
    assert_eq!(told, 13);

    // Past the deadline no getattr reads the file.
    tokio::time::sleep(PAST_DEADLINE).await;
    for _ in 0..3 {
        assert_eq!(size().await, told);
    }
    // The read takes the tenth version, and gives the ninth, whose size its
    // reader has; the attributes after it tell the tenth, and the next read
    // gives the version of that size that it takes.
    assert_eq!(read().await, "fresh-full-9\n");
    assert_eq!(size().await, 14);
    assert_eq!(read().await, "fresh-full-11\n");
}

/// A mount without its provider serves a fed file as with it: by the ranges
/// of its body in the projection, which no size bounds.
#[tokio::test(flavor = "multi_thread")]
async fn a_mount_without_its_provider_serves_the_ranges_of_a_fed_file() {
    let harness = make_initialized_runtime(CONFIG);
    names(&harness, &["shelves", "oak", "books"]).await.unwrap();
    assert_eq!(
        read(&harness, &["shelves", "oak", "books", "1", "title"]).await,
        "Book 1"
    );

    let offline = harness.without_providers().unwrap();
    let mut file = Path::root();
    let mut found = None;
    for segment in ["test", "shelves", "oak", "books", "1", "title"] {
        let answer = offline.lookup(file, segment).await.unwrap();
        file = answer.path.clone();
        found = Some(answer);
    }
    let attrs = found.unwrap().attrs().unwrap().clone();
    assert_eq!(attrs.read_style, omnifs_engine::ReadStyle::Ranged);
    assert_eq!(attrs.size, 6);
    let range = offline.read(file, 2, 3).await.unwrap();
    assert_eq!(range.bytes, b"ok ");
    assert!(!range.eof);
}

/// An answer that writes the facts of a file as they were moves no attribute
/// of the file and sends no event for it: a filesystem keeps what it has of
/// an unchanged file.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_that_writes_a_file_as_it_was_changes_nothing_for_a_filesystem() {
    let harness = make_initialized_runtime(CONFIG);
    assert!(
        names(&harness, &["shelves", "flaky", "books"])
            .await
            .is_err()
    );
    let segments = ["shelves", "flaky", "books", "1", "title"];
    tokio::time::sleep(PAST_DEADLINE).await;
    assert_eq!(read(&harness, &segments).await, "Book 1 (own)");
    let title = path(&harness, &segments).await;
    let change = harness
        .namespace
        .getattr(title.clone())
        .await
        .unwrap()
        .change;
    let calls = log(&harness).await.len();
    let mut events = harness.namespace.subscribe();

    // The own feed of the book answers again, with the same title.
    tokio::time::sleep(PAST_DEADLINE).await;
    assert_eq!(read(&harness, &segments).await, "Book 1 (own)");
    assert_eq!(log(&harness).await.len(), calls + 1);
    assert_eq!(
        harness
            .namespace
            .getattr(title.clone())
            .await
            .unwrap()
            .change,
        change
    );
    while let Some(event) = events.try_recv() {
        assert!(
            !matches!(&event, omnifs_engine::NsEvent::AttrsChanged { path, .. } if *path == title),
            "{event:?}"
        );
    }
}

/// A part that declines the kept parts of its answer begins the answer: the
/// kept parts leave without their facts.
#[tokio::test(flavor = "multi_thread")]
async fn a_part_that_declines_the_kept_parts_of_its_answer_publishes_alone() {
    let harness = make_initialized_runtime(CONFIG);
    let segments = ["shelves", "fickle", "books"];
    assert_eq!(names(&harness, &segments).await.unwrap(), ["1", "2"]);
    read(&harness, &["hello", "shelf-change"]).await;
    tokio::time::sleep(PAST_DEADLINE).await;

    // The first part of the later answer, which the host keeps, renames
    // book 1 and adds book 3; the last part declines it and removes book 2.
    assert_eq!(names(&harness, &segments).await.unwrap(), ["1"]);
    assert_eq!(
        read(&harness, &["shelves", "fickle", "books", "1", "title"]).await,
        "Book 1"
    );
}

/// The `until` of a part marks the facts under the anchor of its own object
/// alone: the listing that a note joins is of the feed of its desk, and stays
/// complete past the time of the note.
#[tokio::test(flavor = "multi_thread")]
async fn the_until_of_a_part_marks_no_listing_that_its_object_joins() {
    let harness = make_initialized_runtime(CONFIG);
    let notes = ["desks", "dated", "notes"];
    assert_eq!(names(&harness, &notes).await.unwrap(), ["1"]);
    assert_eq!(
        read(&harness, &["desks", "dated", "notes", "2", "text"]).await,
        "Note 2"
    );
    let calls = log(&harness).await.len();
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(names(&harness, &notes).await.unwrap(), ["1", "2"]);
    assert_eq!(log(&harness).await.len(), calls, "the listing asks no feed");
}

/// An own feed whose object no inventory above it has answers for its paths
/// itself, past its own deadline: the desk that lists note 1 alone is not
/// asked for note 2.
#[tokio::test(flavor = "multi_thread")]
async fn an_unlisted_own_feed_is_asked_past_its_deadline_for_its_paths() {
    let harness = make_initialized_runtime(CONFIG);
    let text = ["desks", "oak", "notes", "2", "text"];
    assert_eq!(
        names(&harness, &["desks", "oak", "notes"]).await.unwrap(),
        ["1"]
    );
    assert_eq!(read(&harness, &text).await, "Note 2");
    tokio::time::sleep(PAST_DEADLINE).await;
    assert_eq!(read(&harness, &text).await, "Note 2");
    let log = log(&harness).await;
    // The lookup of the anchor of the note on the way to its text, a
    // starting path of the feed, asks it past its deadline.
    assert_eq!(log.last().unwrap(), "oak/2 1 ~", "{log:?}");
    assert!(!log.iter().any(|line| line == "oak 1 -"), "{log:?}");
}

/// A read that continues a fed file whose feed wrote another version since
/// the attributes that its reader has gives the rest of the version that it
/// began: no reader takes the bytes of two versions.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_that_continues_a_fed_file_gives_the_version_that_it_began() {
    let harness = make_initialized_runtime(CONFIG);
    let segments = ["shelves", "oak", "books", "1", "title"];
    assert_eq!(
        names(&harness, &["shelves", "oak", "books"]).await.unwrap(),
        ["1", "2"]
    );
    let title = path(&harness, &segments).await;
    read(&harness, &["hello", "shelf-change"]).await;
    tokio::time::sleep(PAST_DEADLINE).await;
    let rest = harness.namespace.read(title, 4, 64).await.unwrap();
    assert_eq!(rest.bytes, b" 1");
    assert!(rest.eof);
    assert_eq!(read(&harness, &segments).await, "Book 1 v1");
}

// The tests below take the shared helpers of a provider test, by the path
// of a node from the root of the namespace.
use omnifs_itest::support;

/// A feed reads a listing of the projection as its names, each parsed as a
/// key, or as its count, each with whether the listing is complete and with
/// the time that the part that wrote the listing gave its facts.
#[tokio::test(flavor = "multi_thread")]
async fn a_feed_reads_the_names_and_the_count_of_a_listing_with_its_completeness_and_its_time() {
    let harness = make_initialized_runtime(CONFIG);
    for shelf in ["oak", "dated"] {
        assert_eq!(
            support::names(&harness, &format!("/test/shelves/{shelf}/books"))
                .await
                .unwrap(),
            ["1", "2"]
        );
    }
    support::read(&harness, "/test/hello/shelf-change")
        .await
        .unwrap();
    tokio::time::sleep(PAST_DEADLINE).await;
    for (shelf, until) in [("oak", "None"), ("dated", "Some(4102444800)")] {
        assert_eq!(
            support::names(&harness, &format!("/test/shelves/{shelf}/books"))
                .await
                .unwrap(),
            ["1", "3"]
        );
        assert_eq!(
            support::read(&harness, &format!("/test/shelves/{shelf}/seen"))
                .await
                .unwrap(),
            "1,2"
        );
        assert_eq!(
            support::read(&harness, &format!("/test/shelves/{shelf}/listed"))
                .await
                .unwrap(),
            format!("names true {until}, count 2 true {until}")
        );
    }
}

/// A feed reads a fed file of the projection by its ranges, each with the
/// length of the whole file and the time of its facts, and a file that is
/// one record whole within the bytes it names: a longer file is too large.
#[tokio::test(flavor = "multi_thread")]
async fn a_feed_reads_a_fed_file_by_its_ranges_and_a_record_whole_within_its_bytes() {
    let harness = make_initialized_runtime(CONFIG);
    for shelf in ["oak", "dated"] {
        support::names(&harness, &format!("/test/shelves/{shelf}/books"))
            .await
            .unwrap();
    }
    support::read(&harness, "/test/hello/shelf-change")
        .await
        .unwrap();
    tokio::time::sleep(PAST_DEADLINE).await;
    for (shelf, until) in [("oak", "None"), ("dated", "Some(4102444800)")] {
        support::names(&harness, &format!("/test/shelves/{shelf}/books"))
            .await
            .unwrap();
        assert_eq!(
            support::read(&harness, &format!("/test/shelves/{shelf}/catalog-read"))
                .await
                .unwrap(),
            format!(
                "150000 bytes in 3 ranges, same, until {until}; \
                 as a record: TooLarge; title Some(\"Book 1\")"
            )
        );
    }
}

/// An object declared without a load serves its fed faces from the facts of
/// the feed of the object above it.
#[tokio::test(flavor = "multi_thread")]
async fn an_object_without_a_load_serves_its_fed_faces() {
    let harness = make_initialized_runtime(CONFIG);
    assert_eq!(
        support::names(&harness, "/test/shelves/oak/cards")
            .await
            .unwrap(),
        ["1"]
    );
    assert_eq!(
        support::names(&harness, "/test/shelves/oak/cards/1")
            .await
            .unwrap(),
        ["text"]
    );
    assert_eq!(
        support::read(&harness, "/test/shelves/oak/cards/1/text")
            .await
            .unwrap(),
        "Card 1"
    );
}

/// The faces of an object whose canonical bytes a feed writes render from
/// those bytes without a load of the object, within and past the deadline
/// of a loaded object, and from the bytes that a later part writes.
#[tokio::test(flavor = "multi_thread")]
async fn the_faces_of_an_object_render_from_the_canonical_bytes_of_a_feed_without_a_load() {
    let harness = &make_initialized_runtime(CONFIG);
    let faces = |version: u64| async move {
        assert_eq!(
            support::read(harness, "/test/shelves/oak/prints/1/print.json")
                .await
                .unwrap(),
            format!(r#"{{"print":1,"version":{version}}}"#)
        );
        assert_eq!(
            support::read(harness, "/test/shelves/oak/prints/1/print.md")
                .await
                .unwrap(),
            format!("# Print 1 v{version}\n")
        );
        let log = support::read(harness, "/test/hello/shelf-log")
            .await
            .unwrap();
        assert!(!log.contains("load"), "no load of the print: {log}");
    };
    assert_eq!(
        support::names(harness, "/test/shelves/oak/prints")
            .await
            .unwrap(),
        ["1"]
    );
    assert_eq!(
        support::names(harness, "/test/shelves/oak/prints/1")
            .await
            .unwrap(),
        ["print.json", "print.md"]
    );
    faces(0).await;
    let json = support::lookup(harness, "/test/shelves/oak/prints/1/print.json")
        .await
        .unwrap()
        .path;
    let change = || async {
        harness
            .namespace
            .getattr(json.clone())
            .await
            .unwrap()
            .change
    };
    let written = change().await;

    // Past the deadline of a loaded object, the bytes of the feed stand, and
    // an answer of the feed that writes none leaves the faces as they were.
    tokio::time::sleep(PAST_DEADLINE).await;
    faces(0).await;
    faces(0).await;
    support::names(harness, "/test/shelves/oak/prints")
        .await
        .unwrap();
    assert_eq!(change().await, written);

    // The next answer of the feed writes other bytes.
    support::read(harness, "/test/hello/shelf-change")
        .await
        .unwrap();
    tokio::time::sleep(PAST_DEADLINE).await;
    assert_eq!(
        support::names(harness, "/test/shelves/oak/prints")
            .await
            .unwrap(),
        ["1"]
    );
    faces(1).await;
    assert_ne!(
        change().await,
        written,
        "other bytes of the same length change the attributes of a face"
    );
}

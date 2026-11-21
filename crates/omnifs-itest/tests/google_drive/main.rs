#![cfg(not(target_os = "wasi"))]

//! The requests of the mount `google-drive` to Google Drive: the full
//! listings and the changes of the feed of the Drive, and the files, their
//! content, and the folders of their own feeds.

mod support;

use omnifs_engine::{Namespace, NsError, NsEvent};
use omnifs_itest::RuntimeHarness;
use serde_json::{Value, json};
use support::{
    PAST_DEADLINE, answering, blob, bytes, changed, changes, drive_harness, expect_bytes,
    expect_changes, expect_export, expect_item, expect_listing, expect_start, failure, files,
    google, lookup, names, ok, read, read_range, removed, served, trashed, uploaded,
};

const EDITED: &str = "2026-11-21T10:00:00.000Z";
const EDITED_LATER: &str = "2026-11-21T10:05:00.000Z";

/// The size of an uploaded file above the bytes that the host holds of a
/// file in memory.
const LARGE: u64 = 5 * 1024 * 1024 * 1024;

fn sorted(names: &[&str]) -> Vec<String> {
    let mut names = names.iter().map(ToString::to_string).collect::<Vec<_>>();
    names.sort();
    names
}

/// The first full listing of one page with `items`, after the token
/// `changes-1` of the changes.
async fn first_full_listing(harness: &RuntimeHarness, items: &[Value]) {
    let (listing, requests) = answering(
        harness,
        names(harness, "files"),
        vec![
            ok(&json!({ "startPageToken": "changes-1" })),
            ok(&files(items, None)),
        ],
    )
    .await;
    listing.unwrap();
    expect_start(&requests[0]);
    expect_listing(&requests[1], None);
}

/// The first full listing at the first read of `files/`: the token of the
/// changes after now, then the pages of the items by `nextPageToken`, with
/// the uploaded files of any size and the files of Google with an export as
/// files, the folders as folders, the other items as others, and no item in
/// the trash.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_first_full_listing_takes_the_items_by_next_page_token() {
    let harness = drive_harness();
    let notes = uploaded("file-1", "notes.md", "rev-1", 5);
    let document = google("doc-1", "document", "Plan", EDITED);
    let folder = google("folder-1", "folder", "Work", EDITED);
    let (listing, requests) = answering(
        &harness,
        names(&harness, "files"),
        vec![
            ok(&json!({ "startPageToken": "changes-1" })),
            ok(&files(
                &[
                    notes.clone(),
                    document.clone(),
                    folder.clone(),
                    google("form-1", "form", "Survey", EDITED),
                ],
                Some("page-2"),
            )),
            ok(&files(
                &[
                    google("sheet-1", "spreadsheet", "Budget", EDITED),
                    google("slides-1", "presentation", "Deck", EDITED),
                    google("drawing-1", "drawing", "Sketch", EDITED),
                    google("script-1", "script", "Macros", EDITED),
                    google("shortcut-1", "shortcut", "Link", EDITED),
                    uploaded("video-1", "recording.mp4", "rev-2", LARGE),
                    trashed(uploaded("file-2", "old.md", "rev-3", 3)),
                    trashed(google("form-2", "form", "Old survey", EDITED)),
                ],
                None,
            )),
        ],
    )
    .await;
    assert_eq!(
        listing.unwrap(),
        sorted(&[
            "file-1",
            "doc-1",
            "sheet-1",
            "slides-1",
            "drawing-1",
            "script-1",
            "video-1"
        ])
    );
    expect_start(&requests[0]);
    expect_listing(&requests[1], None);
    expect_listing(&requests[2], Some("page-2"));

    assert_eq!(
        served(&harness, names(&harness, "folders")).await.unwrap(),
        ["folder-1"]
    );
    assert_eq!(
        served(&harness, names(&harness, "others")).await.unwrap(),
        sorted(&["form-1", "shortcut-1"])
    );
    assert_eq!(
        served(&harness, names(&harness, "others/form-1"))
            .await
            .unwrap(),
        ["item.json", "name"]
    );
    assert_eq!(
        served(&harness, read(&harness, "others/form-1/name"))
            .await
            .unwrap(),
        "Survey"
    );
    assert_eq!(
        served(&harness, names(&harness, "files/file-1"))
            .await
            .unwrap(),
        ["content", "file.json", "name"]
    );
    assert_eq!(
        served(&harness, names(&harness, "folders/folder-1"))
            .await
            .unwrap(),
        ["folder.json", "name"]
    );
    for (path, expected) in [
        ("files/file-1/name", "notes.md"),
        ("files/doc-1/name", "Plan"),
        ("folders/folder-1/name", "Work"),
    ] {
        assert_eq!(
            served(&harness, read(&harness, path)).await.unwrap(),
            expected,
            "{path}"
        );
    }
    for (path, object) in [
        ("files/file-1/file.json", &notes),
        ("files/doc-1/file.json", &document),
        ("folders/folder-1/folder.json", &folder),
    ] {
        let read: Value =
            serde_json::from_str(&served(&harness, read(&harness, path)).await.unwrap()).unwrap();
        assert_eq!(&read, object, "{path}");
    }
}

/// The content of a file at the first read of its `content`: the bytes of
/// the head revision of an uploaded file, which the host holds and serves a
/// range of, and the Markdown of a document, the CSV of a spreadsheet, the
/// plain text of a presentation, the SVG of a drawing, and the JSON of a
/// project of Apps Script, each served after it without a request.
#[tokio::test(flavor = "multi_thread")]
async fn the_content_takes_the_bytes_of_an_uploaded_file_and_the_export_of_a_file_of_google() {
    let harness = drive_harness();
    first_full_listing(
        &harness,
        &[
            uploaded("file-1", "notes.md", "rev-1", 5),
            google("doc-1", "document", "Plan", EDITED),
            google("sheet-1", "spreadsheet", "Budget", EDITED),
            google("slides-1", "presentation", "Deck", EDITED),
            google("drawing-1", "drawing", "Sketch", EDITED),
            google("script-1", "script", "Macros", EDITED),
        ],
    )
    .await;

    // The file `content` keeps its identity over the answer that writes it,
    // as the filehandle of the read that waits for the answer.
    let file = served(&harness, lookup(&harness, "files/file-1/content"))
        .await
        .unwrap()
        .path;
    let mut events = harness.namespace.subscribe();
    let (content, requests) = answering(
        &harness,
        read(&harness, "files/file-1/content"),
        vec![blob(200, b"notes")],
    )
    .await;
    assert_eq!(content.unwrap(), "notes");
    expect_bytes(&requests[0], "file-1", "rev-1");
    // A read of a range of the file has the bytes of the range alone.
    assert_eq!(
        served(&harness, read_range(&harness, "files/file-1/content", 1, 3))
            .await
            .unwrap(),
        ("ote".to_owned(), false)
    );
    assert_eq!(
        served(&harness, read_range(&harness, "files/file-1/content", 3, 9))
            .await
            .unwrap(),
        ("es".to_owned(), true)
    );
    // The next operation on the mount gives its events.
    served(&harness, lookup(&harness, "files/file-1/content"))
        .await
        .unwrap();
    let events = std::iter::from_fn(|| events.try_recv()).collect::<Vec<_>>();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, NsEvent::AttrsChanged { path, .. } if *path == file)),
        "{events:?}"
    );
    assert!(
        !events.iter().any(
            |event| matches!(event, NsEvent::InvalidateSubtree { path } if file.has_prefix(path))
        ),
        "{events:?}"
    );

    for (id, mime_type, export) in [
        ("doc-1", "text/markdown", "# Plan"),
        ("sheet-1", "text/csv", "a,b\n1,2"),
        ("slides-1", "text/plain", "Deck"),
        ("drawing-1", "image/svg+xml", "<svg/>"),
        (
            "script-1",
            "application/vnd.google-apps.script+json",
            "{\"files\":[]}",
        ),
    ] {
        let path = format!("files/{id}/content");
        let (content, requests) = answering(
            &harness,
            read(&harness, &path),
            vec![bytes(200, export.as_bytes())],
        )
        .await;
        assert_eq!(content.unwrap(), export, "{id}");
        expect_export(&requests[0], id, mime_type);
        assert_eq!(
            served(&harness, read(&harness, &path)).await.unwrap(),
            export,
            "{id}"
        );
    }
}

/// The changes at a read of `files/` past the deadline of the feed, from the
/// token of the answer before: a new file, a file with other content, whose
/// content the next read takes again, a file with another name alone, whose
/// content stays, and the removal of an item that the account no longer has
/// and of one in the trash.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_changes_take_the_files_after_the_token_of_the_answer_before() {
    let harness = drive_harness();
    first_full_listing(
        &harness,
        &[
            uploaded("file-1", "notes.md", "rev-1", 5),
            uploaded("file-2", "kept.md", "rev-2", 4),
            uploaded("file-3", "shared.md", "rev-3", 6),
            uploaded("file-4", "old.md", "rev-4", 3),
            google("doc-1", "document", "Plan", EDITED),
            google("folder-1", "folder", "Work", EDITED),
            google("form-1", "form", "Survey", EDITED),
        ],
    )
    .await;
    for (id, body) in [("file-1", "notes"), ("file-2", "kept")] {
        let (content, _) = answering(
            &harness,
            read(&harness, &format!("files/{id}/content")),
            vec![blob(200, body.as_bytes())],
        )
        .await;
        assert_eq!(content.unwrap(), body);
    }
    let (content, _) = answering(
        &harness,
        read(&harness, "files/doc-1/content"),
        vec![bytes(200, b"# Plan")],
    )
    .await;
    assert_eq!(content.unwrap(), "# Plan");

    tokio::time::sleep(PAST_DEADLINE).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, "files"),
        vec![
            ok(&json!({
                "changes": [
                    changed(&uploaded("file-1", "notes.md", "rev-1b", 6)),
                    changed(&uploaded("file-2", "renamed.md", "rev-2", 4)),
                    removed("file-3"),
                ],
                "nextPageToken": "changes-1b",
            })),
            ok(&changes(
                &[
                    changed(&trashed(uploaded("file-4", "old.md", "rev-4", 3))),
                    changed(&uploaded("file-6", "new.md", "rev-6", 3)),
                    removed("form-1"),
                    changed(&google("form-2", "form", "Poll", EDITED)),
                    changed(&google("doc-1", "document", "Plan", EDITED_LATER)),
                    removed("folder-1"),
                    changed(&google("folder-2", "folder", "Home", EDITED)),
                    json!({ "changeType": "drive", "driveId": "drive-1", "removed": false }),
                ],
                "changes-2",
            )),
        ],
    )
    .await;
    assert_eq!(
        listing.unwrap(),
        sorted(&["file-1", "file-2", "file-6", "doc-1"])
    );
    expect_changes(&requests[0], "changes-1");
    expect_changes(&requests[1], "changes-1b");
    assert_eq!(
        served(&harness, names(&harness, "folders")).await.unwrap(),
        ["folder-2"]
    );
    assert_eq!(
        served(&harness, names(&harness, "others")).await.unwrap(),
        ["form-2"]
    );

    assert_eq!(
        served(&harness, read(&harness, "files/file-2/name"))
            .await
            .unwrap(),
        "renamed.md"
    );
    assert_eq!(
        served(&harness, read(&harness, "files/file-2/content"))
            .await
            .unwrap(),
        "kept"
    );
    let (content, requests) = answering(
        &harness,
        read(&harness, "files/file-1/content"),
        vec![blob(200, b"notes!")],
    )
    .await;
    assert_eq!(content.unwrap(), "notes!");
    expect_bytes(&requests[0], "file-1", "rev-1b");
    let (content, requests) = answering(
        &harness,
        read(&harness, "files/doc-1/content"),
        vec![bytes(200, b"# Plan, edited")],
    )
    .await;
    assert_eq!(content.unwrap(), "# Plan, edited");
    expect_export(&requests[0], "doc-1", "text/markdown");

    // The next changes come from the token after the last page.
    tokio::time::sleep(PAST_DEADLINE).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, "files"),
        vec![ok(&changes(&[], "changes-3"))],
    )
    .await;
    assert_eq!(
        listing.unwrap(),
        sorted(&["file-1", "file-2", "file-6", "doc-1"])
    );
    expect_changes(&requests[0], "changes-2");
}

/// A token of the changes that Google Drive refuses with `400` gives the read
/// its error and begins a full listing again at the next read, after which
/// the files and the folders that it lacks leave the mount, with the content
/// of the others as it was; a page of that listing whose token it refuses
/// with `400` begins the listing again, and one for which it searched not
/// every drive is the error of the read. Another failing status of the
/// changes is the error of the read alone.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn a_refused_token_of_the_changes_begins_a_full_listing_again() {
    let harness = drive_harness();
    first_full_listing(
        &harness,
        &[
            uploaded("file-1", "notes.md", "rev-1", 5),
            uploaded("file-2", "gone.md", "rev-2", 4),
            google("folder-1", "folder", "Work", EDITED),
            google("folder-2", "folder", "Gone", EDITED),
        ],
    )
    .await;
    let (content, _) = answering(
        &harness,
        read(&harness, "files/file-1/content"),
        vec![blob(200, b"notes")],
    )
    .await;
    assert_eq!(content.unwrap(), "notes");

    // A status other than `400` leaves the token of the changes as it was.
    tokio::time::sleep(PAST_DEADLINE).await;
    let (denied, requests) = answering(
        &harness,
        names(&harness, "files"),
        vec![failure(403, "insufficientPermissions")],
    )
    .await;
    assert!(denied.is_err(), "{denied:?}");
    expect_changes(&requests[0], "changes-1");

    tokio::time::sleep(PAST_DEADLINE).await;
    let (refused, requests) = answering(
        &harness,
        names(&harness, "files"),
        vec![failure(400, "invalid")],
    )
    .await;
    assert!(refused.is_err(), "{refused:?}");
    expect_changes(&requests[0], "changes-1");

    // The second page of the full listing is refused for its token, and the
    // listing begins again from its first page.
    tokio::time::sleep(PAST_DEADLINE).await;
    let first = files(
        &[uploaded("file-1", "notes.md", "rev-1", 5)],
        Some("page-2"),
    );
    let (refused, requests) = answering(
        &harness,
        names(&harness, "files"),
        vec![
            ok(&json!({ "startPageToken": "changes-5" })),
            ok(&first),
            failure(400, "invalid"),
        ],
    )
    .await;
    assert!(refused.is_err(), "{refused:?}");
    expect_start(&requests[0]);
    expect_listing(&requests[1], None);
    expect_listing(&requests[2], Some("page-2"));

    // A page for which Google Drive searched not every drive ends no listing.
    tokio::time::sleep(PAST_DEADLINE).await;
    let mut incomplete = first.clone();
    incomplete["incompleteSearch"] = true.into();
    let (stopped, requests) =
        answering(&harness, names(&harness, "files"), vec![ok(&incomplete)]).await;
    assert!(stopped.is_err(), "{stopped:?}");
    expect_listing(&requests[0], None);

    tokio::time::sleep(PAST_DEADLINE).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, "files"),
        vec![
            ok(&first),
            ok(&files(
                &[
                    uploaded("file-3", "new.md", "rev-3", 3),
                    google("folder-1", "folder", "Work", EDITED),
                ],
                None,
            )),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), sorted(&["file-1", "file-3"]));
    expect_listing(&requests[0], None);
    expect_listing(&requests[1], Some("page-2"));
    assert_eq!(
        served(&harness, read(&harness, "files/file-1/content"))
            .await
            .unwrap(),
        "notes"
    );
    assert_eq!(
        served(&harness, names(&harness, "folders")).await.unwrap(),
        ["folder-1"]
    );

    tokio::time::sleep(PAST_DEADLINE).await;
    let (_, requests) = answering(
        &harness,
        names(&harness, "files"),
        vec![ok(&changes(&[], "changes-6"))],
    )
    .await;
    expect_changes(&requests[0], "changes-5");
}

/// A file, a folder, and another item by their IDs, before a full listing:
/// found with their facts, and missing for `404`, in the trash, or of
/// another kind.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_a_folder_and_another_item_by_their_ids() {
    let harness = drive_harness();
    let (found, requests) = answering(
        &harness,
        read(&harness, "files/file-1/name"),
        vec![ok(&uploaded("file-1", "notes.md", "rev-1", 5))],
    )
    .await;
    assert_eq!(found.unwrap(), "notes.md");
    expect_item(&requests[0], "file-1");
    let (content, requests) = answering(
        &harness,
        read(&harness, "files/file-1/content"),
        vec![blob(200, b"notes")],
    )
    .await;
    assert_eq!(content.unwrap(), "notes");
    expect_bytes(&requests[0], "file-1", "rev-1");

    let (found, requests) = answering(
        &harness,
        read(&harness, "others/form-1/name"),
        vec![ok(&google("form-1", "form", "Survey", EDITED))],
    )
    .await;
    assert_eq!(found.unwrap(), "Survey");
    expect_item(&requests[0], "form-1");

    let (found, requests) = answering(
        &harness,
        read(&harness, "folders/folder-1/name"),
        vec![ok(&google("folder-1", "folder", "Work", EDITED))],
    )
    .await;
    assert_eq!(found.unwrap(), "Work");
    expect_item(&requests[0], "folder-1");

    for (path, id, answer) in [
        ("files/file-2", "file-2", failure(404, "notFound")),
        (
            "files/file-3",
            "file-3",
            ok(&trashed(uploaded("file-3", "old.md", "rev-3", 3))),
        ),
        (
            "files/folder-1",
            "folder-1",
            ok(&google("folder-1", "folder", "Work", EDITED)),
        ),
        (
            "files/form-1",
            "form-1",
            ok(&google("form-1", "form", "Survey", EDITED)),
        ),
        (
            "others/doc-1",
            "doc-1",
            ok(&google("doc-1", "document", "Plan", EDITED)),
        ),
        (
            "folders/file-1",
            "file-1",
            ok(&uploaded("file-1", "notes.md", "rev-1", 5)),
        ),
    ] {
        let (found, requests) = answering(&harness, lookup(&harness, path), vec![answer]).await;
        assert!(found.unwrap().is_missing(), "{path}");
        expect_item(&requests[0], id);
    }
}

/// The content that Google Drive refuses is the error of its read, which the
/// next read asks for again; the content of a file that Google Drive no
/// longer has removes the file.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_content_is_the_error_of_its_read_and_a_missing_one_removes_its_file() {
    let harness = drive_harness();
    first_full_listing(
        &harness,
        &[
            uploaded("file-1", "restricted.pdf", "rev-1", 5),
            uploaded("file-2", "gone.md", "rev-2", 4),
            google("sheet-1", "spreadsheet", "Large", EDITED),
        ],
    )
    .await;

    for _ in 0..2 {
        let (refused, requests) = answering(
            &harness,
            read(&harness, "files/file-1/content"),
            vec![blob(403, b"{}")],
        )
        .await;
        assert!(matches!(refused, Err(NsError::Permission)), "{refused:?}");
        expect_bytes(&requests[0], "file-1", "rev-1");
    }
    let (content, _) = answering(
        &harness,
        read(&harness, "files/file-1/content"),
        vec![blob(200, b"bytes")],
    )
    .await;
    assert_eq!(content.unwrap(), "bytes");

    let (refused, requests) = answering(
        &harness,
        read(&harness, "files/sheet-1/content"),
        vec![failure(403, "exportSizeLimitExceeded")],
    )
    .await;
    assert!(matches!(refused, Err(NsError::Permission)), "{refused:?}");
    expect_export(&requests[0], "sheet-1", "text/csv");
    assert_eq!(
        served(&harness, read(&harness, "files/sheet-1/name"))
            .await
            .unwrap(),
        "Large"
    );

    let (gone, requests) = answering(
        &harness,
        read(&harness, "files/file-2/content"),
        vec![blob(404, b"{}")],
    )
    .await;
    assert!(matches!(gone, Err(NsError::NotFound)), "{gone:?}");
    expect_bytes(&requests[0], "file-2", "rev-2");
    assert_eq!(
        served(&harness, names(&harness, "files")).await.unwrap(),
        sorted(&["file-1", "sheet-1"])
    );
}

/// A `403` of a request over a quota gives a read the error of a rate limit.
#[tokio::test(flavor = "multi_thread")]
async fn a_403_over_a_quota_is_a_rate_limit() {
    let harness = drive_harness();
    let (limited, requests) = answering(
        &harness,
        names(&harness, "files"),
        vec![failure(403, "userRateLimitExceeded")],
    )
    .await;
    assert!(
        matches!(limited, Err(NsError::RateLimited { .. })),
        "{limited:?}"
    );
    expect_start(&requests[0]);
}

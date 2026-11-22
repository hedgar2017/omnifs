#![cfg(not(target_os = "wasi"))]

//! The requests of the mount `google-drive` to Google Drive: the full
//! listings and the changes of the feed of the Drive, and the items and the
//! content of the files of their own feeds.

mod support;

use omnifs_engine::{Namespace, NsError, NsEvent};
use omnifs_itest::RuntimeHarness;
use omnifs_itest::support::{
    Answer, Request, answering, lookup, names, next_callout, read, served,
};
use omnifs_wit::host::types::{Callout, CalloutError, CalloutResult, ErrorKind};
use serde_json::{Value, json};
use support::{
    Corpus, at, blob, bytes, changed, changes, drive_harness, drives, expect_bytes, expect_changes,
    expect_drives, expect_export, expect_export_refusal, expect_first_byte, expect_item,
    expect_listing, expect_revision, expect_start, failure, files, google, in_drive, md5, ok,
    read_range, removed, sized, trashed, uploaded, without,
};

const EDITED: &str = "2026-11-21T10:00:00.000Z";
const EDITED_LATER: &str = "2026-11-21T10:05:00.000Z";

/// The size of the largest uploaded file of Google Drive.
const LARGEST: u64 = 5 * 1024 * 1024 * 1024 * 1024;

/// The size of an uploaded file above the bytes that the host holds of a
/// file in memory, and the MD5 of that many bytes `x`.
const LARGE: usize = 8 * 1024 * 1024;
const LARGE_MD5: &str = "2058fb53f643fcd58a8d83a05542392b";

fn sorted(names: &[&str]) -> Vec<String> {
    let mut names = names.iter().map(ToString::to_string).collect::<Vec<_>>();
    names.sort();
    names
}

/// The names of the directory `path` of the mount, served from the
/// projection.
async fn listed(harness: &RuntimeHarness, path: &str) -> Vec<String> {
    served(harness, names(harness, &at(path))).await.unwrap()
}

/// The text of the file `path` of the mount, served from the projection.
async fn text(harness: &RuntimeHarness, path: &str) -> String {
    served(harness, read(harness, &at(path))).await.unwrap()
}

/// Whether the mount has no `path`, served from the projection.
async fn missing(harness: &RuntimeHarness, path: &str) -> bool {
    served(harness, lookup(harness, &at(path)))
        .await
        .unwrap()
        .is_missing()
}

/// The first full listing with `items` of the user and no shared drive,
/// after the token `changes-1` of the changes.
async fn first_full_listing(harness: &RuntimeHarness, items: &[Value]) {
    let (listing, requests) = answering(
        harness,
        names(harness, &at("files")),
        [
            ok(&json!({ "startPageToken": "changes-1" })),
            ok(&files(items, None)),
            ok(&drives(&[], None)),
        ],
    )
    .await;
    listing.unwrap();
    expect_start(&requests[0]);
    expect_listing(&requests[1], Corpus::User, None);
    expect_drives(&requests[2], None);
}

/// A read of the file `path` of the mount with the answers `answers`.
async fn reading(
    harness: &RuntimeHarness,
    path: &str,
    answers: impl IntoIterator<Item = Answer>,
) -> (Result<String, NsError>, Vec<Request>) {
    answering(harness, read(harness, &at(path)), answers).await
}

/// The first full listing at the first read of `files/`: the token of the
/// changes after now, then the pages of the items of the user by
/// `nextPageToken`, the pages of the shared drives, and the pages of the
/// items of each drive, with the uploaded files of any size, with an MD5 or
/// without one, and the files of Google with an export as files, the
/// folders as folders, the other items and an item without a size as
/// others, and no item in the trash. An item whose object has no name is in
/// the mount, and its `name` has the error of its object by its ID. An item
/// that the mount took by its ID as another item is a file alone after the
/// listing. A page that Google Drive refuses with `400` for its token begins
/// the listing again with the token of the changes of its start. A request
/// of a listing that fails for now leaves the check of each item that the
/// listing brought to its own feed in a sync. An object without an ID, a
/// MIME type, or `trashed` is the error of the listing, whose other items
/// are in the mount.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_first_full_listing_takes_the_items_of_the_user_and_of_each_shared_drive() {
    let harness = drive_harness();
    // By its ID as another item, before the listing brings it as a file.
    let (survey, requests) = reading(
        &harness,
        "others/x-1/name",
        [ok(&google("x-1", "form", "Survey", EDITED))],
    )
    .await;
    assert_eq!(survey.unwrap(), "Survey");
    expect_item(&requests[0], "x-1");

    let notes = uploaded("file-1", "notes.md", "notes");
    let renamed_notes = uploaded("file-1", "renamed.md", "notes");
    let document = google("doc-1", "document", "Plan", EDITED);
    let folder = google("folder-1", "folder", "Work", EDITED);
    let bare = without(uploaded("bare-1", "link.url", ""), "size");
    let headless = without(uploaded("head-1", "raw.bin", "old"), "md5Checksum");
    let nameless = without(uploaded("broken-1", "broken.md", "old"), "name");
    let first = files(
        &[
            notes.clone(),
            nameless.clone(),
            document.clone(),
            folder.clone(),
            google("form-1", "form", "Survey", EDITED),
            bare.clone(),
            headless.clone(),
            uploaded("x-1", "survey.csv", "kept"),
            without(uploaded("anon-1", "anon.md", "old"), "id"),
            without(google("blank-1", "form", "Blank", EDITED), "mimeType"),
        ],
        Some("page-2"),
    );
    let (refused, requests) = answering(
        &harness,
        names(&harness, &at("files")),
        [
            ok(&json!({ "startPageToken": "changes-1" })),
            ok(&first),
            failure(400, "invalid"),
        ],
    )
    .await;
    assert!(matches!(refused, Err(NsError::Invalid)), "{refused:?}");
    expect_start(&requests[0]);
    expect_listing(&requests[1], Corpus::User, None);
    expect_listing(&requests[2], Corpus::User, Some("page-2"));

    // A page of the items of the user that fails for now stops the listing
    // after the first one, whose items are in the mount.
    let item = served(&harness, lookup(&harness, &at("files/file-1")))
        .await
        .unwrap()
        .path;
    let name = served(&harness, lookup(&harness, &at("files/file-1/name")))
        .await
        .unwrap()
        .path;
    let (stopped, _) = answering(
        &harness,
        names(&harness, &at("files")),
        [ok(&first), failure(503, "backendError")],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");
    // In a sync, an operation on an item that the listing brought checks it
    // by its ID while the listing has not ended.
    harness.begin_sync();
    let (old, requests) = answering(
        &harness,
        harness.namespace.read(name.clone(), 0, u32::MAX),
        [ok(&renamed_notes)],
    )
    .await;
    assert_eq!(old.unwrap().bytes, b"notes.md");
    expect_item(&requests[0], "file-1");
    let renamed = served(&harness, async {
        let name = harness.namespace.lookup(item, "name").await?;
        harness.namespace.read(name.path, 0, u32::MAX).await
    })
    .await;
    assert_eq!(renamed.unwrap().bytes, b"renamed.md");

    let (listing, requests) = answering(
        &harness,
        names(&harness, &at("files")),
        [
            ok(&files(
                &[
                    google("sheet-1", "spreadsheet", "Budget", EDITED),
                    google("slides-1", "presentation", "Deck", EDITED),
                    google("drawing-1", "drawing", "Sketch", EDITED),
                    google("script-1", "script", "Macros", EDITED),
                    google("shortcut-1", "shortcut", "Link", EDITED),
                    sized("video-1", "recording.mp4", LARGE_MD5, LARGEST),
                    trashed(uploaded("file-2", "old.md", "old")),
                    trashed(google("form-2", "form", "Old survey", EDITED)),
                ],
                None,
            )),
            ok(&drives(&["drive-1", "drive-2"], Some("drives-2"))),
            ok(&files(
                &[in_drive(uploaded("team-1", "team.md", "shared"), "drive-1")],
                Some("drive-page-2"),
            )),
            ok(&files(
                &[in_drive(
                    google("team-2", "folder", "Team", EDITED),
                    "drive-1",
                )],
                None,
            )),
            ok(&files(&[], None)),
            ok(&drives(&["drive-3"], None)),
            ok(&files(
                &[in_drive(
                    google("team-3", "form", "Poll", EDITED),
                    "drive-3",
                )],
                None,
            )),
        ],
    )
    .await;
    // The objects without what names them are the error of the listing,
    // after the facts of the other items.
    assert!(matches!(listing, Err(NsError::Invalid)), "{listing:?}");
    assert_eq!(
        listed(&harness, "files").await,
        sorted(&[
            "file-1",
            "broken-1",
            "doc-1",
            "head-1",
            "x-1",
            "sheet-1",
            "slides-1",
            "drawing-1",
            "script-1",
            "video-1",
            "team-1",
        ])
    );
    expect_listing(&requests[0], Corpus::User, Some("page-2"));
    expect_drives(&requests[1], None);
    expect_listing(&requests[2], Corpus::Drive("drive-1"), None);
    expect_listing(&requests[3], Corpus::Drive("drive-1"), Some("drive-page-2"));
    expect_listing(&requests[4], Corpus::Drive("drive-2"), None);
    expect_drives(&requests[5], Some("drives-2"));
    expect_listing(&requests[6], Corpus::Drive("drive-3"), None);

    assert_eq!(
        listed(&harness, "folders").await,
        sorted(&["folder-1", "team-2"])
    );
    assert_eq!(
        listed(&harness, "others").await,
        sorted(&["form-1", "shortcut-1", "bare-1", "team-3"])
    );
    // The item that the mount had by its ID as another item is a file alone.
    let (as_other, requests) = answering(
        &harness,
        lookup(&harness, &at("others/x-1")),
        [ok(&uploaded("x-1", "survey.csv", "kept"))],
    )
    .await;
    assert!(as_other.unwrap().is_missing());
    expect_item(&requests[0], "x-1");
    assert_eq!(
        listed(&harness, "others/form-1").await,
        ["item.json", "name"]
    );
    assert_eq!(
        listed(&harness, "files/file-1").await,
        ["content", "file.json", "name"]
    );
    assert_eq!(
        listed(&harness, "folders/folder-1").await,
        ["folder.json", "name"]
    );
    for (path, expected) in [
        ("files/file-1/name", "renamed.md"),
        ("files/doc-1/name", "Plan"),
        ("files/x-1/name", "survey.csv"),
        ("folders/folder-1/name", "Work"),
        ("others/form-1/name", "Survey"),
        ("others/bare-1/name", "link.url"),
    ] {
        assert_eq!(text(&harness, path).await, expected, "{path}");
    }
    for (path, object) in [
        ("files/file-1/file.json", &renamed_notes),
        ("files/doc-1/file.json", &document),
        ("files/broken-1/file.json", &nameless),
        ("files/head-1/file.json", &headless),
        ("folders/folder-1/folder.json", &folder),
        ("others/bare-1/item.json", &bare),
    ] {
        let read: Value = serde_json::from_str(&text(&harness, path).await).unwrap();
        assert_eq!(&read, object, "{path}");
    }

    // The path of the ID of an object without a MIME type asks Google Drive
    // for it by its ID once in the sync, and has its error: the next
    // operation in the sync has the error without a request.
    let blank = without(google("blank-1", "form", "Blank", EDITED), "mimeType");
    let (failed, requests) = answering(
        &harness,
        lookup(&harness, &at("others/blank-1")),
        [ok(&blank)],
    )
    .await;
    assert!(matches!(failed, Err(NsError::Invalid)), "{failed:?}");
    expect_item(&requests[0], "blank-1");
    let failed = served(&harness, lookup(&harness, &at("others/blank-1"))).await;
    assert!(matches!(failed, Err(NsError::Invalid)), "{failed:?}");

    // The name that Google Drive states not is the error of its read, by
    // the item by its ID.
    let (nameless_read, requests) = reading(&harness, "files/broken-1/name", [ok(&nameless)]).await;
    assert!(
        matches!(nameless_read, Err(NsError::Invalid)),
        "{nameless_read:?}"
    );
    expect_item(&requests[0], "broken-1");
}

/// The content of a file at the first read of its `content`, at the
/// revision of the projection, in one request: the bytes of an uploaded
/// file with their MD5, which the host holds and serves a range of, of a
/// file above the bytes that the host holds in memory too, the bytes of the
/// revision of a file without an MD5, and the Markdown of a document, the
/// CSV of a spreadsheet, the plain text of a presentation, the SVG of a
/// drawing, and the JSON of a project of Apps Script at the time of their
/// modification, each served after it without a request. Bytes of another
/// MD5 than the one of the projection take the file by its ID and its bytes
/// of its MD5, which its change after keeps.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_content_takes_the_bytes_and_the_export_of_a_file_at_the_revision_of_the_projection() {
    let harness = drive_harness();
    let large = sized("large-1", "recording.mp4", LARGE_MD5, LARGE as u64);
    let headless = without(uploaded("head-1", "raw.bin", "old"), "md5Checksum");
    let exports = [
        ("doc-1", "document", "text/markdown", "# Plan"),
        ("sheet-1", "spreadsheet", "text/csv", "a,b\n1,2"),
        ("slides-1", "presentation", "text/plain", "Deck"),
        ("drawing-1", "drawing", "image/svg+xml", "<svg/>"),
        (
            "script-1",
            "script",
            "application/vnd.google-apps.script+json",
            "{\"files\":[]}",
        ),
    ];
    let mut items = vec![
        uploaded("file-1", "notes.md", "notes"),
        uploaded("file-2", "draft.md", "old"),
        large,
        headless,
    ];
    items.extend(
        exports
            .iter()
            .map(|(id, kind, _, _)| google(id, kind, "Plan", EDITED)),
    );
    first_full_listing(&harness, &items).await;

    // The file `content` keeps its identity over the answer that writes it,
    // as the filehandle of the read that waits for the answer.
    let file = served(&harness, lookup(&harness, &at("files/file-1/content")))
        .await
        .unwrap()
        .path;
    let mut events = harness.namespace.subscribe();
    let (content, requests) =
        reading(&harness, "files/file-1/content", [blob(200, b"notes")]).await;
    assert_eq!(content.unwrap(), "notes");
    expect_bytes(&requests[0], "file-1", md5("notes"));
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
    served(&harness, lookup(&harness, &at("files/file-1/content")))
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

    let end = LARGE as u64 - 3;
    let (range, requests) = answering(
        &harness,
        read_range(&harness, "files/large-1/content", end, 9),
        [blob(200, &vec![b'x'; LARGE])],
    )
    .await;
    assert_eq!(range.unwrap(), ("xxx".to_owned(), true));
    expect_bytes(&requests[0], "large-1", LARGE_MD5);
    assert_eq!(
        served(
            &harness,
            read_range(&harness, "files/large-1/content", 0, 4)
        )
        .await
        .unwrap(),
        ("xxxx".to_owned(), false)
    );

    let (content, requests) = reading(&harness, "files/head-1/content", [blob(200, b"raw")]).await;
    assert_eq!(content.unwrap(), "raw");
    expect_revision(
        &requests[0],
        "head-1",
        &format!("revision-{}", &md5("old")[..8]),
    );

    for (id, _, mime_type, export) in exports {
        let path = format!("files/{id}/content");
        let (content, requests) = reading(&harness, &path, [blob(200, export.as_bytes())]).await;
        assert_eq!(content.unwrap(), export, "{id}");
        expect_export(&requests[0], id, mime_type, EDITED);
        assert_eq!(text(&harness, &path).await, export, "{id}");
    }

    // The file is uploaded again before its change: the bytes of another
    // MD5 take the file by its ID, and its bytes of its MD5, which stay at
    // the change of that MD5.
    let uploaded_again = uploaded("file-2", "draft.md", "new");
    let content_path = at("files/file-2/content");
    let (content, ()) = tokio::join!(read(&harness, &content_path), async {
        let first = next_callout(&harness).await;
        let Callout::FetchBlob(asked) = first.callout().clone() else {
            panic!("the bytes are a blob fetch: {:?}", first.callout());
        };
        assert_eq!(asked.md5.as_deref(), Some(md5("old")));
        harness.runtime.answer_test_blob(first, 200, b"new").await;
        let by_id = next_callout(&harness).await;
        let Callout::Fetch(request) = by_id.callout().clone() else {
            panic!("the item by its ID: {:?}", by_id.callout());
        };
        expect_item(&Request::Fetch(request), "file-2");
        let Answer::Fetch(answer) = ok(&uploaded_again) else {
            unreachable!()
        };
        by_id.answer(answer);
        // The host keeps the bytes of the first answer under their own MD5,
        // so the request of that MD5 downloads nothing.
        let again = next_callout(&harness).await;
        let Callout::FetchBlob(asked) = again.callout().clone() else {
            panic!("the bytes are a blob fetch: {:?}", again.callout());
        };
        assert_eq!(asked.md5.as_deref(), Some(md5("new")));
        assert!(harness.runtime.keeps_test_blob(&asked));
        harness.runtime.answer_test_blob(again, 200, b"new").await;
    });
    assert_eq!(content.unwrap(), "new");
    harness.begin_sync();
    let (listing, requests) = answering(
        &harness,
        names(&harness, &at("files")),
        [ok(&changes(&[changed(&uploaded_again)], "changes-2"))],
    )
    .await;
    listing.unwrap();
    expect_changes(&requests[0], "changes-1");
    assert_eq!(text(&harness, "files/file-2/content").await, "new");
    let read: Value =
        serde_json::from_str(&text(&harness, "files/file-2/file.json").await).unwrap();
    assert_eq!(read, uploaded_again);
}

/// The changes at a read of `files/` once in a sync, from the
/// token of the answer before: a new file, a file with other content, whose
/// content the next read takes again, a file with another name alone, whose
/// content stays, the removal of an item that the account no longer has, of
/// one in the trash, of each item of a shared drive that the account no
/// longer has, and of an item whose change without its object Google Drive
/// has not by its ID, the item by its ID of a change without its object, an
/// item whose object has no name, and a file without its MD5. A change of
/// an item of one kind gives the own feed of no other kind a state. A last
/// page without the token of the changes after it is the error of the read,
/// and leaves the token as it was.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_changes_take_the_items_after_the_token_of_the_answer_before() {
    let harness = drive_harness();
    let notes = uploaded("file-1", "notes.md", "notes");
    let kept = uploaded("file-2", "kept.md", "kept");
    let plan = google("doc-1", "document", "Plan", EDITED);
    first_full_listing(
        &harness,
        &[
            notes.clone(),
            kept.clone(),
            uploaded("file-3", "shared.md", "shared"),
            uploaded("file-4", "old.md", "old"),
            uploaded("file-5", "raw.bin", "busy"),
            uploaded("file-7", "unknown.md", "gone"),
            uploaded("file-8", "broken.md", "bytes"),
            uploaded("file-9", "found.md", "new"),
            in_drive(uploaded("team-1", "team.md", "shared"), "drive-1"),
            in_drive(google("team-2", "folder", "Team", EDITED), "drive-1"),
            in_drive(uploaded("team-3", "other.md", "shared"), "drive-2"),
            plan.clone(),
            google("folder-1", "folder", "Work", EDITED),
            google("form-1", "form", "Survey", EDITED),
        ],
    )
    .await;
    for (path, body) in [
        ("files/file-1/content", "notes"),
        ("files/file-2/content", "kept"),
    ] {
        let (content, _) = reading(&harness, path, [blob(200, body.as_bytes())]).await;
        assert_eq!(content.unwrap(), body);
    }
    let (content, _) = reading(&harness, "files/doc-1/content", [blob(200, b"# Plan")]).await;
    assert_eq!(content.unwrap(), "# Plan");

    let edited = uploaded("file-1", "notes.md", "notes!");
    let plan_edited = google("doc-1", "document", "Plan", EDITED_LATER);
    let headless = without(uploaded("file-5", "raw.bin", "busy"), "md5Checksum");
    let found = uploaded("file-9", "found again.md", "new");
    let sync = harness.begin_sync();
    let (listing, requests) = answering(
        &harness,
        names(&harness, &at("files")),
        [
            ok(&json!({
                "changes": [
                    changed(&edited),
                    json!({ "changeType": "file", "removed": false }),
                    changed(&uploaded("file-2", "renamed.md", "kept")),
                    changed(&without(uploaded("file-10", "odd.md", "new"), "trashed")),
                    removed("file-3"),
                ],
                "nextPageToken": "changes-1b",
            })),
            ok(&changes(
                &[
                    changed(&trashed(uploaded("file-4", "old.md", "old"))),
                    changed(&uploaded("file-6", "new.md", "new")),
                    removed("form-1"),
                    changed(&google("form-2", "form", "Poll", EDITED)),
                    changed(&plan_edited),
                    removed("folder-1"),
                    changed(&google("folder-2", "folder", "Home", EDITED)),
                    json!({ "changeType": "drive", "driveId": "drive-3", "removed": false }),
                    json!({ "changeType": "drive", "driveId": "drive-1", "removed": true }),
                    changed(&headless),
                    json!({ "changeType": "file", "fileId": "file-7", "removed": false }),
                    json!({ "changeType": "file", "fileId": "file-9", "removed": false }),
                    changed(&without(uploaded("file-8", "broken.md", "bytes"), "name")),
                ],
                "changes-2",
            )),
            failure(404, "notFound"),
            ok(&found),
        ],
    )
    .await;
    // A change without what names it is the error of the answer, after the
    // facts of the other changes.
    assert!(matches!(listing, Err(NsError::Invalid)), "{listing:?}");
    expect_changes(&requests[0], "changes-1");
    expect_changes(&requests[1], "changes-1b");
    expect_item(&requests[2], "file-7");
    expect_item(&requests[3], "file-9");
    // The changes are asked once in a sync, and not outside one.
    let files_now = sorted(&[
        "file-1", "file-2", "file-5", "file-6", "file-8", "file-9", "team-3", "doc-1",
    ]);
    assert_eq!(listed(&harness, "files").await, files_now);
    harness.end_sync(sync);
    assert_eq!(listed(&harness, "files").await, files_now);
    assert_eq!(listed(&harness, "folders").await, ["folder-2"]);
    assert_eq!(listed(&harness, "others").await, ["form-2"]);

    // The own feed of a folder has no state from the change of a file: the
    // lookup of the file as a folder asks Google Drive by its ID.
    let (lookup_as_folder, requests) = answering(
        &harness,
        lookup(&harness, &at("folders/file-6")),
        [ok(&uploaded("file-6", "new.md", "new"))],
    )
    .await;
    assert!(lookup_as_folder.unwrap().is_missing());
    expect_item(&requests[0], "file-6");

    assert_eq!(text(&harness, "files/file-2/name").await, "renamed.md");
    assert_eq!(text(&harness, "files/file-2/content").await, "kept");
    assert_eq!(text(&harness, "files/file-9/name").await, "found again.md");
    let read: Value =
        serde_json::from_str(&text(&harness, "files/file-5/file.json").await).unwrap();
    assert_eq!(read, headless);
    let (content, requests) =
        reading(&harness, "files/file-1/content", [blob(200, b"notes!")]).await;
    assert_eq!(content.unwrap(), "notes!");
    expect_bytes(&requests[0], "file-1", md5("notes!"));
    let (content, requests) = reading(
        &harness,
        "files/doc-1/content",
        [blob(200, b"# Plan, edited")],
    )
    .await;
    assert_eq!(content.unwrap(), "# Plan, edited");
    expect_export(&requests[0], "doc-1", "text/markdown", EDITED_LATER);

    // A last page without the token of the changes after it.
    harness.begin_sync();
    let (ended, requests) = answering(
        &harness,
        names(&harness, &at("files")),
        [ok(&json!({ "changes": [] }))],
    )
    .await;
    assert!(matches!(ended, Err(NsError::Invalid)), "{ended:?}");
    expect_changes(&requests[0], "changes-2");

    // The next changes come from the token after the last page.
    harness.begin_sync();
    let (_, requests) = answering(
        &harness,
        names(&harness, &at("files")),
        [ok(&changes(&[], "changes-3"))],
    )
    .await;
    expect_changes(&requests[0], "changes-2");
}

/// A token of the changes that Google Drive refuses with `400` gives the read
/// its error and begins a full listing again at the next read, after which
/// the items that it lacks leave the mount, with the content of the others
/// as it was, but for a file with other content, and an item of another
/// kind is of its kind alone. An item by its ID leaves with them, and comes
/// back at its next lookup with its content as the host keeps it. A page of
/// that listing whose token it refuses with `400` begins the listing again,
/// and one for which it searched not all of its corpus is the error of the
/// read. An own answer that publishes during the listing leaves it to
/// continue. Another failing status of the changes is the error of the read
/// alone.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn a_refused_token_of_the_changes_begins_a_full_listing_again() {
    let harness = drive_harness();
    let notes = uploaded("file-1", "notes.md", "notes");
    let draft = uploaded("file-4", "draft.md", "old");
    first_full_listing(
        &harness,
        &[
            notes.clone(),
            uploaded("file-2", "gone.md", "gone"),
            draft.clone(),
            uploaded("file-5", "later.md", "kept"),
            google("folder-1", "folder", "Work", EDITED),
            google("folder-2", "folder", "Gone", EDITED),
            google("form-1", "form", "Gone", EDITED),
            google("form-2", "form", "Poll", EDITED),
            google("form-3", "form", "Now a file", EDITED),
        ],
    )
    .await;
    for (path, body) in [
        ("files/file-1/content", "notes"),
        ("files/file-4/content", "old"),
    ] {
        let (content, _) = reading(&harness, path, [blob(200, body.as_bytes())]).await;
        assert_eq!(content.unwrap(), body);
    }
    // An export by its ID.
    let nine = google("doc-9", "document", "Nine", EDITED);
    let (content, requests) = reading(
        &harness,
        "files/doc-9/content",
        [ok(&nine), blob(200, b"# Nine")],
    )
    .await;
    assert_eq!(content.unwrap(), "# Nine");
    expect_item(&requests[0], "doc-9");
    let Request::Blob(export) = requests[1].clone() else {
        panic!("an export is a blob fetch");
    };

    // The answer of the content of a file begins before the full listing,
    // and publishes while the listing asks its next page.
    let later = served(&harness, lookup(&harness, &at("files/file-5/content")))
        .await
        .unwrap()
        .path;
    let (published, mut ended) = tokio::sync::oneshot::channel();
    let (later, ()) = tokio::join!(
        async {
            let later = harness.namespace.read(later, 0, 9).await;
            published.send(()).unwrap();
            later
        },
        async {
            let held = next_callout(&harness).await;

            // A status other than `400` leaves the token of the changes as
            // it was.
            harness.begin_sync();
            let (denied, requests) = answering(
                &harness,
                names(&harness, &at("files")),
                [failure(403, "insufficientPermissions")],
            )
            .await;
            assert!(matches!(denied, Err(NsError::Permission)), "{denied:?}");
            expect_changes(&requests[0], "changes-1");
            harness.begin_sync();

            let (refused, requests) = answering(
                &harness,
                names(&harness, &at("files")),
                [failure(400, "invalid")],
            )
            .await;
            assert!(matches!(refused, Err(NsError::Invalid)), "{refused:?}");
            expect_changes(&requests[0], "changes-1");

            // The second page of the full listing is refused for its token,
            // and the listing begins again from the token of the changes
            // after now.
            let first = files(std::slice::from_ref(&notes), Some("page-2"));
            harness.begin_sync();
            let (refused, requests) = answering(
                &harness,
                names(&harness, &at("files")),
                [
                    ok(&json!({ "startPageToken": "changes-5" })),
                    ok(&first),
                    failure(400, "invalid"),
                ],
            )
            .await;
            assert!(matches!(refused, Err(NsError::Invalid)), "{refused:?}");
            expect_start(&requests[0]);
            expect_listing(&requests[1], Corpus::User, None);
            expect_listing(&requests[2], Corpus::User, Some("page-2"));

            // A page for which Google Drive searched not all of its corpus
            // ends no listing.
            let mut incomplete = files(&[], None);
            incomplete["incompleteSearch"] = true.into();
            harness.begin_sync();
            let (stopped, requests) = answering(
                &harness,
                names(&harness, &at("files")),
                [
                    ok(&json!({ "startPageToken": "changes-6" })),
                    ok(&first),
                    ok(&incomplete),
                ],
            )
            .await;
            assert!(matches!(stopped, Err(NsError::Invalid)), "{stopped:?}");
            expect_start(&requests[0]);
            expect_listing(&requests[1], Corpus::User, None);
            expect_listing(&requests[2], Corpus::User, Some("page-2"));

            harness.begin_sync();
            let files_path = at("files");
            let (listing, ()) = tokio::join!(names(&harness, &files_path), async {
                let page = next_callout(&harness).await;
                let Callout::Fetch(request) = page.callout() else {
                    panic!("{:?}", page.callout());
                };
                assert!(request.url.contains("pageToken=page-2"), "{}", request.url);
                harness.runtime.answer_test_blob(held, 200, b"kept").await;
                (&mut ended).await.unwrap();
                let Answer::Fetch(answer) = ok(&files(
                    &[
                        uploaded("file-3", "new.md", "new"),
                        uploaded("file-4", "draft.md", "new"),
                        uploaded("file-5", "later.md", "kept"),
                        uploaded("form-3", "form.csv", "kept"),
                        without(google("anon-2", "form", "Anon", EDITED), "id"),
                        google("folder-1", "folder", "Work", EDITED),
                        google("form-2", "form", "Poll", EDITED),
                    ],
                    None,
                )) else {
                    unreachable!()
                };
                page.answer(answer);
                let drives_page = next_callout(&harness).await;
                let Answer::Fetch(answer) = ok(&drives(&[], None)) else {
                    unreachable!()
                };
                drives_page.answer(answer);
            });
            assert!(matches!(listing, Err(NsError::Invalid)), "{listing:?}");
            assert_eq!(
                listed(&harness, "files").await,
                sorted(&["file-1", "file-3", "file-4", "file-5", "form-3"])
            );
        }
    );
    assert_eq!(later.unwrap().bytes, b"kept");
    assert_eq!(text(&harness, "files/file-1/content").await, "notes");
    assert_eq!(text(&harness, "files/file-5/content").await, "kept");
    assert_eq!(listed(&harness, "folders").await, ["folder-1"]);
    assert_eq!(listed(&harness, "others").await, ["form-2"]);
    for path in [
        "files/file-2",
        "folders/folder-2",
        "others/form-1",
        "others/form-3",
        "files/doc-9",
    ] {
        assert!(missing(&harness, path).await, "{path}");
    }
    let (content, requests) = reading(&harness, "files/file-4/content", [blob(200, b"new")]).await;
    assert_eq!(content.unwrap(), "new");
    expect_bytes(&requests[0], "file-4", md5("new"));

    // The export by its ID comes back at its next lookup in a sync,
    // and the host keeps the answer of its request of its content.
    harness.begin_sync();
    let (content, requests) = reading(
        &harness,
        "files/doc-9/content",
        [
            ok(&changes(&[], "changes-7")),
            ok(&nine),
            blob(200, b"# Nine"),
        ],
    )
    .await;
    assert_eq!(content.unwrap(), "# Nine");
    expect_changes(&requests[0], "changes-6");
    expect_item(&requests[1], "doc-9");
    let Request::Blob(again) = &requests[2] else {
        panic!("an export is a blob fetch");
    };
    assert_eq!(again.url, export.url);
    assert_eq!(again.version, export.version);
    assert!(harness.runtime.keeps_test_blob(&export));
}

/// A file, a folder, and another item by their IDs: found with their facts,
/// and missing for `404`, in the trash, or of another kind. In a sync,
/// each operation on such an item, which no listing has, takes it
/// again by its ID: its lookup, the listing of its folder, the read of its
/// name, of its object, and of its content, which comes again for another
/// revision alone, and the item leaves for `404` or in the trash. A full
/// listing leaves an item that it lacks and the mount has by its ID, and
/// takes an item that it brings from then on. The answer of a file by its
/// ID from before the listing gives way to the listing. A rate limit
/// without a `Retry-After` holds no other item back.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn a_file_a_folder_and_another_item_by_their_ids() {
    let harness = drive_harness();
    let notes = uploaded("file-1", "notes.md", "notes");
    let (found, requests) = reading(&harness, "files/file-1/name", [ok(&notes)]).await;
    assert_eq!(found.unwrap(), "notes.md");
    expect_item(&requests[0], "file-1");
    let (content, requests) =
        reading(&harness, "files/file-1/content", [blob(200, b"notes")]).await;
    assert_eq!(content.unwrap(), "notes");
    expect_bytes(&requests[0], "file-1", md5("notes"));
    for (path, expected) in [
        ("files/file-1/name", "notes.md"),
        ("files/file-1/content", "notes"),
    ] {
        assert_eq!(text(&harness, path).await, expected, "{path}");
    }

    for (path, object, expected) in [
        (
            "files/file-5/name",
            uploaded("file-5", "shared.md", "shared"),
            "shared.md",
        ),
        (
            "files/file-6/name",
            uploaded("file-6", "gone.md", "gone"),
            "gone.md",
        ),
        (
            "others/form-1/name",
            google("form-1", "form", "Survey", EDITED),
            "Survey",
        ),
        (
            "folders/folder-1/name",
            google("folder-1", "folder", "Work", EDITED),
            "Work",
        ),
    ] {
        let (found, requests) = reading(&harness, path, [ok(&object)]).await;
        assert_eq!(found.unwrap(), expected, "{path}");
        expect_item(&requests[0], object["id"].as_str().unwrap());
    }

    for (path, id, answer) in [
        ("files/file-2", "file-2", failure(404, "notFound")),
        (
            "files/file-3",
            "file-3",
            ok(&trashed(uploaded("file-3", "old.md", "old"))),
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
        ("folders/file-1", "file-1", ok(&notes)),
    ] {
        let (found, requests) = answering(&harness, lookup(&harness, &at(path)), [answer]).await;
        assert!(found.unwrap().is_missing(), "{path}");
        expect_item(&requests[0], id);
    }

    // In a sync, an operation on the file takes it by its ID, once.
    let edited = uploaded("file-1", "edited.md", "notes!");
    harness.begin_sync();
    let (found, requests) = answering(
        &harness,
        lookup(&harness, &at("files/file-1")),
        [ok(&notes)],
    )
    .await;
    assert!(!found.unwrap().is_missing());
    expect_item(&requests[0], "file-1");
    assert_eq!(text(&harness, "files/file-1/name").await, "notes.md");
    harness.begin_sync();
    let (listing, requests) =
        answering(&harness, names(&harness, &at("files/file-1")), [ok(&notes)]).await;

    assert_eq!(listing.unwrap(), ["content", "file.json", "name"]);
    expect_item(&requests[0], "file-1");
    harness.begin_sync();
    let (object, requests) = reading(&harness, "files/file-1/file.json", [ok(&notes)]).await;
    let object: Value = serde_json::from_str(&object.unwrap()).unwrap();
    assert_eq!(object, notes);
    expect_item(&requests[0], "file-1");
    // The content of the same revision comes not again.
    harness.begin_sync();
    let (content, requests) = reading(&harness, "files/file-1/content", [ok(&notes)]).await;
    assert_eq!(content.unwrap(), "notes");
    expect_item(&requests[0], "file-1");
    // Another name and other content.
    harness.begin_sync();
    let (_, requests) = reading(&harness, "files/file-1/name", [ok(&edited)]).await;
    expect_item(&requests[0], "file-1");
    assert_eq!(text(&harness, "files/file-1/name").await, "edited.md");
    let (content, requests) =
        reading(&harness, "files/file-1/content", [blob(200, b"notes!")]).await;
    assert_eq!(content.unwrap(), "notes!");
    expect_bytes(&requests[0], "file-1", md5("notes!"));

    harness.begin_sync();
    // The answer of the file by its ID from before a full listing with its
    // other name comes after the listing, and the name is of the listing.
    let listed_file = uploaded("file-1", "listed.md", "notes!");
    let name_path = at("files/file-1/name");
    let (found, listing) = tokio::join!(read(&harness, &name_path), async {
        let by_id = next_callout(&harness).await;
        let (listing, requests) = answering(
            &harness,
            names(&harness, &at("files")),
            [
                ok(&json!({ "startPageToken": "changes-1" })),
                ok(&files(
                    &[listed_file.clone(), uploaded("file-9", "new.md", "new")],
                    None,
                )),
                ok(&drives(&[], None)),
            ],
        )
        .await;
        expect_start(&requests[0]);
        expect_listing(&requests[1], Corpus::User, None);
        let Answer::Fetch(answer) = ok(&edited) else {
            unreachable!()
        };
        by_id.answer(answer);
        listing
    });
    assert_eq!(found.unwrap(), "listed.md");
    // The items by their IDs alone stay beside the items of the listing.
    assert_eq!(
        listing.unwrap(),
        sorted(&["file-1", "file-5", "file-6", "file-9"])
    );
    assert_eq!(listed(&harness, "folders").await, ["folder-1"]);
    assert_eq!(listed(&harness, "others").await, ["form-1"]);

    // After the listing, the changes bring nothing of an item by its ID
    // alone: an operation in a sync takes it again, with its other
    // name, or its removal.
    harness.begin_sync();
    let (_, requests) = reading(
        &harness,
        "files/file-5/name",
        [ok(&uploaded("file-5", "moved.md", "shared"))],
    )
    .await;
    expect_item(&requests[0], "file-5");
    assert_eq!(text(&harness, "files/file-5/name").await, "moved.md");
    for (path, id, answer) in [
        ("files/file-6/name", "file-6", failure(404, "notFound")),
        (
            "folders/folder-1/name",
            "folder-1",
            failure(404, "notFound"),
        ),
        (
            "others/form-1/name",
            "form-1",
            ok(&trashed(google("form-1", "form", "Survey", EDITED))),
        ),
    ] {
        let (gone, requests) = answering(&harness, lookup(&harness, &at(path)), [answer]).await;
        assert!(gone.unwrap().is_missing(), "{path}");
        expect_item(&requests[0], id);
    }
    let (listing, requests) = answering(
        &harness,
        names(&harness, &at("files")),
        [ok(&changes(&[], "changes-2"))],
    )
    .await;
    expect_changes(&requests[0], "changes-1");
    assert_eq!(listing.unwrap(), sorted(&["file-1", "file-5", "file-9"]));
    for kind in ["folders", "others"] {
        assert!(listed(&harness, kind).await.is_empty(), "{kind}");
    }

    // A `429` without a `Retry-After` opens no window: in the same sync,
    // another item by its ID is asked once.
    let harness = drive_harness();
    let survey = google("form-1", "form", "Survey", EDITED);
    let (found, _) = reading(&harness, "others/form-1/name", [ok(&survey)]).await;
    assert_eq!(found.unwrap(), "Survey");
    harness.begin_sync();
    let (limited, requests) = reading(
        &harness,
        "files/file-5/content",
        [
            ok(&uploaded("file-5", "shared.md", "shared")),
            blob(429, b"{}"),
        ],
    )
    .await;
    assert!(
        matches!(limited, Err(NsError::RateLimited { .. })),
        "{limited:?}"
    );
    expect_bytes(&requests[1], "file-5", md5("shared"));
    let (found, _) = reading(&harness, "others/form-1/name", [ok(&survey)]).await;
    assert_eq!(found.unwrap(), "Survey");
    assert_eq!(text(&harness, "others/form-1/name").await, "Survey");
}

/// The content that Google Drive refuses is the error of its read, by the
/// reason of an inline request of it, of the first byte of the bytes of a
/// file, as that of a rate limit for a request over a quota, or the refusal
/// as it came for a first byte that it gives, and the next read asks for it
/// again; an export that the inline request gives is the content. The
/// content of a file that Google Drive no longer has removes the file. A
/// credential that Google Drive refuses needs the credential of the mount.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn a_refused_content_is_the_error_of_its_read_and_a_missing_one_removes_its_file() {
    let harness = drive_harness();
    let sheet = google("sheet-1", "spreadsheet", "Large", EDITED);
    first_full_listing(
        &harness,
        &[
            uploaded("file-1", "restricted.pdf", "bytes"),
            uploaded("file-2", "gone.md", "gone"),
            uploaded("file-3", "busy.bin", "busy"),
            uploaded("file-4", "lost.md", "old"),
            uploaded("file-5", "empty.md", ""),
            uploaded("file-6", "kept.md", "kept"),
            sheet.clone(),
            google("slides-1", "presentation", "Late", EDITED),
            google("doc-1", "document", "Gone", EDITED),
            google("doc-2", "document", "Lost", EDITED),
        ],
    )
    .await;

    // The refusal stands for the rest of the sync: the next read has it
    // without a request, and the next sync asks again, after its changes.
    let (refused, requests) = reading(
        &harness,
        "files/file-1/content",
        [blob(403, b"{}"), failure(403, "cannotDownloadFile")],
    )
    .await;
    assert!(matches!(refused, Err(NsError::Permission)), "{refused:?}");
    expect_bytes(&requests[0], "file-1", md5("bytes"));
    expect_first_byte(&requests[1], "file-1");
    let held = served(&harness, read(&harness, &at("files/file-1/content"))).await;
    assert!(matches!(held, Err(NsError::Permission)), "{held:?}");
    harness.begin_sync();
    let (content, requests) = reading(
        &harness,
        "files/file-1/content",
        [ok(&changes(&[], "changes-1")), blob(200, b"bytes")],
    )
    .await;
    expect_changes(&requests[0], "changes-1");
    assert_eq!(content.unwrap(), "bytes");

    // The refusal as it came for a file whose first byte Google Drive gives,
    // and for one without a first byte.
    for (id, first_byte) in [("file-6", bytes(206, b"k")), ("file-5", bytes(416, b"{}"))] {
        let (refused, requests) = reading(
            &harness,
            &format!("files/{id}/content"),
            [blob(403, b"{}"), first_byte],
        )
        .await;
        assert!(matches!(refused, Err(NsError::Permission)), "{refused:?}");
        expect_first_byte(&requests[1], id);
    }

    let (refused, requests) = reading(
        &harness,
        "files/sheet-1/content",
        [blob(403, b"{}"), failure(403, "exportSizeLimitExceeded")],
    )
    .await;
    assert!(matches!(refused, Err(NsError::Permission)), "{refused:?}");
    expect_export(&requests[0], "sheet-1", "text/csv", EDITED);
    expect_export_refusal(&requests[1], "sheet-1", "text/csv");
    assert_eq!(text(&harness, "files/sheet-1/name").await, "Large");
    // The export that the inline request gives.
    let (content, _) = reading(
        &harness,
        "files/slides-1/content",
        [blob(403, b"{}"), bytes(200, b"Late")],
    )
    .await;
    assert_eq!(content.unwrap(), "Late");
    assert_eq!(text(&harness, "files/slides-1/content").await, "Late");

    // The file leaves for `404` of its bytes, of their first byte, of its
    // export, and of the inline request of its export.
    for (path, answers) in [
        ("files/file-2/content", vec![blob(404, b"{}")]),
        (
            "files/file-4/content",
            vec![blob(403, b"{}"), failure(404, "notFound")],
        ),
        ("files/doc-1/content", vec![blob(404, b"{}")]),
        (
            "files/doc-2/content",
            vec![blob(403, b"{}"), failure(404, "notFound")],
        ),
    ] {
        let (missing_content, _) = reading(&harness, path, answers).await;
        assert!(
            matches!(missing_content, Err(NsError::NotFound)),
            "{path}: {missing_content:?}"
        );
    }
    assert_eq!(
        listed(&harness, "files").await,
        sorted(&[
            "file-1", "file-3", "file-5", "file-6", "sheet-1", "slides-1"
        ])
    );

    // A refusal over a quota without a `Retry-After` states no wait: the
    // next read of another file asks for it.
    let (limited, requests) = reading(
        &harness,
        "files/file-3/content",
        [blob(403, b"{}"), failure(403, "userRateLimitExceeded")],
    )
    .await;
    assert!(
        matches!(limited, Err(NsError::RateLimited { .. })),
        "{limited:?}"
    );
    expect_first_byte(&requests[1], "file-3");
    // The refused content is asked again at the next sync alone.
    let held = served(&harness, read(&harness, &at("files/file-3/content"))).await;
    assert!(matches!(held, Err(NsError::RateLimited { .. })), "{held:?}");
    // The export of sheet 1, refused before in this sync, is asked again in
    // the next one.
    harness.begin_sync();
    let (asked, requests) = reading(
        &harness,
        "files/sheet-1/content",
        [
            ok(&changes(&[], "changes-1")),
            blob(403, b"{}"),
            failure(403, "exportSizeLimitExceeded"),
        ],
    )
    .await;
    assert!(matches!(asked, Err(NsError::Permission)), "{asked:?}");
    expect_changes(&requests[0], "changes-1");
    expect_export(&requests[1], "sheet-1", "text/csv", EDITED);
    harness.begin_sync();
    let (content, requests) = reading(
        &harness,
        "files/file-3/content",
        [ok(&changes(&[], "changes-2")), blob(200, b"busy")],
    )
    .await;
    assert_eq!(content.unwrap(), "busy");
    expect_changes(&requests[0], "changes-1");
    expect_bytes(&requests[1], "file-3", md5("busy"));

    // A download that Google Drive redirects to a host of no domain of the
    // mount: the read has the refusal of the host, after one request of the
    // first byte, which Google Drive answers with the redirect.
    let harness = drive_harness();
    first_full_listing(&harness, &[uploaded("far-1", "far.bin", "new")]).await;
    let content_path = at("files/far-1/content");
    let (refused, ()) = tokio::join!(read(&harness, &content_path), async {
        let download = next_callout(&harness).await;
        let Callout::FetchBlob(asked) = download.callout().clone() else {
            panic!("the bytes are a blob fetch: {:?}", download.callout());
        };
        expect_bytes(&Request::Blob(asked), "far-1", md5("new"));
        download.answer(CalloutResult::CalloutError(CalloutError {
            kind: ErrorKind::Denied,
            message: "the redirect to far.example.com is to no domain of the mount".to_owned(),
            retryable: false,
        }));
        let probe = next_callout(&harness).await;
        let Callout::Fetch(request) = probe.callout().clone() else {
            panic!("the first byte is an inline request: {:?}", probe.callout());
        };
        expect_first_byte(&Request::Fetch(request), "far-1");
        let Answer::Fetch(answer) = bytes(302, b"") else {
            unreachable!()
        };
        probe.answer(answer);
    });
    assert!(matches!(refused, Err(NsError::Permission)), "{refused:?}");

    // A credential that Google Drive refuses.
    let harness = drive_harness();
    first_full_listing(&harness, &[google("doc-3", "document", "Locked", EDITED)]).await;
    let (refused, _) = reading(&harness, "files/doc-3/content", [blob(401, b"{}")]).await;
    assert!(matches!(refused, Err(NsError::AuthRequired)), "{refused:?}");
}

/// A `403` of a request over a quota, by each of its reasons, and a `429`
/// give a read the error of a rate limit: of the token of the changes, of
/// the export of a file of Google, and of the bytes of an uploaded file.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_over_a_quota_is_a_rate_limit() {
    for reason in ["userRateLimitExceeded", "rateLimitExceeded"] {
        let harness = drive_harness();
        let (limited, requests) = answering(
            &harness,
            names(&harness, &at("files")),
            [failure(403, reason)],
        )
        .await;
        assert!(
            matches!(limited, Err(NsError::RateLimited { .. })),
            "{reason}: {limited:?}"
        );
        expect_start(&requests[0]);
    }

    let notes = uploaded("file-1", "notes.md", "notes");
    let document = google("doc-1", "document", "Plan", EDITED);
    for (path, answers) in [
        ("files/file-1/content", vec![blob(429, b"{}")]),
        (
            "files/doc-1/content",
            vec![blob(403, b"{}"), failure(403, "rateLimitExceeded")],
        ),
    ] {
        let harness = drive_harness();
        first_full_listing(&harness, &[notes.clone(), document.clone()]).await;
        let (limited, _) = reading(&harness, path, answers).await;
        assert!(
            matches!(limited, Err(NsError::RateLimited { .. })),
            "{path}: {limited:?}"
        );
    }
}

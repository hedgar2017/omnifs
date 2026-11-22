#![cfg(not(target_os = "wasi"))]

//! The requests of the mount `google-drive` to Google Drive: the full
//! listings and the changes of the feed of the Drive, and the items and the
//! content of the files of their own feeds.

mod support;

use omnifs_engine::{Namespace, NsError, NsEvent};
use omnifs_itest::RuntimeHarness;
use omnifs_wit::host::types::HttpRequest;
use serde_json::{Value, json};
use support::{
    Answer, PAST_DEADLINE, answering, blob, bytes, changed, changes, drive_harness, expect_bytes,
    expect_changes, expect_export, expect_first_byte, expect_item, expect_listing, expect_start,
    failure, files, google, lookup, md5, names, next_callout, ok, read, read_range, removed,
    served, sized, trashed, uploaded,
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

/// `object` without `field`.
fn without(mut object: Value, field: &str) -> Value {
    object.as_object_mut().unwrap().remove(field);
    object
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

/// A read of the `content` of the file of `object` without it in the
/// projection: the file by its ID, answered with `object`, then the request
/// of its content, answered with `answers`.
async fn content(
    harness: &RuntimeHarness,
    object: &Value,
    answers: Vec<Answer>,
) -> (Result<String, NsError>, Vec<HttpRequest>) {
    let id = object["id"].as_str().unwrap();
    let mut all = vec![ok(object)];
    all.extend(answers);
    let (content, mut requests) =
        answering(harness, read(harness, &format!("files/{id}/content")), all).await;
    expect_item(&requests.remove(0), id);
    (content, requests)
}

/// The first full listing at the first read of `files/`: the token of the
/// changes after now, then the pages of the items by `nextPageToken`, with
/// the uploaded files of any size and the files of Google with an export as
/// files, the folders as folders, the other items and an uploaded item
/// without an MD5 as others, no item in the trash, and no item whose object
/// lacks a field, whose lookup has the error of its object. A page that
/// Google Drive refuses with `400` for its token begins the listing again
/// with the token of the changes of its start.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_first_full_listing_takes_the_items_by_next_page_token() {
    let harness = drive_harness();
    let notes = uploaded("file-1", "notes.md", "notes");
    let document = google("doc-1", "document", "Plan", EDITED);
    let folder = google("folder-1", "folder", "Work", EDITED);
    let bare = without(uploaded("bare-1", "link.url", ""), "md5Checksum");
    let nameless = without(uploaded("broken-1", "broken.md", "old"), "name");
    let first = files(
        &[
            notes.clone(),
            nameless.clone(),
            document.clone(),
            folder.clone(),
            google("form-1", "form", "Survey", EDITED),
            bare.clone(),
        ],
        Some("page-2"),
    );
    let (refused, requests) = answering(
        &harness,
        names(&harness, "files"),
        vec![
            ok(&json!({ "startPageToken": "changes-1" })),
            ok(&first),
            failure(400, "invalid"),
        ],
    )
    .await;
    assert!(matches!(refused, Err(NsError::Invalid)), "{refused:?}");
    expect_start(&requests[0]);
    expect_listing(&requests[1], None);
    expect_listing(&requests[2], Some("page-2"));

    let (listing, requests) = answering(
        &harness,
        names(&harness, "files"),
        vec![
            ok(&first),
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
    expect_listing(&requests[0], None);
    expect_listing(&requests[1], Some("page-2"));

    assert_eq!(
        served(&harness, names(&harness, "folders")).await.unwrap(),
        ["folder-1"]
    );
    assert_eq!(
        served(&harness, names(&harness, "others")).await.unwrap(),
        sorted(&["form-1", "shortcut-1", "bare-1"])
    );
    assert_eq!(
        served(&harness, names(&harness, "others/form-1"))
            .await
            .unwrap(),
        ["item.json", "name"]
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
        ("others/form-1/name", "Survey"),
        ("others/bare-1/name", "link.url"),
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
        ("others/bare-1/item.json", &bare),
    ] {
        let read: Value =
            serde_json::from_str(&served(&harness, read(&harness, path)).await.unwrap()).unwrap();
        assert_eq!(&read, object, "{path}");
    }

    // The item whose object lacks its name is none of the mount, and its
    // lookup past the deadline has the error of its object by its ID. The
    // changes come from the token of the start of the listing.
    for kind in ["files", "folders", "others"] {
        let path = format!("{kind}/broken-1");
        let found = served(&harness, lookup(&harness, &path)).await;
        assert!(found.unwrap().is_missing(), "{path}");
    }
    tokio::time::sleep(PAST_DEADLINE).await;
    let (found, requests) = answering(
        &harness,
        lookup(&harness, "files/broken-1"),
        vec![ok(&changes(&[], "changes-2")), ok(&nameless)],
    )
    .await;
    assert!(matches!(found, Err(NsError::Invalid)), "{found:?}");
    expect_changes(&requests[0], "changes-1");
    expect_item(&requests[1], "broken-1");
}

/// The content of a file at the first read of its `content`, at what Google
/// Drive has of the file by its ID then: the bytes of an uploaded file with
/// its MD5, which the host holds and serves a range of, of a file above the
/// bytes that the host holds in memory too, and the Markdown of a document,
/// the CSV of a spreadsheet, the plain text of a presentation, the SVG of a
/// drawing, and the JSON of a project of Apps Script, each served after it
/// without a request. The bytes of a file uploaded again before its change
/// come once, with their MD5.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_content_takes_the_bytes_of_an_uploaded_file_and_the_export_of_a_file_of_google() {
    let harness = drive_harness();
    let notes = uploaded("file-1", "notes.md", "notes");
    let large = sized("large-1", "recording.mp4", LARGE_MD5, LARGE as u64);
    first_full_listing(
        &harness,
        &[
            notes.clone(),
            uploaded("file-2", "draft.md", "old"),
            large.clone(),
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
    let (read_content, requests) = content(&harness, &notes, vec![blob(200, b"notes")]).await;
    assert_eq!(read_content.unwrap(), "notes");
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

    let end = LARGE as u64 - 3;
    let (range, requests) = answering(
        &harness,
        read_range(&harness, "files/large-1/content", end, 9),
        vec![ok(&large), blob(200, &vec![b'x'; LARGE])],
    )
    .await;
    assert_eq!(range.unwrap(), ("xxx".to_owned(), true));
    expect_item(&requests[0], "large-1");
    expect_bytes(&requests[1], "large-1", LARGE_MD5);
    assert_eq!(
        served(
            &harness,
            read_range(&harness, "files/large-1/content", 0, 4)
        )
        .await
        .unwrap(),
        ("xxxx".to_owned(), false)
    );

    for (id, kind, mime_type, export) in [
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
    ] {
        let object = google(id, kind, "Plan", EDITED);
        let (read_content, requests) =
            content(&harness, &object, vec![bytes(200, export.as_bytes())]).await;
        assert_eq!(read_content.unwrap(), export, "{id}");
        expect_export(&requests[0], id, mime_type);
        assert_eq!(
            served(&harness, read(&harness, &format!("files/{id}/content")))
                .await
                .unwrap(),
            export,
            "{id}"
        );
    }

    // The file is uploaded again before its change: its content comes with
    // the MD5 of the file by its ID, and stays at the change of that MD5.
    let uploaded_again = uploaded("file-2", "draft.md", "new");
    let (read_content, requests) =
        content(&harness, &uploaded_again, vec![blob(200, b"new")]).await;
    assert_eq!(read_content.unwrap(), "new");
    expect_bytes(&requests[0], "file-2", md5("new"));
    tokio::time::sleep(PAST_DEADLINE).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, "files"),
        vec![ok(&changes(&[changed(&uploaded_again)], "changes-2"))],
    )
    .await;
    listing.unwrap();
    expect_changes(&requests[0], "changes-1");
    assert_eq!(
        served(&harness, read(&harness, "files/file-2/content"))
            .await
            .unwrap(),
        "new"
    );
    let read: Value = serde_json::from_str(
        &served(&harness, read(&harness, "files/file-2/file.json"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(read, uploaded_again);
}

/// The changes at a read of `files/` past the deadline of the feed, from the
/// token of the answer before: a new file, a file with other content, whose
/// content the next read takes again, a file with another name alone, whose
/// content stays, the removal of an item that the account no longer has, of
/// one in the trash, of one whose change has no object, and of one whose
/// object lacks a field, and an uploaded file without its MD5 as another
/// item. A last page without the token of the changes after it is the error
/// of the read, and leaves the token as it was.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_changes_take_the_files_after_the_token_of_the_answer_before() {
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
            uploaded("file-5", "link.url", "busy"),
            uploaded("file-7", "unknown.md", "gone"),
            uploaded("file-8", "broken.md", "bytes"),
            plan.clone(),
            google("folder-1", "folder", "Work", EDITED),
            google("form-1", "form", "Survey", EDITED),
        ],
    )
    .await;
    for (object, body) in [(&notes, "notes"), (&kept, "kept")] {
        let (read_content, _) = content(&harness, object, vec![blob(200, body.as_bytes())]).await;
        assert_eq!(read_content.unwrap(), body);
    }
    let (read_content, _) = content(&harness, &plan, vec![bytes(200, b"# Plan")]).await;
    assert_eq!(read_content.unwrap(), "# Plan");

    let edited = uploaded("file-1", "notes.md", "notes!");
    let plan_edited = google("doc-1", "document", "Plan", EDITED_LATER);
    tokio::time::sleep(PAST_DEADLINE).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, "files"),
        vec![
            ok(&json!({
                "changes": [
                    changed(&edited),
                    changed(&uploaded("file-2", "renamed.md", "kept")),
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
                    json!({ "changeType": "drive", "driveId": "drive-1", "removed": false }),
                    changed(&without(
                        uploaded("file-5", "link.url", "busy"),
                        "md5Checksum",
                    )),
                    json!({ "changeType": "file", "fileId": "file-7", "removed": false }),
                    changed(&without(uploaded("file-8", "broken.md", "bytes"), "name")),
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
        sorted(&["form-2", "file-5"])
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
    let (read_content, requests) = content(&harness, &edited, vec![blob(200, b"notes!")]).await;
    assert_eq!(read_content.unwrap(), "notes!");
    expect_bytes(&requests[0], "file-1", md5("notes!"));
    let (read_content, requests) =
        content(&harness, &plan_edited, vec![bytes(200, b"# Plan, edited")]).await;
    assert_eq!(read_content.unwrap(), "# Plan, edited");
    expect_export(&requests[0], "doc-1", "text/markdown");

    // A last page without the token of the changes after it.
    tokio::time::sleep(PAST_DEADLINE).await;
    let (ended, requests) = answering(
        &harness,
        names(&harness, "files"),
        vec![ok(&json!({ "changes": [] }))],
    )
    .await;
    assert!(matches!(ended, Err(NsError::Invalid)), "{ended:?}");
    expect_changes(&requests[0], "changes-2");

    // The next changes come from the token after the last page.
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
/// the items that it lacks leave the mount, with the content of the others
/// as it was, but for a file with other content; a page of that listing
/// whose token it refuses with `400` begins the listing again, and one for
/// which it searched not every drive is the error of the read. Another
/// failing status of the changes is the error of the read alone.
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
            google("folder-1", "folder", "Work", EDITED),
            google("folder-2", "folder", "Gone", EDITED),
            google("form-1", "form", "Gone", EDITED),
            google("form-2", "form", "Poll", EDITED),
        ],
    )
    .await;
    for (object, body) in [(&notes, "notes"), (&draft, "old")] {
        let (read_content, _) = content(&harness, object, vec![blob(200, body.as_bytes())]).await;
        assert_eq!(read_content.unwrap(), body);
    }

    // A status other than `400` leaves the token of the changes as it was.
    tokio::time::sleep(PAST_DEADLINE).await;
    let (denied, requests) = answering(
        &harness,
        names(&harness, "files"),
        vec![failure(403, "insufficientPermissions")],
    )
    .await;
    assert!(matches!(denied, Err(NsError::Permission)), "{denied:?}");
    expect_changes(&requests[0], "changes-1");

    let (refused, requests) = answering(
        &harness,
        names(&harness, "files"),
        vec![failure(400, "invalid")],
    )
    .await;
    assert!(matches!(refused, Err(NsError::Invalid)), "{refused:?}");
    expect_changes(&requests[0], "changes-1");

    // The second page of the full listing is refused for its token, and the
    // listing begins again from the token of the changes after now.
    let first = files(std::slice::from_ref(&notes), Some("page-2"));
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
    assert!(matches!(refused, Err(NsError::Invalid)), "{refused:?}");
    expect_start(&requests[0]);
    expect_listing(&requests[1], None);
    expect_listing(&requests[2], Some("page-2"));

    // A page for which Google Drive searched not every drive ends no listing.
    let mut incomplete = files(&[], None);
    incomplete["incompleteSearch"] = true.into();
    let (stopped, requests) = answering(
        &harness,
        names(&harness, "files"),
        vec![
            ok(&json!({ "startPageToken": "changes-6" })),
            ok(&first),
            ok(&incomplete),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Invalid)), "{stopped:?}");
    expect_start(&requests[0]);
    expect_listing(&requests[1], None);
    expect_listing(&requests[2], Some("page-2"));

    let redrafted = uploaded("file-4", "draft.md", "new");
    let (listing, requests) = answering(
        &harness,
        names(&harness, "files"),
        vec![ok(&files(
            &[
                uploaded("file-3", "new.md", "new"),
                redrafted.clone(),
                google("folder-1", "folder", "Work", EDITED),
                google("form-2", "form", "Poll", EDITED),
            ],
            None,
        ))],
    )
    .await;
    assert_eq!(listing.unwrap(), sorted(&["file-1", "file-3", "file-4"]));
    expect_listing(&requests[0], Some("page-2"));
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
    assert_eq!(
        served(&harness, names(&harness, "others")).await.unwrap(),
        ["form-2"]
    );
    for path in ["files/file-2", "folders/folder-2", "others/form-1"] {
        let found = served(&harness, lookup(&harness, path)).await;
        assert!(found.unwrap().is_missing(), "{path}");
    }
    let (read_content, requests) = content(&harness, &redrafted, vec![blob(200, b"new")]).await;
    assert_eq!(read_content.unwrap(), "new");
    expect_bytes(&requests[0], "file-4", md5("new"));

    tokio::time::sleep(PAST_DEADLINE).await;
    let (_, requests) = answering(
        &harness,
        names(&harness, "files"),
        vec![ok(&changes(&[], "changes-7"))],
    )
    .await;
    expect_changes(&requests[0], "changes-6");
}

/// A file, a folder, and another item by their IDs: found with their facts,
/// which a read past their time takes again, and missing for `404`, in the
/// trash, or of another kind. A full listing leaves an item that it lacks
/// and the mount has by its ID, whose changes and whose removal the next
/// reads past the time of its facts take, and the answer of a file by its ID
/// from before the listing gives way to the listing.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn a_file_a_folder_and_another_item_by_their_ids() {
    let harness = drive_harness();
    let notes = uploaded("file-1", "notes.md", "notes");
    let (found, requests) = answering(
        &harness,
        read(&harness, "files/file-1/name"),
        vec![ok(&notes)],
    )
    .await;
    assert_eq!(found.unwrap(), "notes.md");
    expect_item(&requests[0], "file-1");
    let (read_content, requests) = content(&harness, &notes, vec![blob(200, b"notes")]).await;
    assert_eq!(read_content.unwrap(), "notes");
    expect_bytes(&requests[0], "file-1", md5("notes"));
    for (path, expected) in [
        ("files/file-1/name", "notes.md"),
        ("files/file-1/content", "notes"),
    ] {
        assert_eq!(
            served(&harness, read(&harness, path)).await.unwrap(),
            expected,
            "{path}"
        );
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
        let (found, requests) = answering(&harness, read(&harness, path), vec![ok(&object)]).await;
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
        let (found, requests) = answering(&harness, lookup(&harness, path), vec![answer]).await;
        assert!(found.unwrap().is_missing(), "{path}");
        expect_item(&requests[0], id);
    }

    // Past the time of its facts, a read takes the file again: its other
    // name, and its other content at the next read of it.
    let edited = uploaded("file-1", "edited.md", "notes!");
    tokio::time::sleep(PAST_DEADLINE).await;
    let (found, requests) = answering(
        &harness,
        read(&harness, "files/file-1/name"),
        vec![ok(&edited)],
    )
    .await;
    assert_eq!(found.unwrap(), "edited.md");
    expect_item(&requests[0], "file-1");
    let (read_content, requests) = content(&harness, &edited, vec![blob(200, b"notes!")]).await;
    assert_eq!(read_content.unwrap(), "notes!");
    expect_bytes(&requests[0], "file-1", md5("notes!"));

    // The answer of the file by its ID from before a full listing with its
    // other name comes after the listing, and the name is of the listing.
    let listed = uploaded("file-1", "listed.md", "notes!");
    tokio::time::sleep(PAST_DEADLINE).await;
    let (found, listing) = tokio::join!(read(&harness, "files/file-1/name"), async {
        let by_id = next_callout(&harness).await;
        let (listing, requests) = answering(
            &harness,
            names(&harness, "files"),
            vec![
                ok(&json!({ "startPageToken": "changes-1" })),
                ok(&files(
                    &[listed.clone(), uploaded("file-9", "new.md", "new")],
                    None,
                )),
            ],
        )
        .await;
        expect_start(&requests[0]);
        expect_listing(&requests[1], None);
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
    assert_eq!(
        served(&harness, names(&harness, "folders")).await.unwrap(),
        ["folder-1"]
    );
    assert_eq!(
        served(&harness, names(&harness, "others")).await.unwrap(),
        ["form-1"]
    );
    assert_eq!(
        served(&harness, read(&harness, "files/file-1/content"))
            .await
            .unwrap(),
        "notes!"
    );

    // After the listing, the changes bring nothing of an item by its ID
    // alone: a read past the time of its facts takes it again, with its
    // other name, or its removal.
    tokio::time::sleep(PAST_DEADLINE).await;
    let (found, requests) = answering(
        &harness,
        read(&harness, "files/file-5/name"),
        vec![
            ok(&changes(&[], "changes-2")),
            ok(&uploaded("file-5", "moved.md", "shared")),
        ],
    )
    .await;
    // The read gives the name whose size its lookup told, and the next one,
    // after its lookup, the name that it took.
    assert_eq!(found.unwrap(), "shared.md");
    expect_changes(&requests[0], "changes-1");
    expect_item(&requests[1], "file-5");
    assert_eq!(
        served(&harness, read(&harness, "files/file-5/name"))
            .await
            .unwrap(),
        "moved.md"
    );
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
        let (gone, requests) = answering(&harness, read(&harness, path), vec![answer]).await;
        assert!(matches!(gone, Err(NsError::NotFound)), "{path}: {gone:?}");
        expect_item(&requests[0], id);
        let found = served(&harness, lookup(&harness, path)).await;
        assert!(found.unwrap().is_missing(), "{path}");
    }
    assert_eq!(
        served(&harness, names(&harness, "files")).await.unwrap(),
        sorted(&["file-1", "file-5", "file-9"])
    );
    for kind in ["folders", "others"] {
        let listing = served(&harness, names(&harness, kind)).await;
        assert!(listing.unwrap().is_empty(), "{kind}");
    }
}

/// The content that Google Drive refuses is the error of its read, by the
/// reason in the answer to its first byte, as that of a rate limit for a
/// request over a quota, or the refusal as it came for a first byte that it
/// gives, and the next read asks for it again; the content of a file that
/// Google Drive no longer has removes the file.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn a_refused_content_is_the_error_of_its_read_and_a_missing_one_removes_its_file() {
    let harness = drive_harness();
    let restricted = uploaded("file-1", "restricted.pdf", "bytes");
    let gone = uploaded("file-2", "gone.md", "gone");
    let busy = uploaded("file-3", "busy.bin", "busy");
    let lost = uploaded("file-4", "lost.md", "old");
    let empty = uploaded("file-5", "empty.md", "");
    let kept = uploaded("file-6", "kept.md", "kept");
    let sheet = google("sheet-1", "spreadsheet", "Large", EDITED);
    let document = google("doc-1", "document", "Gone", EDITED);
    first_full_listing(
        &harness,
        &[
            restricted.clone(),
            gone.clone(),
            busy.clone(),
            lost.clone(),
            empty.clone(),
            kept.clone(),
            uploaded("file-7", "removed.md", "new"),
            sheet.clone(),
            document.clone(),
        ],
    )
    .await;

    for _ in 0..2 {
        let (refused, requests) = content(
            &harness,
            &restricted,
            vec![blob(403, b"{}"), failure(403, "cannotDownloadFile")],
        )
        .await;
        assert!(matches!(refused, Err(NsError::Permission)), "{refused:?}");
        expect_bytes(&requests[0], "file-1", md5("bytes"));
        expect_first_byte(&requests[1], "file-1");
    }
    let (read_content, _) = content(&harness, &restricted, vec![blob(200, b"bytes")]).await;
    assert_eq!(read_content.unwrap(), "bytes");

    // The refusal as it came for a file whose first byte Google Drive gives,
    // and for one without a first byte.
    for (object, first_byte) in [(&kept, bytes(206, b"k")), (&empty, bytes(416, b"{}"))] {
        let (refused, requests) =
            content(&harness, object, vec![blob(403, b"{}"), first_byte]).await;
        assert!(matches!(refused, Err(NsError::Permission)), "{refused:?}");
        expect_first_byte(&requests[1], object["id"].as_str().unwrap());
    }

    let (refused, requests) = content(
        &harness,
        &sheet,
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

    // The file leaves for `404` of its bytes, of their first byte, of its
    // export, and of the file by its ID.
    for (object, answers) in [
        (&gone, vec![blob(404, b"{}")]),
        (&lost, vec![blob(403, b"{}"), failure(404, "notFound")]),
        (&document, vec![failure(404, "notFound")]),
    ] {
        let (missing, _) = content(&harness, object, answers).await;
        assert!(matches!(missing, Err(NsError::NotFound)), "{missing:?}");
    }
    let (missing, requests) = answering(
        &harness,
        read(&harness, "files/file-7/content"),
        vec![failure(404, "notFound")],
    )
    .await;
    assert!(matches!(missing, Err(NsError::NotFound)), "{missing:?}");
    expect_item(&requests[0], "file-7");
    assert_eq!(
        served(&harness, names(&harness, "files")).await.unwrap(),
        sorted(&["file-1", "file-3", "file-5", "file-6", "sheet-1"])
    );

    // A refusal over a quota holds the next requests of the mount back.
    let (limited, requests) = content(
        &harness,
        &busy,
        vec![blob(403, b"{}"), failure(403, "userRateLimitExceeded")],
    )
    .await;
    assert!(
        matches!(limited, Err(NsError::RateLimited { .. })),
        "{limited:?}"
    );
    expect_first_byte(&requests[1], "file-3");
    let held = served(&harness, read(&harness, "files/sheet-1/content")).await;
    assert!(matches!(held, Err(NsError::RateLimited { .. })), "{held:?}");
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
            names(&harness, "files"),
            vec![failure(403, reason)],
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
    for (object, answer) in [
        (&notes, blob(429, b"{}")),
        (&document, failure(403, "rateLimitExceeded")),
    ] {
        let harness = drive_harness();
        first_full_listing(&harness, &[notes.clone(), document.clone()]).await;
        let (limited, _) = content(&harness, object, vec![answer]).await;
        assert!(
            matches!(limited, Err(NsError::RateLimited { .. })),
            "{limited:?}"
        );
    }
}

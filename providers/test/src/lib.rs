//! Test-provider: host guest-loader fixture on the object-shaped SDK surface.
//!
//! Synthetic canned data exercises the object SDK end to end: object canonical,
//! representation, and computed faces; a child-object `comments` collection with a
//! typed page cursor; scoped intent-tagged invalidation from `on_tick`;
//! object-load prefetch via `preload_object`; an object alias; deferred, ranged,
//! and live files; object tree handoffs, one with an unknown handle and one of
//! a Git remote at the commit its upstream answers; paged and partial
//! listings, with and without page validators; an object feed with fed faces;
//! rate limits; and object aliases.
//!
//! It is the conformance fixture every host integration test drives, so every
//! pinned behavior is preserved on the new surface: the root, hello, and items
//! listings; the ranged contracts; negative lookup; and feed
//! pagination.

#![allow(clippy::needless_pass_by_value)]
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use core::time::Duration;

use omnifs_sdk::handler::BoxFuture;
use omnifs_sdk::object::Load;
use omnifs_sdk::prelude::*;

#[derive(Clone)]
#[config]
struct Config {
    /// Expose a provider-backed mount-root .gitignore for FUSE regression tests.
    #[serde(default)]
    root_ignore: bool,
}

#[derive(Clone, Default)]
struct State {
    fresh_full_reads: u64,
    /// Calls of the `hello/validated` listing, which `hello/validated-calls`
    /// reads.
    validated_calls: u64,
    /// Whether a revalidation of the first page of `hello/validated` has
    /// answered `unchanged`; the next one answers a changed page.
    validated_first_page_stood: bool,
    /// Each call of a shelf or book feed as `object token start`, and each
    /// load of a print as `print load`, which `shelf-log` reads.
    shelf_log: Vec<String>,
    /// The upstream version of the shelves, which a read of `shelf-change`
    /// advances.
    shelf_version: u64,
    /// Whether the feed of the `flaky` shelf has stopped once.
    flaky_stopped: bool,
    /// Whether a later answer of the feed of the `wobbly` shelf has stopped
    /// once.
    wobbly_stopped: bool,
}

const LARGE_RANGED_SIZE: u64 = 64 * 1024 * 1024 + 1;

#[omnifs_sdk::path_captures]
struct DynamicCaptures {
    name: String,
}

#[omnifs_sdk::path_captures]
struct SlowCaptures {
    ms: u64,
}

// ===========================================================================
// Object family: `Item`
// ===========================================================================

#[omnifs_sdk::path_segment]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[strum(serialize_all = "snake_case")]
enum StateFilter {
    Open,
    All,
}

#[omnifs_sdk::object(kind = "test.item", key = ItemKey)]
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Item {
    number: u64,
    title: String,
    body: Option<String>,
    state: String,
}

impl Item {
    /// Object SDK computed signature: `fn(&Item, &ItemKey) -> Result<FileProjection>`.
    fn title(&self, _key: &ItemKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.title.as_bytes(), TextFormat::Raw).build())
    }

    fn state(&self, _key: &ItemKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.state.as_bytes(), TextFormat::Raw).build())
    }

    fn body(&self, _key: &ItemKey) -> Result<FileProjection> {
        Ok(FileProjection::inline(self.body.as_deref().unwrap_or(""))
            .content_type(ContentType::Markdown)
            .build())
    }

    /// A live ranged leaf under the item anchor.
    /// It opens through `open-file`, exercising the object Stream dispatch the
    /// conformance fixture must cover. Unlike `hello/ranged` (`LiveTailReader`,
    /// probed only at fixed offsets), a recursive walk (`grep -r`) opens every
    /// leaf under the item anchor including this one and reads it to
    /// completion, so its reader must actually reach EOF (`ItemLogReader`)
    /// rather than fabricate bytes forever.
    fn log(
        _cx: Cx<State>,
        _key: ItemKey,
    ) -> impl core::future::Future<Output = Result<StreamFile>> {
        core::future::ready(Ok(StreamFile::new(ItemLogReader).live()))
    }

    /// Returns an intentionally unknown handle so host validation can exercise
    /// the retained object-directory tree-handoff boundary.
    fn unknown_tree(
        _cx: Cx<State>,
        _key: ItemKey,
    ) -> impl core::future::Future<Output = Result<Tree>> {
        core::future::ready(Ok(Tree::at(
            TreeRef::new(777),
            "0123456789abcdef0123456789abcdef01234567",
        )))
    }

    /// The hand-written load `#[object]` forwards `Object::load` to. Item 404 is
    /// absent (negative lookup); a matching `since` validator is `Unchanged`
    /// (conditional load); otherwise fresh, and loading item 7 also preloads its
    /// sibling item 8 from the same payload (object-load prefetch).
    #[allow(clippy::unused_async)]
    async fn load(cx: &Cx<State>, key: &ItemKey, since: Option<Validator>) -> Result<Load<Self>> {
        if key.number == 404 {
            return Ok(Load::NotFound);
        }
        if key.number == 9 {
            return load_remote_item(cx, key, since).await;
        }
        let (value, canonical) = canned_item(key.number)?;
        if since.is_some() && since == canonical.validator {
            return Ok(Load::Unchanged);
        }
        let mut load = Load::fresh(value, canonical);
        if key.number == 7 {
            // Same-type sibling preload (Oura-shaped): one fetch materializes a
            // neighboring item's canonical at its own anchor.
            let (_, sibling_canonical) = canned_item(8)?;
            load = load.preload_object(ObjectEntry::fresh(
                ItemKey {
                    filter: Facet::<StateFilter>(key.filter.0),
                    number: 8,
                },
                sibling_canonical,
            ));
        }
        Ok(load)
    }
}

async fn load_remote_item(
    cx: &Cx<State>,
    key: &ItemKey,
    since: Option<Validator>,
) -> Result<Load<Item>> {
    let mut request = cx
        .http()
        .get(format!("https://httpbin.org/anything/items/{}", key.number));
    if let Some(validator) = since.as_ref() {
        request = request.header("if-none-match", validator.as_str());
    }
    let response = request.send().await?;
    match response.status().as_u16() {
        200 => {
            let validator = response
                .headers()
                .get("etag")
                .and_then(|value| value.to_str().ok())
                .map(Validator::from);
            let bytes = response.into_body();
            let value = Item::decode(&bytes)?;
            Ok(Load::fresh(value, Canonical::new(bytes, validator)))
        },
        304 => Ok(Load::Unchanged),
        404 => Ok(Load::NotFound),
        status => Err(ProviderError::network(format!(
            "remote item returned status {status}"
        ))),
    }
}

impl Representable<Markdown> for Item {
    fn represent(&self) -> Vec<u8> {
        format!(
            "# {}\n\n{}\n",
            self.title,
            self.body.as_deref().unwrap_or("")
        )
        .into_bytes()
    }
}

/// Canned `(value, canonical)` for one item number. Canonical bytes are the raw
/// upstream body and are filter-independent so `open/N` and `all/N` share
/// identity.
fn canned_item(number: u64) -> Result<(Item, Canonical)> {
    let bytes = format!(
        r#"{{"number":{number},"title":"Item {number}","body":"Body {number}","state":"open"}}"#
    )
    .into_bytes();
    let value = Item::decode(&bytes)?;
    let validator = Some(Validator::from(format!("item-{number}-v1")));
    Ok((value, Canonical::new(bytes, validator)))
}

#[omnifs_sdk::path_captures]
struct ItemKey {
    #[allow(dead_code)] // route facet; identity uses `number` only
    filter: Facet<StateFilter>,
    number: u64,
}

/// The `/items/{filter}` collection-dir key: carries the `{filter}` facet so the
/// list method lists the same items under either filter (identity collapse).
#[omnifs_sdk::path_captures]
struct ItemListKey {
    filter: Facet<StateFilter>,
}

/// The `/items/{filter}` listing. `items` is a synthetic dir with no parent
/// object, so this is a raw handler (not a typed object-dir collection): it
/// lists items 7 and 8 and stores each item's canonical plus its eager computed
/// leaves at listing time, so a later read of any item leaf serves warm
/// (collection preload). The typed `Collection`/`Cursor` surface is exercised by
/// the object-dir `comments` collection.
async fn item_list(_cx: DirCx<State>, key: ItemListKey) -> Result<DirListing> {
    let filter = key.filter.0.to_string();
    let rows = [7_u64, 8]
        .into_iter()
        .map(|number| canned_item(number).map(|(item, canonical)| (number, item, canonical)))
        .collect::<Result<Vec<_>>>()?;

    let mut projection = DirListing::exhaustive(
        rows.iter()
            .map(|(number, _, _)| Entry::dir(number.to_string())),
    );

    for (number, item, canonical) in rows {
        let item_key = ItemKey {
            filter: Facet::<StateFilter>(key.filter.0),
            number,
        };
        let base = format!("items/{filter}/{number}");
        // The canonical-view leaves are the canonical/representation/computed
        // file leaves only. `comments` is a collection DIR, not a view of the
        // item canonical, so it must not appear here.
        let leaves = ["item.json", "item.md", "title", "state", "body"]
            .into_iter()
            .map(|leaf| format!("/{base}/{leaf}"))
            .collect();
        projection = projection.store_canonical(
            item_key.anchor(Item::kind()),
            canonical.validator.clone(),
            canonical.bytes,
            leaves,
        );
        projection = projection.preload_file(
            format!("{base}/title"),
            FileProjection::inline(item.title.as_bytes().to_vec()).build(),
        );
        projection = projection.preload_file(
            format!("{base}/state"),
            FileProjection::inline(item.state.as_bytes().to_vec()).build(),
        );
    }

    Ok(projection)
}

// ===========================================================================
// Object family: `Comment` (child-object collection under an item)
// ===========================================================================

#[omnifs_sdk::object(kind = "test.comment", key = ItemCommentKey)]
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Comment {
    idx: u64,
    author: String,
    body: String,
}

impl Comment {
    fn author(&self, _key: &ItemCommentKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.author.as_bytes(), TextFormat::Raw).build())
    }

    fn load(
        _cx: &Cx<State>,
        key: &ItemCommentKey,
        _since: Option<Validator>,
    ) -> impl core::future::Future<Output = Result<Load<Self>>> {
        core::future::ready(if key.idx == 0 {
            Err(ProviderError::invalid_input("comment index is 1-based"))
        } else {
            canned_comment(key.idx).map(|(value, canonical)| Load::fresh(value, canonical))
        })
    }
}

impl Representable<Markdown> for Comment {
    fn represent(&self) -> Vec<u8> {
        format!("**{}**\n\n{}\n", self.author, self.body).into_bytes()
    }
}

fn canned_comment(idx: u64) -> Result<(Comment, Canonical)> {
    let bytes = format!(r#"{{"idx":{idx},"author":"commenter-{idx}","body":"comment {idx}"}}"#)
        .into_bytes();
    let value = Comment::decode(&bytes)?;
    let validator = Some(Validator::from(format!("comment-{idx}-v1")));
    Ok((value, Canonical::new(bytes, validator)))
}

#[omnifs_sdk::path_captures]
struct ItemCommentKey {
    #[allow(dead_code)]
    filter: Facet<StateFilter>,
    #[allow(dead_code)]
    number: u64,
    idx: u64,
}

/// The `/items/{filter}/{number}/comments` collection-dir key, carrying the
/// `{filter}` and `{number}` captures the list method needs.
#[omnifs_sdk::path_captures]
struct ItemCommentsListKey {
    filter: Facet<StateFilter>,
    number: u64,
}

/// The `comments` collection: paged child `Comment` objects. Page 0 lists
/// comment 1 with a resume cursor (typed `Page`); the resumed page lists
/// comment 2 as `Partial` (open, no cursor). Exercises typed Collection/Cursor
/// page + partial + fresh child canonical.
#[allow(clippy::unused_async)]
async fn item_comments(
    key: ItemCommentsListKey,
    cx: ListCx<PageCursor, State>,
) -> Result<Collection<Comment, PageCursor>> {
    let page = cx.cursor().map_or(0, |c| c.0);
    let idx = page + 1;
    let (value, canonical) = canned_comment(idx)?;
    let entry = CollectionEntry::fresh(
        ItemCommentKey {
            filter: Facet::<StateFilter>(key.filter.0),
            number: key.number,
            idx,
        },
        value,
        canonical,
    );
    if page == 0 {
        Ok(Collection::page([entry]).next(PageCursor(1)))
    } else {
        Ok(Collection::partial([entry]))
    }
}

// ===========================================================================
// Object family: `Shelf` and `Book`, fed by the feed of the shelf
// ===========================================================================

#[omnifs_sdk::object(kind = "test.shelf", key = ShelfKey)]
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Shelf {
    name: String,
}

impl Shelf {
    fn load(
        _cx: &Cx<State>,
        key: &ShelfKey,
        _since: Option<Validator>,
    ) -> impl core::future::Future<Output = Result<Load<Self>>> {
        let bytes = format!(r#"{{"name":"{}"}}"#, key.shelf).into_bytes();
        core::future::ready(
            Self::decode(&bytes).map(|shelf| Load::fresh(shelf, Canonical::new(bytes, None))),
        )
    }

    /// The feed of a shelf: its first answer lists books 1 and 2 in two
    /// parts, each with the inner token of its book, the first with card 1
    /// and with the canonical bytes of print 1 too, and a third part
    /// completes `books`; the `flaky` shelf stops once before its second
    /// part. After the first answer, an answer is `unchanged` until the
    /// shelf version moves, and then renames book 1, adds book 3, writes the
    /// canonical bytes of print 1 at that version, and records
    /// in `kept` the books that the kept parts of the answer wrote and
    /// removed at its last part, in `seen` the names the host listed for
    /// `books` and in `listed` the
    /// completeness, the time and the count of that listing in a part, and removes
    /// book 2 in its last part. The last part of the first answer writes
    /// `catalog`, unsettled for the `dated` shelf, and the part of a change
    /// records in `catalog-read` what it reads of the fed files; the `wobbly`
    /// shelf stops once at a rate limit of a second before that last part.
    fn feed(cx: FeedCx<ShelfKey, State>) -> impl core::future::Future<Output = Result<Feed>> {
        core::future::ready(Self::answer(&cx))
    }

    #[allow(clippy::too_many_lines)]
    fn answer(cx: &FeedCx<ShelfKey, State>) -> Result<Feed> {
        let shelf = cx.key().shelf.clone();
        let token = cx.token::<ShelfToken>()?;
        let version = cx.state_mut(|state| {
            state.shelf_log.push(format!(
                "{shelf} {} {}",
                token.map_or_else(|| "-".to_owned(), |token| token.to_string()),
                asked(cx.start(), cx.due())
            ));
            state.shelf_version
        });
        if shelf == "refused" {
            // The upstream of the `refused` shelf refuses each request.
            return Err(ProviderError::denied("the shelf is refused"));
        }
        let books = format!("/shelves/{shelf}/books");
        let book = |number: u64, title: &str| -> Result<()> {
            cx.dir(format!("{books}/{number}"), ListingWrite::Partial)?;
            cx.file(format!("{books}/{number}/title"), title.as_bytes().to_vec())
        };
        let key = |number: u64| BookKey {
            shelf: shelf.clone(),
            book: number,
        };
        if shelf == "faulty" && token.is_none() {
            // The first answer of the `faulty` shelf ends with an error of
            // an item that its upstream gave without what names it, after
            // the facts of book 1.
            cx.dir(&books, ListingWrite::Complete)?;
            book(1, "Book 1")?;
            return Ok(Feed::part(&ShelfToken::Version(version))?
                .error(ProviderError::invalid_input("an item without an id"))
                .into());
        }
        match token {
            None => {
                let cards = format!("/shelves/{shelf}/cards");
                cx.dir(&cards, ListingWrite::Complete)?;
                cx.dir(format!("{cards}/1"), ListingWrite::Complete)?;
                cx.file(format!("{cards}/1/text"), b"Card 1".to_vec())?;
                Self::print(cx, &shelf, version)?;
                cx.dir(&books, ListingWrite::Partial)?;
                book(1, "Book 1")?;
                Ok(Feed::part(&ShelfToken::Page(1))?
                    .more(true)
                    .inner::<Book>(&key(1), &BookToken::Row(1))?
                    .into())
            },
            Some(ShelfToken::Page(1)) => {
                if shelf == "flaky"
                    && !cx.state_mut(|state| std::mem::replace(&mut state.flaky_stopped, true))
                {
                    return Err(ProviderError::network("the shelf upstream stopped"));
                }
                cx.dir(&books, ListingWrite::Partial)?;
                book(2, "Book 2")?;
                Ok(Feed::part(&ShelfToken::Page(2))?
                    .more(true)
                    .inner::<Book>(&key(2), &BookToken::Row(2))?
                    .into())
            },
            Some(ShelfToken::Page(_)) => {
                cx.dir(&books, ListingWrite::Complete)?;
                cx.file(format!("/shelves/{shelf}/catalog"), catalog())?;
                let part = Feed::part(&ShelfToken::Version(version))?;
                Ok(if shelf == "dated" {
                    part.unsettled()
                } else {
                    part
                }
                .into())
            },
            Some(ShelfToken::Version(done)) if done == version => Ok(Feed::unchanged()),
            Some(ShelfToken::Change(change)) if change == version => {
                if shelf == "wobbly"
                    && !cx.state_mut(|state| std::mem::replace(&mut state.wobbly_stopped, true))
                {
                    return Err(ProviderError::rate_limited("the shelf upstream is busy")
                        .with_retry_after(Some(core::time::Duration::from_secs(1))));
                }
                let kept = cx.kept::<u64>(&books)?;
                cx.file(
                    format!("/shelves/{shelf}/kept"),
                    format!("written {:?} removed {:?}", kept.written, kept.removed).into_bytes(),
                )?;
                cx.dir(&books, ListingWrite::Complete)?;
                cx.remove(format!("{books}/2"))?;
                let part = Feed::part(&ShelfToken::Version(version))?;
                // The `fickle` shelf declines the part that the host kept.
                Ok(if shelf == "fickle" {
                    part.anew().into()
                } else {
                    part.into()
                })
            },
            Some(ShelfToken::Version(_) | ShelfToken::Change(_)) => {
                Self::listing_read(cx, &shelf, &books)?;
                cx.file(
                    format!("/shelves/{shelf}/catalog-read"),
                    Self::catalog_read(cx, &shelf)?.into_bytes(),
                )?;
                Self::print(cx, &shelf, version)?;
                cx.dir(&books, ListingWrite::Complete)?;
                book(1, &format!("Book 1 v{version}"))?;
                book(3, "Book 3")?;
                Ok(Feed::part(&ShelfToken::Change(version))?.more(true).into())
            },
        }
    }
}

impl Shelf {
    /// Print 1 of the shelf at `version`, with its canonical bytes as the
    /// feed has them.
    fn print(cx: &FeedCx<ShelfKey, State>, shelf: &str, version: u64) -> Result<()> {
        let prints = format!("/shelves/{shelf}/prints");
        cx.dir(&prints, ListingWrite::Complete)?;
        cx.dir(format!("{prints}/1"), ListingWrite::Complete)?;
        cx.canonical::<Print>(
            format!("{prints}/1"),
            Canonical::new(Print::bytes(1, version), None),
        );
        Ok(())
    }

    /// Write what a feed reads of the listing of `books` in the projection:
    /// its names, as numbers, in `seen`, and its completeness, its time, and
    /// its count in `listed`.
    fn listing_read(cx: &FeedCx<ShelfKey, State>, shelf: &str, books: &str) -> Result<()> {
        let (Some(seen), Some(listed)) = (cx.names::<u64>(books)?, cx.count(books)?) else {
            return Err(ProviderError::internal(format!("no listing of {books}")));
        };
        let names = seen
            .names
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        cx.file(format!("/shelves/{shelf}/seen"), names.into_bytes())?;
        cx.file(
            format!("/shelves/{shelf}/listed"),
            format!(
                "names {} {:?}, count {} {} {:?}",
                seen.complete, seen.unsettled, listed.count, listed.complete, listed.unsettled,
            )
            .into_bytes(),
        )
    }

    /// What a feed reads of the fed files of the projection: the catalog by
    /// its ranges of [`CATALOG_RANGE`] bytes, the catalog as a record of one
    /// range, which it is too large for, and the title of book 1 as a
    /// record.
    fn catalog_read(cx: &FeedCx<ShelfKey, State>, shelf: &str) -> Result<String> {
        let at = format!("/shelves/{shelf}/catalog");
        let mut read = Vec::new();
        let mut ranges = 0;
        let (length, until) = loop {
            let offset = u64::try_from(read.len())
                .map_err(|error| ProviderError::internal(error.to_string()))?;
            let Some(range) = cx.read_range(&at, offset, CATALOG_RANGE)? else {
                return Err(ProviderError::internal(format!("no fed file at {at}")));
            };
            ranges += 1;
            let ended = range.bytes.len() < CATALOG_RANGE as usize;
            read.extend(range.bytes);
            if ended {
                break (range.length, range.unsettled);
            }
        };
        let record = cx.read_record(&at, CATALOG_RANGE).map_or_else(
            |error| format!("{:?}", error.kind()),
            |_| "a record".to_owned(),
        );
        let title = cx
            .read_record(format!("/shelves/{shelf}/books/1/title"), CATALOG_RANGE)?
            .map(|record| String::from_utf8_lossy(&record.bytes).into_owned());
        Ok(format!(
            "{length} bytes in {ranges} ranges, {}, unsettled {until}; as a record: {record}; title {title:?}",
            if read == catalog() { "same" } else { "other" },
        ))
    }
}

/// The bytes of a range that the shelf feed reads of its catalog.
const CATALOG_RANGE: u32 = 64 * 1024;

/// The catalog of a shelf: a fed file of more than two ranges.
fn catalog() -> Vec<u8> {
    (0..150_000_u32).map(|at| (at % 251) as u8).collect()
}

/// The token of the feed of a shelf: the page of its first answer that a
/// part brought, the version of the shelves that an answer ended at, or the
/// version that the first part of a change brought.
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
enum ShelfToken {
    Page(u8),
    Version(u64),
    Change(u64),
}

impl core::fmt::Display for ShelfToken {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Page(page) => write!(f, "page-{page}"),
            Self::Version(version) => write!(f, "version-{version}"),
            Self::Change(version) => write!(f, "change-{version}"),
        }
    }
}

/// The token of the feed of a book: the row of its shelf that brought it, or
/// its own answer.
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
enum BookToken {
    Row(u64),
    Own,
}

impl core::fmt::Display for BookToken {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Row(row) => write!(f, "row-{row}"),
            Self::Own => f.write_str("own"),
        }
    }
}

#[omnifs_sdk::path_captures]
struct ShelfKey {
    shelf: String,
}

/// A print of a shelf, whose canonical bytes the feed of its shelf writes:
/// its faces render from those bytes without a load.
#[omnifs_sdk::object(kind = "test.print", key = PrintKey)]
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Print {
    print: u64,
    version: u64,
}

impl Print {
    /// The canonical bytes of the print `print` at `version`.
    fn bytes(print: u64, version: u64) -> Vec<u8> {
        format!(r#"{{"print":{print},"version":{version}}}"#).into_bytes()
    }

    /// The load of a print, which the log of the feeds records: the print
    /// of an upstream at the version 1000, which no feed writes.
    fn load(
        cx: &Cx<State>,
        key: &PrintKey,
        _since: Option<Validator>,
    ) -> impl core::future::Future<Output = Result<Load<Self>>> {
        cx.state_mut(|state| {
            state
                .shelf_log
                .push(format!("{}/prints/{} load", key.shelf, key.print));
        });
        let bytes = Self::bytes(key.print, 1000);
        core::future::ready(
            Self::decode(&bytes).map(|print| Load::fresh(print, Canonical::new(bytes, None))),
        )
    }
}

impl Representable<Markdown> for Print {
    fn represent(&self) -> Vec<u8> {
        format!("# Print {} v{}\n", self.print, self.version).into_bytes()
    }
}

#[omnifs_sdk::path_captures]
struct PrintKey {
    shelf: String,
    print: u64,
}

/// A desk, whose feed lists note 1 alone: a note that the desk does not list
/// is an item of its own feed alone.
#[omnifs_sdk::object(kind = "test.desk", key = DeskKey)]
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Desk {
    desk: String,
}

impl Desk {
    fn load(
        _cx: &Cx<State>,
        key: &DeskKey,
        _since: Option<Validator>,
    ) -> impl core::future::Future<Output = Result<Load<Self>>> {
        let bytes = format!(r#"{{"desk":"{}"}}"#, key.desk).into_bytes();
        core::future::ready(
            Self::decode(&bytes).map(|desk| Load::fresh(desk, Canonical::new(bytes, None))),
        )
    }

    /// The feed of a desk: its first answer lists note 1 alone, with the
    /// inner state of note 1; each answer after it is `unchanged`.
    fn feed(cx: FeedCx<DeskKey, State>) -> impl core::future::Future<Output = Result<Feed>> {
        core::future::ready(Self::answer(&cx))
    }

    fn answer(cx: &FeedCx<DeskKey, State>) -> Result<Feed> {
        let desk = cx.key().desk.clone();
        let token = cx.token::<u64>()?;
        cx.state_mut(|state| {
            state.shelf_log.push(format!(
                "{desk} {} {}",
                token.map_or_else(|| "-".to_owned(), |token| token.to_string()),
                asked(cx.start(), cx.due())
            ));
        });
        if token.is_some() {
            return Ok(Feed::unchanged());
        }
        let notes = format!("/desks/{desk}/notes");
        if desk == "lent" {
            // The `lent` desk lists note 2, whose upstream refused it in the
            // answer of the desk: the desk hands its feed that failure.
            cx.dir(&notes, ListingWrite::Complete)?;
            cx.dir(format!("{notes}/2"), ListingWrite::Partial)?;
            return Ok(Feed::part(&1_u64)?
                .inner_failed::<Note>(
                    &NoteKey { desk, note: 2 },
                    &0_u64,
                    ProviderError::denied("note 2 is lent"),
                )?
                .into());
        }
        cx.dir(&notes, ListingWrite::Complete)?;
        cx.dir(format!("{notes}/1"), ListingWrite::Partial)?;
        cx.file(format!("{notes}/1/text"), b"Note 1".to_vec())?;
        Ok(Feed::part(&1_u64)?
            .inner::<Note>(&NoteKey { desk, note: 1 }, &0_u64)?
            .into())
    }
}

#[omnifs_sdk::path_captures]
struct DeskKey {
    desk: String,
}

/// A note of a desk, of an own feed: it joins the listing of the notes of
/// its desk, says that the desk does not list it for a note other than 1,
/// and says that its facts are unsettled on the `dated` desk.
#[omnifs_sdk::object(kind = "test.note", key = NoteKey, no_load)]
struct Note;

impl Note {
    fn feed(cx: FeedCx<NoteKey, State>) -> impl core::future::Future<Output = Result<Feed>> {
        core::future::ready(Self::answer(&cx))
    }

    fn answer(cx: &FeedCx<NoteKey, State>) -> Result<Feed> {
        let NoteKey { desk, note } = cx.key();
        let token = cx.token::<u64>()?;
        cx.state_mut(|state| {
            state.shelf_log.push(format!(
                "{desk}/{note} {} {}",
                token.map_or_else(|| "-".to_owned(), |token| token.to_string()),
                asked(cx.start(), cx.due())
            ));
        });
        let notes = format!("/desks/{desk}/notes");
        cx.dir(&notes, ListingWrite::Join)?;
        cx.dir(format!("{notes}/{note}"), ListingWrite::Partial)?;
        cx.file(
            format!("{notes}/{note}/text"),
            format!("Note {note}").into_bytes(),
        )?;
        let mut part = Feed::part(&token.map_or(1, |token| token + 1))?;
        if desk == "dated" {
            part = part.unsettled();
        }
        if *note != 1 {
            part = part.unlisted();
        }
        Ok(part.into())
    }
}

#[omnifs_sdk::path_captures]
struct NoteKey {
    desk: String,
    note: u64,
}

/// A memo, a dynamic object that its load gives, with two representations of
/// its canonical bytes: past its deadline a read of either loads it again.
#[omnifs_sdk::object(kind = "test.memo", key = MemoKey)]
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Memo {
    memo: u64,
}

impl Memo {
    fn load(
        _cx: &Cx<State>,
        key: &MemoKey,
        _since: Option<Validator>,
    ) -> impl core::future::Future<Output = Result<Load<Self>>> {
        let bytes = format!(r#"{{"memo":{}}}"#, key.memo).into_bytes();
        core::future::ready(
            Self::decode(&bytes).map(|memo| Load::fresh(memo, Canonical::new(bytes, None))),
        )
    }
}

impl Memo {
    /// The memo as the projection holds it, by the value of its object and
    /// without a load or a request of its own: `none` before its canonical
    /// bytes are stored.
    fn echo(
        cx: Cx<State>,
        key: MemoKey,
    ) -> impl core::future::Future<Output = Result<FileProjection>> {
        core::future::ready(cx.object::<Memo>(&key).map(|memo| {
            let text = memo.map_or_else(|| "none".to_owned(), |memo| format!("echo {}", memo.memo));
            FileProjection::body(text.into_bytes()).dynamic().build()
        }))
    }
}

impl Representable<Markdown> for Memo {
    fn represent(&self) -> Vec<u8> {
        format!("# Memo {}\n", self.memo).into_bytes()
    }
}

#[omnifs_sdk::path_captures]
struct MemoKey {
    memo: u64,
}

/// A card of a shelf, whose facts come from the feed of its shelf alone: an
/// object without canonical bytes and without a load.
#[omnifs_sdk::object(kind = "test.card", key = CardKey, no_load)]
struct Card;

#[omnifs_sdk::path_captures]
struct CardKey {
    #[allow(dead_code)]
    shelf: String,
    #[allow(dead_code)]
    card: u64,
}

#[omnifs_sdk::object(kind = "test.book", key = BookKey)]
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Book {
    book: u64,
}

impl Book {
    fn load(
        _cx: &Cx<State>,
        key: &BookKey,
        _since: Option<Validator>,
    ) -> impl core::future::Future<Output = Result<Load<Self>>> {
        let bytes = format!(r#"{{"book":{}}}"#, key.book).into_bytes();
        core::future::ready(
            Self::decode(&bytes).map(|book| Load::fresh(book, Canonical::new(bytes, None))),
        )
    }

    /// The feed of one book, answering for its title before the first answer
    /// of its shelf ends: it rewrites the title from the book's own upstream.
    fn feed(cx: FeedCx<BookKey, State>) -> impl core::future::Future<Output = Result<Feed>> {
        core::future::ready(Self::answer(&cx))
    }

    fn answer(cx: &FeedCx<BookKey, State>) -> Result<Feed> {
        let BookKey { shelf, book } = cx.key();
        let token = cx.token::<BookToken>()?;
        cx.state_mut(|state| {
            state.shelf_log.push(format!(
                "{shelf}/{book} {} {}",
                token.map_or_else(|| "-".to_owned(), |token| token.to_string()),
                asked(cx.start(), cx.due())
            ));
        });
        cx.file(
            format!("/shelves/{shelf}/books/{book}/title"),
            format!("Book {book} (own)").into_bytes(),
        )?;
        Ok(Feed::part(&BookToken::Own)?.into())
    }
}

#[omnifs_sdk::path_captures]
struct BookKey {
    shelf: String,
    book: u64,
}

/// What the operation of a feed call asked, for the log of the feeds: its
/// starting path without its fact, its starting path past the deadline of
/// the feed after `~`, or `-`.
fn asked(start: Option<&str>, due: Option<&str>) -> String {
    match (start, due) {
        (Some(start), _) => start.to_owned(),
        (None, Some(due)) => format!("~{due}"),
        (None, None) => "-".to_owned(),
    }
}

/// The calls of the shelf and book feeds so far, one per line.
async fn shelf_log(cx: Cx<State>) -> Result<FileProjection> {
    let log = cx.state(|state| state.shelf_log.join("\n"));
    Ok(FileProjection::body(format!("{log}\n").into_bytes())
        .dynamic()
        .build())
}

/// Advance the upstream version of the shelves.
async fn shelf_change(cx: Cx<State>) -> Result<FileProjection> {
    let version = cx.state_mut(|state| {
        state.shelf_version += 1;
        state.shelf_version
    });
    Ok(FileProjection::body(format!("{version}\n").into_bytes())
        .dynamic()
        .build())
}

// ===========================================================================
// Object family: `Repo`, with the tree of a Git remote
// ===========================================================================

#[omnifs_sdk::object(kind = "test.repo", key = RepoKey)]
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Repo {
    repo: String,
}

impl Repo {
    fn load(
        _cx: &Cx<State>,
        key: &RepoKey,
        _since: Option<Validator>,
    ) -> impl core::future::Future<Output = Result<Load<Self>>> {
        let bytes = format!(r#"{{"repo":"{}"}}"#, key.repo).into_bytes();
        core::future::ready(
            Self::decode(&bytes).map(|repo| Load::fresh(repo, Canonical::new(bytes, None))),
        )
    }

    /// The tree of the repository at the commit its upstream answers: a
    /// `304` to the commit of the tree stands, and a `200` names the commit
    /// the open checks out.
    async fn tree(cx: Cx<State>, key: RepoKey) -> Result<Tree> {
        let mut request = cx.http().get(format!(
            "https://httpbin.org/anything/repos/{}/commit",
            key.repo
        ));
        if let Some(commit) = cx.version() {
            request = request.header("if-none-match", commit.as_str());
        }
        let response = request.send().await?;
        match response.status().as_u16() {
            304 => Ok(Tree::unchanged()),
            200 => {
                let commit = String::from_utf8(response.into_body())
                    .map_err(|_| ProviderError::invalid_input("the commit is not UTF-8"))?;
                let opened = cx
                    .git()
                    .open_repo(format!("https://fixture.test/{}.git", key.repo))
                    .commit(&commit)
                    .await?;
                Ok(Tree::at(TreeRef::new(opened.tree), commit))
            },
            status => Err(ProviderError::network(format!(
                "the commit of the repository answered status {status}"
            ))),
        }
    }
}

#[omnifs_sdk::path_captures]
struct RepoKey {
    repo: String,
}

// ===========================================================================
// Provider
// ===========================================================================

#[omnifs_sdk::provider(
    id = "test-provider",
    display_name = "A test provider with canned data",
    mount = "test",
    projection = 1,
    capabilities(
        domain(
            "httpbin.org",
            "Synthetic fixture endpoint exercised by host guest-loader tests."
        ),
        git_repo(
            "https://fixture.test/*",
            "Fixture repositories whose trees host tests open."
        ),
    ),
    limits(memory_mb(16, "Canned-data fixture needs only a small heap."),)
)]
impl TestProvider {
    fn start(config: Config, r: &mut Router<State>) -> Result<State> {
        if config.root_ignore {
            r.file("/.gitignore").handler(root_ignore)?;
        }

        // Object family `Item`: facet collapse, canonical + representation +
        // computed faces, and a paged child-object `comments` collection.
        r.dir("/items").handler(items_root)?;
        r.dir("/items/{filter}").handler(item_list)?;
        r.object::<Item>("/items/{filter}/{number}", |o| {
            o.stable();
            o.file("item.json").canonical::<Json>()?;
            o.file("item.md").representation::<Markdown>()?;
            o.file("title").computed(Item::title)?;
            o.file("state").computed(Item::state)?;
            o.file("body").lazy().computed(Item::body)?;
            // The live ranged leaf opens through object stream dispatch.
            o.file("log").stream(Item::log)?;
            o.dir("comments").collection(item_comments)?;
            o.dir("checkout").tree(Item::unknown_tree)?;
            Ok(())
        })?;

        // Child `Comment` objects, plus an alias mounting the same object spec
        // at a second template (same identity, deeper than any pinned listing).
        let comment = r.object::<Comment>("/items/{filter}/{number}/comments/{idx}", |o| {
            o.stable();
            o.file("comment.json").canonical::<Json>()?;
            o.file("comment.md").representation::<Markdown>()?;
            o.file("author").computed(Comment::author)?;
            Ok(())
        })?;
        r.alias("/items/{filter}/{number}/replies/{idx}", &comment)?;

        // Preserved structural surface (route-shaped coverage).
        r.dir("/hello").handler(hello_dir)?;
        r.file("/hello/message").handler(message)?;
        r.file("/hello/greeting").handler(greeting)?;
        r.file("/hello/projected").handler(projected)?;
        r.file("/hello/lazy").handler(lazy)?;
        r.file("/hello/fresh-full").handler(fresh_full)?;
        // Each suspends on an HTTP callout. Two distinct paths (so reads do not
        // coalesce) let the host concurrency test hold two ops in flight at once
        // to prove independent ops interleave on one instance.
        r.file("/hello/blob").handler(blob)?;
        r.file("/hello/blob-of-md5").handler(blob_of_md5)?;
        r.file("/hello/blob-then-error").handler(blob_then_error)?;
        r.file("/hello/blob-of-another-size")
            .handler(blob_of_another_size)?;
        r.file("/hello/remote-a").handler(remote)?;
        r.file("/hello/remote-b").handler(remote)?;
        // A read parked on a slow upstream: the filesystem concurrency net holds
        // the callout answer for `{ms}` to prove one slow op cannot block the
        // mount.
        r.file("/slow/{ms}").ranged().handler(slow)?;
        r.file("/hello/ranged").ranged().handler(ranged)?;
        r.file("/hello/unknown-ranged")
            .ranged()
            .handler(unknown_ranged)?;
        r.file("/hello/large-ranged")
            .ranged()
            .handler(large_ranged)?;
        r.file("/hello/volatile-tail")
            .ranged()
            .handler(volatile_tail)?;
        r.file("/hello/live-log").ranged().handler(live_log)?;
        r.dir("/hello/bundle").handler(bundle)?;
        r.dir("/hello/feed").handler(feed)?;
        r.dir("/hello/validated").handler(validated)?;
        r.file("/hello/validated-calls").handler(validated_calls)?;
        r.dir("/hello/unbounded").handler(unbounded_feed)?;
        r.dir("/hello/many").handler(many_pages)?;
        r.dir("/hello/throttled").handler(throttled)?;
        r.dir("/hello/snapshot").handler(snapshot)?;
        r.dir("/hello/snapshot/comments")
            .handler(snapshot_comments)?;

        // Object feeds: the shelf feed answers for its books, the book feed
        // for a title before the first answer of its shelf ends.
        fed_objects(r)?;
        r.file("/hello/shelf-log").handler(shelf_log)?;
        r.file("/hello/shelf-change").handler(shelf_change)?;

        // A tree handoff at the commit the upstream of the repository answers.
        r.object::<Repo>("/repos/{repo}", |o| {
            o.stable();
            o.dir("tree").tree(Repo::tree)?;
            Ok(())
        })?;

        r.dir("/scoped").handler(scoped)?;
        r.file("/scoped/item").handler(scoped_item)?;

        r.dir("/dynamic/{name}").handler(dynamic)?;

        Ok(State::default())
    }
}

// ===========================================================================
// Object collection root
// ===========================================================================

async fn items_root(_cx: DirCx<State>) -> Result<DirListing> {
    Ok(DirListing::exhaustive([
        Entry::dir("open"),
        Entry::dir("all"),
    ]))
}

async fn root_ignore(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::body(b"provider ignore\n".to_vec()).build())
}

// ===========================================================================
// /hello directory
// ===========================================================================

async fn hello_dir(_cx: DirCx<State>) -> Result<DirListing> {
    Ok(DirListing::open(core::iter::empty::<Entry>()).preload_dir(
        "hello/bundle",
        DirListing::open([])
            .preload_file("title", FileProjection::inline(b"title".to_vec()).build())
            .preload_file("body", FileProjection::inline(b"body".to_vec()).build())
            .preload_file("empty", FileProjection::inline(Vec::new()).build()),
    ))
}

async fn message(cx: Cx<State>) -> Result<FileProjection> {
    let body = cx
        .version()
        .map_or("Hello, world!", |version| version.as_str());
    Ok(FileProjection::body(body.as_bytes().to_vec()).build())
}

async fn greeting(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::body(b"Hi there!\n".to_vec()).build())
}

async fn projected(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::body(b"title\n".to_vec())
        .preload_file(
            "hello/body",
            FileProjection::inline(b"body\n".to_vec()).build(),
        )
        .preload_file(
            "hello/state",
            FileProjection::inline(b"open\n".to_vec()).build(),
        )
        .build())
}

async fn lazy(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::body(b"lazy\n".to_vec()).build())
}

async fn fresh_full(cx: Cx<State>) -> Result<FileProjection> {
    let read = cx.state_mut(|state| {
        state.fresh_full_reads += 1;
        state.fresh_full_reads
    });
    Ok(
        FileProjection::body(format!("fresh-full-{read}\n").into_bytes())
            .dynamic()
            .build(),
    )
}

/// The blob of a fixed version of the fixture endpoint, of the bytes that the
/// host test answers its fetch with.
async fn fixture_blob(cx: &Cx<State>) -> Result<omnifs_sdk::blob::BlobRef> {
    cx.http()
        .get("https://httpbin.org/bytes")
        .into_blob()
        .version("1")
        .send()
        .await?
        .error_for_status()
}

/// A file of the bytes of a blob of the MD5 of `hello`, which the host
/// answers with those bytes alone.
async fn blob_of_md5(cx: Cx<State>) -> Result<FileProjection> {
    let blob = cx
        .http()
        .get("https://httpbin.org/bytes")
        .into_blob()
        .md5("5d41402abc4b2a76b9719d911017c592")
        .send()
        .await?
        .error_for_status()?;
    Ok(FileProjection::blob(blob.id)
        .size(FileSize::Exact(blob.size))
        .build())
}

/// A file of the bytes of a blob, which never cross to the provider.
async fn blob(cx: Cx<State>) -> Result<FileProjection> {
    let blob = fixture_blob(&cx).await?;
    Ok(FileProjection::blob(blob.id)
        .size(FileSize::Exact(blob.size))
        .build())
}

/// A read that fetches a blob and then fails: the host keeps the answer of
/// the fetch whatever the read does after it.
async fn blob_then_error(cx: Cx<State>) -> Result<FileProjection> {
    fixture_blob(&cx).await?;
    Err(ProviderError::internal("the read failed after its blob"))
}

/// A read that fetches a blob and declares another size than its body has,
/// which the host refuses when it lowers the answer.
async fn blob_of_another_size(cx: Cx<State>) -> Result<FileProjection> {
    let blob = fixture_blob(&cx).await?;
    Ok(FileProjection::blob(blob.id)
        .size(FileSize::Exact(blob.size + 1))
        .build())
}

/// Issues a single HTTP callout and returns its body verbatim. The handler
/// suspends on the async host import, so the host concurrency test can hold two
/// of these in flight simultaneously on one provider instance.
async fn remote(cx: Cx<State>) -> Result<FileProjection> {
    let resp = cx.http().get("https://httpbin.org/get").send().await?;
    Ok(FileProjection::body(resp.into_body()).dynamic().build())
}

/// A file whose read stays in flight for `{ms}` milliseconds (capped at 10s).
///
/// The file is ranged so the slowness lands on the read op itself: opening it
/// is cheap, and the first chunk read suspends on an HTTP callout that the
/// filesystem concurrency tests hold open for the requested delay, with the
/// host's callout-capture harness playing the slow upstream. Keeping the open
/// fast matters on macOS NFS, whose client serializes OPENs mount-wide, so a
/// file that is slow to open would measure client protocol behavior instead of
/// filesystem dispatch. Against a real network the URL is httpbin's delay
/// endpoint, whose 10-second ceiling matches the cap (it takes whole seconds,
/// so the requested delay rounds up).
async fn slow(_cx: Cx<State>, captures: SlowCaptures) -> Result<FileProjection> {
    let ms = captures.ms.min(10_000);
    Ok(FileProjection::ranged(SlowReader { ms })
        .size(FileSize::Unknown)
        .dynamic()
        .build())
}

/// Range reader for `slow/{ms}`: the first chunk performs the delayed HTTP
/// callout and serves the response body as the whole file.
#[derive(Clone, Debug)]
struct SlowReader {
    ms: u64,
}

impl RangeReader for SlowReader {
    fn read_chunk<'a>(
        &'a self,
        cx: &'a Cx<()>,
        offset: u64,
        length: u32,
    ) -> BoxFuture<'a, FileChunk> {
        Box::pin(async move {
            let secs = self.ms.div_ceil(1000);
            let resp = cx
                .http()
                .get(format!("https://httpbin.org/delay/{secs}"))
                .send()
                .await?;
            let body = resp.into_body();
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(body.len());
            let end = start.saturating_add(length as usize).min(body.len());
            Ok(FileChunk::new(body[start..end].to_vec(), end >= body.len()))
        })
    }
}

// ===========================================================================
// Ranged / unknown-size / live files
// ===========================================================================

async fn ranged(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::ranged(MemoryRangeReader::new(
        b"abcdefghijklmnopqrstuvwxyz".to_vec(),
    ))
    .size(FileSize::Exact(26))
    .dynamic()
    .version("alphabet-v1")
    .build())
}

async fn unknown_ranged(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(
        FileProjection::ranged(MemoryRangeReader::new(b"unknown-size\n".to_vec()))
            .size(FileSize::Unknown)
            .stable()
            .build(),
    )
}

async fn large_ranged(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::ranged(LargeRangedReader)
        .size(FileSize::Exact(LARGE_RANGED_SIZE))
        .stable()
        .build())
}

async fn volatile_tail(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::ranged(LiveTailReader)
        .size(FileSize::Unknown)
        .live()
        .build())
}

async fn live_log(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::ranged(GrowingLogReader)
        .size(FileSize::Unknown)
        .live()
        .build())
}

// ===========================================================================
// Static sub-directories
// ===========================================================================

async fn bundle(_cx: DirCx<State>) -> Result<DirListing> {
    Ok(
        DirListing::exhaustive([Entry::file("title"), Entry::file("body")])
            .preload_file(
                "hello/bundle/title",
                FileProjection::inline(b"title".to_vec()).build(),
            )
            .preload_file(
                "hello/bundle/body",
                FileProjection::inline(b"body".to_vec()).build(),
            ),
    )
}

async fn feed(cx: DirCx<State>) -> Result<DirListing> {
    let page = cx.page_cursor(0);
    let entries = [
        Entry::dir(format!("item-{}", page * 2)),
        Entry::dir(format!("item-{}", page * 2 + 1)),
    ];
    if page >= 2 {
        Ok(DirListing::open(entries))
    } else {
        Ok(DirListing::paged(entries, Cursor::Page(page + 1)))
    }
}

/// Two pages with validators and an interval of one second: the first lists
/// `v-0` and `v-1` with a cursor, the second `v-2`, complete. A call with the
/// validator of its page answers `unchanged` with that validator, except the
/// second revalidation of the first page, which answers a changed page with
/// `v-1b` in place of `v-1` and the same cursor, and the third, which answers
/// a complete page with `v-0` alone.
async fn validated(cx: DirCx<State>) -> Result<DirListing> {
    if !matches!(cx.intent(), DirIntent::List { .. }) {
        return Ok(DirListing::exhaustive([]));
    }
    cx.state_mut(|state| state.validated_calls += 1);
    let validator = |token: &str| PageValidator {
        token: VersionToken::from(token),
        interval: None,
    };
    let page = |names: &[&str], token: &str| {
        let entries = names.iter().map(|name| Entry::dir(*name));
        let listing = match cx.cursor() {
            None => DirListing::paged(entries, Cursor::Page(1)),
            Some(_) => DirListing::exhaustive(entries),
        };
        listing.with_validator(validator(token))
    };
    let first_page_stood = cx.state(|state| state.validated_first_page_stood);
    match (cx.cursor(), cx.version().map(VersionToken::as_str)) {
        (None, None) => Ok(page(&["v-0", "v-1"], "first-v1")),
        (None, Some("first-v1")) if !first_page_stood => {
            cx.state_mut(|state| state.validated_first_page_stood = true);
            Ok(DirListing::unchanged(validator("first-v1")))
        },
        (None, Some("first-v1")) => Ok(page(&["v-0", "v-1b"], "first-v2")),
        (None, Some("first-v2")) => {
            Ok(DirListing::exhaustive([Entry::dir("v-0")]).with_validator(validator("first-v3")))
        },
        (Some(Cursor::Page(1)), None) => Ok(page(&["v-2"], "second-v1")),
        (_, Some(token)) => Ok(DirListing::unchanged(validator(token))),
        (Some(_), None) => Err(ProviderError::not_found("hello/validated has two pages")),
    }
}

/// The calls of the `hello/validated` listing so far.
async fn validated_calls(cx: Cx<State>) -> Result<FileProjection> {
    let calls = cx.state(|state| state.validated_calls);
    Ok(FileProjection::body(format!("{calls}\n").into_bytes())
        .dynamic()
        .build())
}

async fn unbounded_feed(cx: DirCx<State>) -> Result<DirListing> {
    let page = cx.page_cursor(0);
    let entries = [
        Entry::dir(format!("u-{}", page * 2)),
        Entry::dir(format!("u-{}", page * 2 + 1)),
    ];
    Ok(DirListing::paged(entries, Cursor::Page(page + 1)))
}

/// A listing of 25 pages of one entry each: more than any count of pages
/// that the host could take as enough.
async fn many_pages(cx: DirCx<State>) -> Result<DirListing> {
    let page = cx.page_cursor(0);
    let entries = [Entry::dir(format!("m-{page}"))];
    if page >= 24 {
        Ok(DirListing::open(entries))
    } else {
        Ok(DirListing::paged(entries, Cursor::Page(page + 1)))
    }
}

async fn throttled(_cx: DirCx<State>) -> Result<DirListing> {
    Err(ProviderError::rate_limited("test throttle").with_retry_after(Some(Duration::from_secs(2))))
}

async fn snapshot(_cx: DirCx<State>) -> Result<DirListing> {
    Ok(
        DirListing::exhaustive([Entry::file("status")]).preload_file(
            "hello/snapshot/status",
            FileProjection::inline(b"open\n".to_vec()).build(),
        ),
    )
}

async fn snapshot_comments(_cx: DirCx<State>) -> Result<DirListing> {
    Ok(DirListing::exhaustive(core::iter::empty::<Entry>()))
}

// ===========================================================================
// /scoped
// ===========================================================================

async fn scoped(_cx: DirCx<State>) -> Result<DirListing> {
    Ok(DirListing::exhaustive([Entry::file("item")]).preload_file(
        "scoped/item",
        FileProjection::inline(b"scoped\n".to_vec()).build(),
    ))
}

async fn scoped_item(_cx: Cx<State>) -> Result<FileProjection> {
    Ok(FileProjection::body(b"scoped\n".to_vec()).build())
}

// ===========================================================================
// /dynamic/{name}
// ===========================================================================

async fn dynamic(_cx: DirCx<State>, captures: DynamicCaptures) -> Result<DirListing> {
    let value_path = format!("dynamic/{}/value", captures.name);
    let mut content = captures.name.into_bytes();
    content.push(b'\n');
    Ok(DirListing::exhaustive([Entry::file("value")])
        .preload_file(value_path, FileProjection::inline(content).build()))
}

// ===========================================================================
// Live-tail range reader for the live file
// ===========================================================================

#[derive(Clone, Debug)]
struct LiveTailReader;

impl RangeReader for LiveTailReader {
    fn read_chunk<'a>(
        &'a self,
        _cx: &'a Cx<()>,
        offset: u64,
        length: u32,
    ) -> BoxFuture<'a, FileChunk> {
        Box::pin(async move {
            let body = format!("tail:{offset}\n");
            let mut bytes = body.into_bytes();
            bytes.truncate(length as usize);
            Ok(FileChunk::new(bytes, false))
        })
    }
}

/// Finite content for the `Item::log` object stream face.
///
/// Unlike [`LiveTailReader`] (fixed-offset probes only), a recursive walk
/// (`grep -r "$ROOT/items/open/7"`) opens this leaf like any other file and
/// reads it to completion, so it must actually signal EOF once its small,
/// fixed content is exhausted rather than fabricate bytes forever.
const ITEM_LOG_CONTENT: &str = "log line 0\nlog line 1\nlog line 2\n";

#[derive(Clone, Debug)]
struct ItemLogReader;

impl RangeReader for ItemLogReader {
    fn read_chunk<'a>(
        &'a self,
        _cx: &'a Cx<()>,
        offset: u64,
        length: u32,
    ) -> BoxFuture<'a, FileChunk> {
        Box::pin(async move {
            let content = ITEM_LOG_CONTENT.as_bytes();
            let total = content.len() as u64;
            if offset >= total {
                return Ok(FileChunk::new(Vec::new(), true));
            }
            let start = usize::try_from(offset).expect("log offset fits usize");
            let end = offset.saturating_add(u64::from(length)).min(total);
            let end_usize = usize::try_from(end).expect("log end fits usize");
            Ok(FileChunk::new(
                content[start..end_usize].to_vec(),
                end >= total,
            ))
        })
    }
}

/// A live log that grows by one line at each read that reaches its end: a
/// read below the extent serves bytes bounded at the extent, and a read at or
/// past it returns the contract's "no more right now" shape, an empty chunk
/// with `eof = true`, which for a live file marks the current end without
/// ending it. The log grows by an event of its own, a read, and at no time.
#[derive(Clone, Debug)]
struct GrowingLogReader;

/// `line 000042\n`: 12 bytes per line, so extent math is exact.
const LOG_LINE_LEN: u64 = 12;

/// The lines of the log so far.
static LOG_LINES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

impl GrowingLogReader {
    /// The extent of the log now, in bytes.
    fn extent_now() -> u64 {
        LOG_LINES.load(core::sync::atomic::Ordering::Relaxed) * LOG_LINE_LEN
    }

    /// One more line, after a read that reached the end.
    fn grow() {
        LOG_LINES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
}

impl RangeReader for GrowingLogReader {
    fn read_chunk<'a>(
        &'a self,
        _cx: &'a Cx<()>,
        offset: u64,
        length: u32,
    ) -> BoxFuture<'a, FileChunk> {
        Box::pin(async move {
            let extent = Self::extent_now();
            if offset >= extent {
                Self::grow();
                return Ok(FileChunk::new(Vec::new(), true));
            }
            let end = offset.saturating_add(u64::from(length)).min(extent);
            let first_line = offset / LOG_LINE_LEN;
            let last_line = (end - 1) / LOG_LINE_LEN;
            let mut lines = Vec::new();
            for n in first_line..=last_line {
                lines.extend_from_slice(format!("line {n:06}\n").as_bytes());
            }
            let skip = usize::try_from(offset - first_line * LOG_LINE_LEN)
                .expect("line-relative offset fits usize");
            let take = usize::try_from(end - offset).expect("chunk length fits usize");
            if end >= extent {
                Self::grow();
            }
            Ok(FileChunk::new(
                lines[skip..skip + take].to_vec(),
                end >= extent,
            ))
        })
    }
}

#[derive(Clone, Debug)]
struct LargeRangedReader;

impl RangeReader for LargeRangedReader {
    fn read_chunk<'a>(
        &'a self,
        _cx: &'a Cx<()>,
        offset: u64,
        length: u32,
    ) -> BoxFuture<'a, FileChunk> {
        Box::pin(async move {
            let remaining = LARGE_RANGED_SIZE.saturating_sub(offset);
            let len = remaining.min(u64::from(length));
            let bytes = vec![b'L'; usize::try_from(len).expect("chunk length fits usize")];
            Ok(FileChunk::new(
                bytes,
                offset.saturating_add(len) >= LARGE_RANGED_SIZE,
            ))
        })
    }
}

/// The objects with fed faces: a shelf, its prints, cards and books, and a
/// memo beside them.
fn fed_objects(r: &mut Router<State>) -> Result<()> {
    r.object::<Desk>("/desks/{desk}", |o| {
        o.stable();
        o.file("desk.json").canonical::<Json>()?;
        o.dir("notes").fed()?;
        o.feed(Desk::feed).starts(["notes"])?;
        Ok(())
    })?;
    r.object::<Note>("/desks/{desk}/notes/{note}", |o| {
        o.stable();
        o.fed();
        o.file("text").fed()?;
        o.feed(Note::feed).own().starts(["", "text"])?;
        Ok(())
    })?;
    r.object::<Shelf>("/shelves/{shelf}", |o| {
        o.stable();
        o.file("shelf.json").canonical::<Json>()?;
        o.file("seen").fed()?;
        o.file("kept").fed()?;
        o.file("listed").fed()?;
        o.file("catalog").fed()?;
        o.file("catalog-read").fed()?;
        o.dir("books").fed()?;
        o.dir("cards").fed()?;
        o.dir("prints").fed()?;
        o.feed(Shelf::feed).starts(["books", "cards", "prints"])?;
        Ok(())
    })?;
    r.object::<Print>("/shelves/{shelf}/prints/{print}", |o| {
        o.dynamic();
        o.fed();
        o.file("print.json").canonical::<Json>()?;
        o.file("print.md").representation::<Markdown>()?;
        Ok(())
    })?;
    r.object::<Memo>("/memos/{memo}", |o| {
        o.dynamic();
        o.file("memo.json").canonical::<Json>()?;
        o.file("memo.md").representation::<Markdown>()?;
        o.file("echo").direct(Memo::echo)?;
        Ok(())
    })?;
    r.object::<Card>("/shelves/{shelf}/cards/{card}", |o| {
        o.stable();
        o.fed();
        o.file("text").fed()?;
        Ok(())
    })?;
    r.object::<Book>("/shelves/{shelf}/books/{book}", |o| {
        o.stable();
        o.fed();
        o.file("book.json").canonical::<Json>()?;
        o.file("title").fed()?;
        o.feed(Book::feed).starts(["title"])?;
        Ok(())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_snapshot_matches() {
        let mut router = Router::<State>::new();
        TestProvider::start(Config { root_ignore: false }, &mut router).unwrap();
        let router = router.compile().unwrap();

        let actual = omnifs_sdk::serde_json::to_string_pretty(&router.routes()).unwrap();
        let expected = include_str!("../tests/routes.snapshot.json").trim_end();
        if actual != expected {
            eprintln!("{actual}");
        }
        assert_eq!(actual, expected);
    }
}

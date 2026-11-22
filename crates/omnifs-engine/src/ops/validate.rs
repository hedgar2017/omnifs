//! Typed validation for provider operation payloads and terminal effects.

use std::collections::HashMap;

use crate::object_id::ObjectId;
use crate::view::{MAX_EAGER_RESPONSE_BYTES, MAX_VERSION_TOKEN_BYTES};
use crate::wit_protocol::{try_file_attrs_from_attrs, try_file_attrs_from_file_out};
use omnifs_core::path::{Path, Segment};
use omnifs_wit::host::types as wit_types;

pub(crate) struct ReturnValidator<'a, F> {
    effects: &'a wit_types::Effects,
    eager_bytes: usize,
    tree_exists: F,
    /// The effects are the facts of a feed part: their files are fed files.
    fed: bool,
}

impl<'a, F> ReturnValidator<'a, F>
where
    F: Fn(u64) -> bool,
{
    pub(crate) fn new(effects: &'a wit_types::Effects, tree_exists: F, fed: bool) -> Self {
        Self {
            effects,
            eager_bytes: 0,
            tree_exists,
            fed,
        }
    }

    pub(crate) fn common<T>(
        result: &std::result::Result<T, wit_types::ProviderError>,
        effects: &'a wit_types::Effects,
        tree_exists: F,
    ) -> std::result::Result<Self, String> {
        Self::checked(result, effects, tree_exists, false)
    }

    fn checked<T>(
        result: &std::result::Result<T, wit_types::ProviderError>,
        effects: &'a wit_types::Effects,
        tree_exists: F,
        fed: bool,
    ) -> std::result::Result<Self, String> {
        if result.is_err() && !effects_empty(effects) {
            return Err("provider error returns must not carry effects".to_string());
        }
        let mut validator = Self::new(effects, tree_exists, fed);
        validator.effects()?;
        Ok(validator)
    }

    /// The claims of a fed answer: at least one, each with an object kind
    /// and an anchor path, and no effects, since fed facts come from feeds.
    fn claims(&self, claims: &[wit_types::FeedClaim]) -> std::result::Result<(), String> {
        if claims.is_empty() {
            return Err("a feed answer carries no claims".to_string());
        }
        if !effects_empty(self.effects) {
            return Err("a feed answer must not carry effects".to_string());
        }
        for claim in claims {
            Self::feed_object(&claim.object)?;
        }
        Ok(())
    }

    fn object_id(id: &wit_types::LogicalId) -> std::result::Result<(), String> {
        if id.kind.is_empty() {
            return Err("feed object id.kind must not be empty".to_string());
        }
        Ok(())
    }

    fn feed_object(object: &wit_types::FeedObject) -> std::result::Result<(), String> {
        Self::object_id(&object.id)?;
        Path::parse(&object.path).map(|_| ()).map_err(|_| {
            format!(
                "feed object path {:?} is not a valid protocol path",
                object.path
            )
        })
    }

    /// A feed token carries what an answer takes to its last part, as the
    /// IDs of the results of a listing of an upstream without an inventory:
    /// no size bounds it, as none bounds the upstream.
    fn feed_token(token: &str) -> std::result::Result<(), String> {
        if token.is_empty() {
            return Err("feed token must not be empty".to_string());
        }
        Ok(())
    }

    /// `tokened` says whether the call carried a token, the only call
    /// `unchanged` and a restart can answer. Only a part is unsettled, and a
    /// restart carries no effects.
    pub(crate) fn feed(
        result: &std::result::Result<wit_types::FeedAnswer, wit_types::ProviderError>,
        effects: &wit_types::Effects,
        unsettled: bool,
        tokened: bool,
    ) -> std::result::Result<(), String> {
        if unsettled && !matches!(result, Ok(wit_types::FeedAnswer::Part(_))) {
            return Err("only a feed part is unsettled".to_string());
        }
        let Ok(answer) = result else { return Ok(()) };
        match answer {
            wit_types::FeedAnswer::Unchanged | wit_types::FeedAnswer::Restart(_) if !tokened => {
                Err("feed answered unchanged or restart to a call without a token".to_string())
            },
            wit_types::FeedAnswer::Unchanged => Ok(()),
            wit_types::FeedAnswer::Restart(_) if !effects_empty(effects) => {
                Err("a feed restart must not carry effects".to_string())
            },
            wit_types::FeedAnswer::Restart(restart) => Self::feed_token(&restart.token),
            wit_types::FeedAnswer::Part(part) if part.more && part.error.is_some() => {
                Err("a feed part with an error has no more parts".to_string())
            },
            wit_types::FeedAnswer::Part(part) => {
                Self::feed_token(&part.token)?;
                for state in &part.inner {
                    Self::object_id(&state.id)?;
                    Self::feed_token(&state.token)?;
                }
                for anchor in &part.anchors {
                    for path in [&anchor.path, &anchor.anchor] {
                        Path::parse(path).map_err(|_| {
                            format!("facet anchor {path:?} is not a valid protocol path")
                        })?;
                    }
                }
                Ok(())
            },
        }
    }

    pub(crate) fn lookup(
        &mut self,
        result: &std::result::Result<wit_types::LookupChildResult, wit_types::ProviderError>,
    ) -> std::result::Result<(), String> {
        let Ok(result) = result else { return Ok(()) };
        match result {
            wit_types::LookupChildResult::Entry(entry) => {
                Self::segment_name(&entry.target.name)?;
                self.entry(&entry.target.kind)?;
                for sibling in &entry.siblings {
                    Self::segment_name(&sibling.name)?;
                    self.entry(&sibling.kind)?;
                }
            },
            wit_types::LookupChildResult::Subtree(subtree) => self.subtree(subtree)?,
            wit_types::LookupChildResult::NotFound(_) => {},
            wit_types::LookupChildResult::Feed(claims) => self.claims(claims)?,
        }
        Ok(())
    }

    /// `validated` says whether the call carried the validator of a cached
    /// page, the only call `unchanged` can answer.
    pub(crate) fn list(
        &mut self,
        result: &std::result::Result<wit_types::ListChildrenResult, wit_types::ProviderError>,
        validated: bool,
    ) -> std::result::Result<(), String> {
        let Ok(result) = result else { return Ok(()) };
        match result {
            wit_types::ListChildrenResult::Entries(listing) => {
                for entry in &listing.entries {
                    Self::segment_name(&entry.name)?;
                    self.entry(&entry.kind)?;
                }
                if let Some(validator) = &listing.validator {
                    Self::version_token(&validator.token)
                        .map_err(|error| format!("page validator: {error}"))?;
                }
            },
            wit_types::ListChildrenResult::Subtree(subtree) => self.subtree(subtree)?,
            wit_types::ListChildrenResult::Unchanged(_) if !validated => {
                return Err(
                    "list-children answered unchanged to a call without a validator".into(),
                );
            },
            wit_types::ListChildrenResult::Unchanged(validator) => {
                if let Some(validator) = validator {
                    Self::version_token(&validator.token)
                        .map_err(|error| format!("page validator: {error}"))?;
                }
            },
            wit_types::ListChildrenResult::Feed(claims) => self.claims(claims)?,
        }
        Ok(())
    }

    pub(crate) fn read(
        &mut self,
        result: &std::result::Result<wit_types::ReadFileOutcome, wit_types::ProviderError>,
    ) -> std::result::Result<(), String> {
        let Ok(result) = result else { return Ok(()) };
        match result {
            wit_types::ReadFileOutcome::Found(result) => self.read_file_result(result),
            wit_types::ReadFileOutcome::NotFound(_) => Ok(()),
            wit_types::ReadFileOutcome::Feed(claims) => self.claims(claims),
        }
    }

    pub(crate) fn open(
        result: &std::result::Result<wit_types::OpenFileResult, wit_types::ProviderError>,
    ) -> std::result::Result<(), String> {
        if let Ok(result) = result {
            Self::file_attrs_metadata(&result.attrs)?;
        }
        Ok(())
    }

    pub(crate) fn chunk(
        result: &std::result::Result<wit_types::ReadChunkResult, wit_types::ProviderError>,
        requested_length: u32,
    ) -> std::result::Result<(), String> {
        if let Ok(result) = result
            && result.content.len() > requested_length as usize
        {
            return Err(format!(
                "read-chunk result exceeds requested length {requested_length}: returned {} bytes",
                result.content.len()
            ));
        }
        Ok(())
    }

    fn effects(&mut self) -> std::result::Result<(), String> {
        let effects = self.effects;
        let mut canonical_path_to_id: HashMap<String, Vec<u8>> = HashMap::new();
        let mut fs_path_to_id: HashMap<String, Vec<u8>> = HashMap::new();
        for store in &effects.canonical {
            if store.id.kind.is_empty() {
                return Err("canonical-store id.kind must not be empty".to_string());
            }
            if store.view_leaves.is_empty() {
                return Err("canonical-store has an empty view-leaves list".to_string());
            }
            let id_bytes = ObjectId::from_wit(&store.id).as_bytes().to_vec();
            for leaf in &store.view_leaves {
                if leaf.is_empty() {
                    return Err("canonical-store has an empty view-leaf".to_string());
                }
                if Path::parse(leaf).is_err() {
                    return Err(format!(
                        "canonical-store view-leaf {leaf:?} is not a valid protocol path"
                    ));
                }
                let leaf_path = Path::parse(leaf).map_err(|_| {
                    format!("canonical-store view-leaf {leaf:?} is not a valid protocol path")
                })?;
                if crate::tree::synthetic::is_reserved_provider_leaf(leaf_path.name()) {
                    return Err(format!(
                        "canonical-store view-leaf {leaf:?} uses a reserved provider name"
                    ));
                }
                Self::track_unique_path_id(&mut canonical_path_to_id, leaf, &id_bytes)?;
            }
            if let Some(token) = &store.validator
                && token.len() > MAX_VERSION_TOKEN_BYTES
            {
                return Err(format!(
                    "canonical-store validator token exceeds {MAX_VERSION_TOKEN_BYTES} bytes"
                ));
            }
        }
        for write in &effects.fs {
            let path = Path::parse(&write.path);
            if path.is_err() {
                return Err(format!(
                    "fs-write path {:?} is not a valid protocol path",
                    write.path
                ));
            }
            let path = path.map_err(|_| {
                format!(
                    "fs-write path {:?} is not a valid protocol path",
                    write.path
                )
            })?;
            if crate::tree::synthetic::is_reserved_provider_leaf(path.name()) {
                return Err(format!(
                    "fs-write {:?} uses a reserved provider name",
                    write.path
                ));
            }
            if let Some(id) = &write.id {
                let id_bytes = ObjectId::from_wit(id).as_bytes().to_vec();
                Self::track_unique_path_id(&mut fs_path_to_id, &write.path, &id_bytes)?;
                Self::check_path_id_conflict(&canonical_path_to_id, &write.path, &id_bytes)?;
            }
            if let wit_types::FsKind::File(file) = &write.kind {
                if self.fed {
                    let view_leaf = write.id.as_ref().is_some_and(|id| {
                        canonical_path_to_id.get(&write.path).map(Vec::as_slice)
                            == Some(ObjectId::from_wit(id).as_bytes())
                    });
                    Self::fed_file(file, view_leaf)
                } else {
                    self.file_out(file)
                }
                .map_err(|e| format!("fs-write {:?}: {e}", write.path))?;
            }
        }
        for removal in &effects.removals {
            let path = Path::parse(removal)
                .map_err(|_| format!("removal path {removal:?} is not a valid protocol path"))?;
            if path.is_root() {
                return Err("a removal must not name the mount root".to_string());
            }
        }
        for invalidation in &effects.invalidations {
            if let wit_types::Invalidation::Listing(
                wit_types::PathOrPrefix::Path(p) | wit_types::PathOrPrefix::Prefix(p),
            ) = invalidation
                && Path::parse(p).is_err()
            {
                return Err(format!(
                    "invalidation path {p:?} is not a valid protocol path"
                ));
            }
        }
        Ok(())
    }

    /// A subtree names a tree the host opened and a commit.
    fn subtree(&self, subtree: &wit_types::Subtree) -> std::result::Result<(), String> {
        if !(self.tree_exists)(subtree.tree) {
            return Err(format!(
                "subtree result references unknown tree {}",
                subtree.tree
            ));
        }
        crate::cloner::GitCloner::validate_commit(&subtree.commit)
            .map_err(|error| format!("subtree commit: {error}"))
    }

    fn track_unique_path_id(
        map: &mut HashMap<String, Vec<u8>>,
        path: &str,
        id: &[u8],
    ) -> std::result::Result<(), String> {
        match map.get(path) {
            Some(other) if other.as_slice() != id => Err(format!(
                "path {path:?} maps to two different object ids in one return"
            )),
            Some(_) => Err(format!("duplicate path {path:?} in one return")),
            None => {
                map.insert(path.to_string(), id.to_vec());
                Ok(())
            },
        }
    }

    fn check_path_id_conflict(
        map: &HashMap<String, Vec<u8>>,
        path: &str,
        id: &[u8],
    ) -> std::result::Result<(), String> {
        match map.get(path) {
            Some(other) if other.as_slice() != id => Err(format!(
                "path {path:?} maps to two different object ids in one return"
            )),
            _ => Ok(()),
        }
    }

    fn entry(&mut self, kind: &wit_types::EntryKind) -> std::result::Result<(), String> {
        match kind {
            wit_types::EntryKind::Directory => Ok(()),
            wit_types::EntryKind::File(file) => self.file_out(file),
        }
    }

    fn segment_name(name: &str) -> std::result::Result<(), String> {
        Segment::try_from(name)
            .map_err(|error| format!("dir-entry name {name:?} is not a valid segment: {error}"))
            .and_then(|_| {
                if crate::tree::synthetic::is_reserved_provider_leaf(name) {
                    Err(format!(
                        "dir-entry name {name:?} is reserved for host controls"
                    ))
                } else {
                    Ok(())
                }
            })
    }

    /// A fed file: a stable file of its declared exact size without a
    /// version, with inline bytes, which the projection holds whole, or with
    /// a blob of the host, which no size bounds; or, for a `view_leaf` of
    /// the canonical bytes that the effects store for the object of its
    /// write, a stable file without a version, a size, or bytes, which a
    /// full read renders. Fed files are facts, not eager preloads.
    fn fed_file(file: &wit_types::FileOut, view_leaf: bool) -> std::result::Result<(), String> {
        if !matches!(file.attrs.stability, wit_types::Stability::Stable)
            || file.attrs.version_token.is_some()
        {
            return Err("a fed file is a stable file without a version".into());
        }
        if matches!(
            file.bytes,
            wit_types::ByteSource::Deferred(wit_types::ReadMode::Full)
        ) {
            return if view_leaf && matches!(file.attrs.size, wit_types::FileSize::Unknown) {
                Ok(())
            } else {
                Err("a fed file without bytes is a view leaf, without a size, of the canonical bytes that its part stores for its object".into())
            };
        }
        let wit_types::FileSize::Exact(size) = file.attrs.size else {
            return Err("a fed file has its exact size".into());
        };
        match &file.bytes {
            wit_types::ByteSource::Inline(bytes) => {
                let len =
                    u64::try_from(bytes.len()).map_err(|_| "fed file length does not fit u64")?;
                if size != len {
                    return Err("a fed file has the size of its inline bytes".into());
                }
                if len > crate::tree::MATERIALIZE_MAX_BYTES {
                    return Err(format!(
                        "fed file with inline bytes exceeds {} bytes",
                        crate::tree::MATERIALIZE_MAX_BYTES
                    ));
                }
                Ok(())
            },
            wit_types::ByteSource::Blob(_) => Ok(()),
            wit_types::ByteSource::Canonical | wit_types::ByteSource::Deferred(_) => {
                Err("a fed file carries inline bytes or a blob".to_string())
            },
        }
    }

    fn file_out(&mut self, file: &wit_types::FileOut) -> std::result::Result<(), String> {
        if matches!(file.bytes, wit_types::ByteSource::Blob(_)) {
            Self::file_attrs_metadata(&file.attrs)
        } else {
            let attrs = try_file_attrs_from_file_out(file, |_| {
                Err("blob handle requires mount resolution".to_string())
            })?;
            attrs.validate()?;
            self.add_eager_bytes(attrs.eager_byte_len())
        }
    }

    fn read_file_result(
        &mut self,
        result: &wit_types::ReadFileResult,
    ) -> std::result::Result<(), String> {
        Self::file_attrs_metadata(&result.attrs)?;
        match &result.bytes {
            wit_types::ByteSource::Inline(bytes) => {
                let attrs = try_file_attrs_from_attrs(&result.attrs)?;
                attrs
                    .validate_complete_content(bytes.len())
                    .map_err(|e| format!("read-file result: {e}"))?;
                self.add_eager_bytes(bytes.len())?;
            },
            wit_types::ByteSource::Canonical | wit_types::ByteSource::Blob(_) => {},
            wit_types::ByteSource::Deferred(_) => {
                return Err(
                    "read-file result: ByteSource::Deferred is not a valid read answer".to_string(),
                );
            },
        }
        Ok(())
    }

    fn file_attrs_metadata(attrs: &wit_types::FileAttrs) -> std::result::Result<(), String> {
        attrs
            .version_token
            .as_deref()
            .map_or(Ok(()), Self::version_token)
    }

    fn version_token(token: &str) -> std::result::Result<(), String> {
        if token.is_empty() {
            return Err("version token must not be empty".to_string());
        }
        if token.len() > MAX_VERSION_TOKEN_BYTES {
            return Err(format!(
                "version token exceeds {MAX_VERSION_TOKEN_BYTES} bytes"
            ));
        }
        Ok(())
    }

    fn add_eager_bytes(&mut self, bytes: usize) -> std::result::Result<(), String> {
        self.eager_bytes = self
            .eager_bytes
            .checked_add(bytes)
            .ok_or_else(|| "aggregate eager byte count overflowed".to_string())?;
        if self.eager_bytes > MAX_EAGER_RESPONSE_BYTES {
            return Err(format!(
                "terminal response exceeds aggregate eager byte limit of {MAX_EAGER_RESPONSE_BYTES} bytes"
            ));
        }
        Ok(())
    }
}

pub(crate) fn validate_lookup<F>(
    result: &std::result::Result<wit_types::LookupChildResult, wit_types::ProviderError>,
    effects: &wit_types::Effects,
    tree_exists: F,
) -> std::result::Result<(), String>
where
    F: Fn(u64) -> bool,
{
    let mut v = ReturnValidator::common(result, effects, tree_exists)?;
    v.lookup(result)
}
pub(crate) fn validate_list<F>(
    result: &std::result::Result<wit_types::ListChildrenResult, wit_types::ProviderError>,
    effects: &wit_types::Effects,
    validated: bool,
    tree_exists: F,
) -> std::result::Result<(), String>
where
    F: Fn(u64) -> bool,
{
    let mut v = ReturnValidator::common(result, effects, tree_exists)?;
    v.list(result, validated)
}
/// Validate one feed call: `tokened` says whether the call carried a token,
/// and `unsettled` whether the call said that the facts of its part are. The effects
/// of a part are its fed facts.
pub(crate) fn validate_feed<F>(
    result: &std::result::Result<wit_types::FeedAnswer, wit_types::ProviderError>,
    effects: &wit_types::Effects,
    unsettled: bool,
    tokened: bool,
    tree_exists: F,
) -> std::result::Result<(), String>
where
    F: Fn(u64) -> bool,
{
    let _ = ReturnValidator::checked(result, effects, tree_exists, true)?;
    ReturnValidator::<F>::feed(result, effects, unsettled, tokened)
}
pub(crate) fn validate_read<F>(
    result: &std::result::Result<wit_types::ReadFileOutcome, wit_types::ProviderError>,
    effects: &wit_types::Effects,
    tree_exists: F,
) -> std::result::Result<(), String>
where
    F: Fn(u64) -> bool,
{
    let mut v = ReturnValidator::common(result, effects, tree_exists)?;
    v.read(result)
}
pub(crate) fn validate_open<F>(
    result: &std::result::Result<wit_types::OpenFileResult, wit_types::ProviderError>,
    effects: &wit_types::Effects,
    tree_exists: F,
) -> std::result::Result<(), String>
where
    F: Fn(u64) -> bool,
{
    let _ = ReturnValidator::common(result, effects, tree_exists)?;
    ReturnValidator::<F>::open(result)
}
pub(crate) fn validate_chunk<F>(
    result: &std::result::Result<wit_types::ReadChunkResult, wit_types::ProviderError>,
    effects: &wit_types::Effects,
    requested_length: u32,
    tree_exists: F,
) -> std::result::Result<(), String>
where
    F: Fn(u64) -> bool,
{
    let _ = ReturnValidator::common(result, effects, tree_exists)?;
    ReturnValidator::<F>::chunk(result, requested_length)
}
pub(crate) fn validate_initialize<F>(
    result: &std::result::Result<(), wit_types::ProviderError>,
    effects: &wit_types::Effects,
    tree_exists: F,
) -> std::result::Result<(), String>
where
    F: Fn(u64) -> bool,
{
    let _ = ReturnValidator::common(result, effects, tree_exists)?;
    Ok(())
}
fn effects_empty(effects: &wit_types::Effects) -> bool {
    effects.canonical.is_empty()
        && effects.fs.is_empty()
        && effects.invalidations.is_empty()
        && effects.removals.is_empty()
}

/// The checks of the effects of any terminal, for the tests of them alone.
#[cfg(test)]
fn validate_effects<F>(
    result: &std::result::Result<(), wit_types::ProviderError>,
    effects: &wit_types::Effects,
    tree_exists: F,
) -> std::result::Result<(), String>
where
    F: Fn(u64) -> bool,
{
    let _ = ReturnValidator::common(result, effects, tree_exists)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::MAX_INLINE_PROJECTABLE_BYTES;

    fn effects() -> wit_types::Effects {
        wit_types::Effects {
            canonical: Vec::new(),
            fs: Vec::new(),
            invalidations: Vec::new(),
            removals: Vec::new(),
        }
    }

    fn entry(name: &str) -> wit_types::DirEntry {
        wit_types::DirEntry {
            name: name.to_string(),
            kind: wit_types::EntryKind::Directory,
            id: None,
        }
    }

    fn attrs(size: wit_types::FileSize, stability: wit_types::Stability) -> wit_types::FileAttrs {
        wit_types::FileAttrs {
            size,
            stability,
            version_token: None,
        }
    }

    fn file_out(
        size: wit_types::FileSize,
        bytes: wit_types::ByteSource,
        stability: wit_types::Stability,
    ) -> wit_types::FileOut {
        wit_types::FileOut {
            attrs: attrs(size, stability),
            bytes,
            content_type: None,
        }
    }

    fn deferred_exact(size: u64) -> wit_types::FileOut {
        file_out(
            wit_types::FileSize::Exact(size),
            wit_types::ByteSource::Deferred(wit_types::ReadMode::Full),
            wit_types::Stability::Stable,
        )
    }

    fn fs_file_write(path: String, file: wit_types::FileOut) -> wit_types::FsWrite {
        wit_types::FsWrite {
            id: None,
            path,
            kind: wit_types::FsKind::File(file),
        }
    }

    #[test]
    fn rejects_invalid_inline_projection_in_entries() {
        let result = Ok(wit_types::ListChildrenResult::Entries(
            wit_types::DirListing {
                entries: vec![entry_with_kind(
                    "bad",
                    wit_types::EntryKind::File(file_out(
                        wit_types::FileSize::Unknown,
                        wit_types::ByteSource::Inline(b"bad".to_vec()),
                        wit_types::Stability::Stable,
                    )),
                )],
                exhaustive: true,
                next_cursor: None,
                validator: None,
            },
        ));
        let error = validate_list(&result, &effects(), false, |_| true).unwrap_err();
        assert!(error.contains("inline bytes require FileSize::Exact"));
    }

    #[test]
    fn rejects_volatile_non_ranged_attrs() {
        let result = Ok(wit_types::ListChildrenResult::Entries(
            wit_types::DirListing {
                entries: vec![entry_with_kind(
                    "tail",
                    wit_types::EntryKind::File(file_out(
                        wit_types::FileSize::Unknown,
                        wit_types::ByteSource::Deferred(wit_types::ReadMode::Full),
                        wit_types::Stability::Live,
                    )),
                )],
                exhaustive: true,
                next_cursor: None,
                validator: None,
            },
        ));
        let error = validate_list(&result, &effects(), false, |_| true).unwrap_err();
        assert!(error.contains("Stability::Live requires"));
    }

    fn entry_with_kind(name: &str, kind: wit_types::EntryKind) -> wit_types::DirEntry {
        wit_types::DirEntry {
            name: name.to_string(),
            kind,
            id: None,
        }
    }

    #[test]
    fn rejects_invalid_fs_write_path_without_id() {
        let effects = wit_types::Effects {
            fs: vec![fs_file_write("bad".to_string(), deferred_exact(1))],
            ..effects()
        };
        let error = validate_effects(&Ok(()), &effects, |_| true).unwrap_err();
        assert!(error.contains("fs-write path") && error.contains("valid protocol path"));
    }

    #[test]
    fn rejects_bad_fs_write_size_and_aggregate_eager_cap() {
        let mut bad_size_file = deferred_exact(4);
        bad_size_file.bytes = wit_types::ByteSource::Inline(b"toolong".to_vec());
        let bad_size_effects = wit_types::Effects {
            fs: vec![fs_file_write("/bad".to_string(), bad_size_file)],
            ..effects()
        };
        let error = validate_effects(&Ok(()), &bad_size_effects, |_| true).unwrap_err();
        assert!(error.contains("declares size 4"));

        let aggregate_effects = wit_types::Effects {
            fs: (0..9)
                .map(|index| {
                    let bytes = vec![0; MAX_INLINE_PROJECTABLE_BYTES];
                    fs_file_write(
                        format!("/large-{index}"),
                        file_out(
                            wit_types::FileSize::Exact(bytes.len() as u64),
                            wit_types::ByteSource::Inline(bytes),
                            wit_types::Stability::Stable,
                        ),
                    )
                })
                .collect(),
            ..effects()
        };
        let error = validate_effects(&Ok(()), &aggregate_effects, |_| true).unwrap_err();
        assert!(error.contains("aggregate eager byte limit"));
    }

    #[test]
    fn rejects_read_content_that_violates_declared_size() {
        let result = Ok(wit_types::ReadFileOutcome::Found(
            wit_types::ReadFileResult {
                content_type: None,
                attrs: attrs(wit_types::FileSize::NonZero, wit_types::Stability::Stable),
                bytes: wit_types::ByteSource::Inline(Vec::new()),
            },
        ));
        let error = validate_read(&result, &effects(), |_| true).unwrap_err();
        assert!(error.contains("read-file result") && error.contains("FileSize::NonZero"));
    }

    #[test]
    fn rejects_empty_version_tokens() {
        let mut file = deferred_exact(1);
        file.attrs.version_token = Some(String::new());
        let result = Ok(wit_types::ListChildrenResult::Entries(
            wit_types::DirListing {
                entries: vec![entry_with_kind(
                    "versioned",
                    wit_types::EntryKind::File(file),
                )],
                exhaustive: true,
                next_cursor: None,
                validator: None,
            },
        ));
        let error = validate_list(&result, &effects(), false, |_| true).unwrap_err();
        assert!(error.contains("version token must not be empty"));
    }

    #[test]
    fn unchanged_answers_only_a_validated_call_and_page_validators_are_version_tokens() {
        let unchanged = Ok(wit_types::ListChildrenResult::Unchanged(Some(
            wit_types::PageValidator {
                token: "v1".to_owned(),
                interval: Some(60),
            },
        )));
        let error = validate_list(&unchanged, &effects(), false, |_| true).unwrap_err();
        assert!(error.contains("without a validator"));
        validate_list(&unchanged, &effects(), true, |_| true).unwrap();

        for (token, message) in [
            (String::new(), "must not be empty"),
            ("v".repeat(MAX_VERSION_TOKEN_BYTES + 1), "exceeds"),
        ] {
            let validator = wit_types::PageValidator {
                token,
                interval: None,
            };
            let result = Ok(wit_types::ListChildrenResult::Entries(
                wit_types::DirListing {
                    entries: vec![entry("child")],
                    exhaustive: true,
                    next_cursor: None,
                    validator: Some(validator.clone()),
                },
            ));
            let error = validate_list(&result, &effects(), false, |_| true).unwrap_err();
            assert!(error.contains("page validator") && error.contains(message));
            let unchanged = Ok(wit_types::ListChildrenResult::Unchanged(Some(validator)));
            let error = validate_list(&unchanged, &effects(), true, |_| true).unwrap_err();
            assert!(error.contains("page validator") && error.contains(message));
        }
    }

    #[test]
    fn subtree_result_requires_known_tree_and_a_commit() {
        let subtree = |commit: &str| {
            Ok(wit_types::LookupChildResult::Subtree(wit_types::Subtree {
                tree: 7,
                commit: commit.to_string(),
            }))
        };
        let result = subtree("0123456789abcdef0123456789abcdef01234567");
        let error = validate_lookup(&result, &effects(), |_| false).unwrap_err();
        assert!(error.contains("references unknown tree 7"));
        validate_lookup(&result, &effects(), |_| true).unwrap();

        let error = validate_lookup(&subtree("--upload-pack=x"), &effects(), |_| true).unwrap_err();
        assert!(error.contains("subtree commit"));
    }

    #[test]
    fn rejects_oversized_read_chunk() {
        let result = Ok(wit_types::ReadChunkResult {
            content: vec![0; 5],
            eof: false,
        });
        let error = validate_chunk(&result, &effects(), 4, |_| true).unwrap_err();
        assert!(error.contains("requested length 4"));
    }

    #[test]
    fn error_returns_reject_effects() {
        let result = Err(wit_types::ProviderError {
            kind: wit_types::ErrorKind::Internal,
            message: "failed".to_string(),
            retryable: false,
            retry_after: None,
        });
        let effects = wit_types::Effects {
            invalidations: vec![wit_types::Invalidation::Listing(
                wit_types::PathOrPrefix::Path("x".to_string()),
            )],
            ..effects()
        };
        let error = validate_effects(&result, &effects, |_| true).unwrap_err();
        assert!(error.contains("error returns must not carry effects"));
    }

    #[test]
    fn lookup_rejects_invalid_target_and_sibling_names() {
        let result = Ok(wit_types::LookupChildResult::Entry(
            wit_types::LookupEntry {
                target: entry("target/child"),
                siblings: vec![entry("sibling")],
                exhaustive: true,
            },
        ));
        let error = validate_lookup(&result, &effects(), |_| true).unwrap_err();
        assert!(error.contains("target/child"));

        let result = Ok(wit_types::LookupChildResult::Entry(
            wit_types::LookupEntry {
                target: entry("target"),
                siblings: vec![entry("")],
                exhaustive: true,
            },
        ));
        let error = validate_lookup(&result, &effects(), |_| true).unwrap_err();
        assert!(error.contains("empty"));
    }

    #[test]
    fn list_validates_entry_names_and_reserves_exact_control_leaves() {
        let result = Ok(wit_types::ListChildrenResult::Entries(
            wit_types::DirListing {
                entries: vec![entry("nested/name")],
                exhaustive: true,
                next_cursor: None,
                validator: None,
            },
        ));
        let error = validate_list(&result, &effects(), false, |_| true).unwrap_err();
        assert!(error.contains("nested/name"));

        for control in ["@next", "@all"] {
            let result = Ok(wit_types::ListChildrenResult::Entries(
                wit_types::DirListing {
                    entries: vec![entry(control)],
                    exhaustive: true,
                    next_cursor: None,
                    validator: None,
                },
            ));
            let error = validate_list(&result, &effects(), false, |_| true).unwrap_err();
            assert!(error.contains(control));
        }

        let result = Ok(wit_types::ListChildrenResult::Entries(
            wit_types::DirListing {
                entries: vec![entry("@cloudflare")],
                exhaustive: true,
                next_cursor: None,
                validator: None,
            },
        ));
        validate_list(&result, &effects(), false, |_| true).unwrap();
    }

    #[test]
    fn no_size_bounds_the_token_of_a_feed_part() {
        let part = |token: String| {
            Ok(wit_types::FeedAnswer::Part(wit_types::FeedPart {
                token,
                more: false,
                inner: Vec::new(),
                anchors: Vec::new(),
                unlisted: false,
                anew: false,
                error: None,
            }))
        };
        let large = "t".repeat(64 * 1024 * 1024 + 1);
        assert_eq!(
            validate_feed(&part(large), &effects(), false, false, |_| false),
            Ok(())
        );
        assert!(validate_feed(&part(String::new()), &effects(), false, false, |_| false).is_err());
    }

    /// A fed file without bytes is a view leaf of the canonical bytes that
    /// its part stores for the object of its write, and no other file.
    #[test]
    fn a_fed_file_without_bytes_is_a_view_leaf_of_a_canonical_of_its_part() {
        let part = Ok(wit_types::FeedAnswer::Part(wit_types::FeedPart {
            token: "t".to_string(),
            more: false,
            inner: Vec::new(),
            anchors: Vec::new(),
            unlisted: false,
            anew: false,
            error: None,
        }));
        let id = |value: &str| wit_types::LogicalId {
            kind: "test.print".to_string(),
            captures: vec![wit_types::IdCapture {
                name: "print".to_string(),
                value: value.to_string(),
            }],
        };
        let leaf = |of: Option<&str>, size: wit_types::FileSize| wit_types::Effects {
            canonical: vec![wit_types::CanonicalStore {
                id: id("1"),
                validator: None,
                bytes: b"{}".to_vec(),
                view_leaves: vec!["/prints/1/print.json".to_string()],
            }],
            fs: vec![wit_types::FsWrite {
                id: of.map(id),
                path: "/prints/1/print.json".to_string(),
                kind: wit_types::FsKind::File(file_out(
                    size,
                    wit_types::ByteSource::Deferred(wit_types::ReadMode::Full),
                    wit_types::Stability::Stable,
                )),
            }],
            ..effects()
        };
        let valid =
            |effects: &wit_types::Effects| validate_feed(&part, effects, false, false, |_| false);
        assert_eq!(
            valid(&leaf(Some("1"), wit_types::FileSize::Unknown)),
            Ok(())
        );
        for other in [
            leaf(None, wit_types::FileSize::Unknown),
            leaf(Some("1"), wit_types::FileSize::Exact(2)),
            wit_types::Effects {
                canonical: Vec::new(),
                ..leaf(Some("1"), wit_types::FileSize::Unknown)
            },
        ] {
            let error = valid(&other).unwrap_err();
            assert!(
                error.contains("a fed file without bytes is a view leaf"),
                "{error}"
            );
        }
        // Another id at the path of a view leaf is the conflict of each return.
        assert!(valid(&leaf(Some("2"), wit_types::FileSize::Unknown)).is_err());
    }
}

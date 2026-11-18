use super::*;

/// Throttled progress.
pub(super) struct Progress {
    pub(super) report: Box<dyn Fn(u64, u64) + Send>,
    pub(super) total: u64,
    pub(super) last: Option<Instant>,
}

impl Progress {
    pub(super) fn tick(&mut self, done: u64) {
        if done == self.total || self.last.is_none_or(|at| at.elapsed() >= PROGRESS_INTERVAL) {
            self.last = Some(Instant::now());
            (self.report)(done, self.total);
        }
    }
}

use mpmp_storage::TagPublish;
use std::fs::File;

use tag_edit::delta::{Delta, MAGIC, PAIR_MAGIC, pair, split_pair};
use tag_edit::snapshot::{ByteUndo, Layout, digest, projected_digest};

#[derive(Clone, Copy)]
pub(super) enum Mode<'a> {
    Edit(&'a [TagChange]),
    Restore(&'a ByteUndo),
    Legacy(&'a FileTags),
}

impl<'a> Mode<'a> {
    pub(super) fn undo(file: &'a FileTags) -> Self {
        match file.exact.as_deref() {
            Some(exact) => Self::Restore(exact),
            None => Self::Legacy(file),
        }
    }

    pub(super) fn verify(
        &self,
        source: &mut File,
        hash: Option<[u8; 32]>,
    ) -> std::result::Result<(), Rejected> {
        match self.check(source, hash) {
            Ok(true) => Ok(()),
            Err(LibraryError::Io(error)) if is_file_in_use(&error) => Err(error.into()),
            _ => Err(Rejected::Changed),
        }
    }

    fn check(&self, source: &mut File, hash: Option<[u8; 32]>) -> Result<bool> {
        match self {
            Self::Restore(exact) => {
                let hash = match hash {
                    Some(hash) => hash,
                    None => digest(source)?,
                };
                Ok(hash == exact.after)
            }
            Self::Legacy(old) if let Some(expected) = old.protected_hash => {
                use sha2::{Digest, Sha256};
                let guarded = if old.raw_id3.is_some() {
                    Sha256::digest(tag_edit::raw::read(source)?).into()
                } else {
                    crate::cover::picture_guard_open(source.try_clone()?)?
                };
                Ok(guarded == expected)
            }
            _ => Ok(true),
        }
    }
}

pub(super) fn volume(
    inner: &Inner,
    path: &Path,
    meta: &std::fs::Metadata,
) -> std::result::Result<TagPublish, Rejected> {
    if meta.permissions().readonly() {
        return Err(Rejected::Code("file_read_only"));
    }
    let parent = path.parent().ok_or(Rejected::Code("location_changed"))?;
    inner
        .storage
        .replacements
        .supports_tag_publish(path)
        .map_err(|error| {
            tracing::warn!(%error, path = %parent.display(), "cannot safely publish tags here");
            match error.kind() {
                _ if is_file_in_use(&error) => Rejected::Code("file_in_use"),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem => {
                    Rejected::Code("file_read_only")
                }
                _ => Rejected::Code("not_undoable"),
            }
        })
}

pub(super) struct Prepared {
    pub exact: ByteUndo,
    pub bytes: usize,
    pub encoding_conversion: bool,
    physical: bool,
    pub changed_keys: Vec<String>,
}

pub(super) enum Outcome {
    Written {
        record: Option<Box<TrackRecord>>,
        reverse: FileTags,
        intent: i64,
        retain: bool,
        physical: bool,
        changed_keys: Vec<String>,
        bytes: usize,
        encoding_conversion: bool,
    },
    Unchanged,
    Skip(&'static str),
    Fail(&'static str),
}

#[derive(Debug)]
pub(super) enum Rejected {
    Changed,
    RetryVolume,
    Code(&'static str),
    Io(std::io::Error),
}

impl From<std::io::Error> for Rejected {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl Rejected {
    pub(super) fn code(&self) -> &'static str {
        match self {
            Self::Changed => "file_changed",
            Self::RetryVolume => "not_undoable",
            Self::Code(code) => code,
            Self::Io(error) if is_file_in_use(error) => "file_in_use",
            Self::Io(_) => "write_failed",
        }
    }
}

pub(super) fn is_file_in_use(error: &std::io::Error) -> bool {
    // Win32 ERROR_SHARING_VIOLATION and ERROR_LOCK_VIOLATION. Raw codes have
    // different meanings on other platforms.
    cfg!(windows) && matches!(error.raw_os_error(), Some(32 | 33))
}

fn snapshot_error(error: std::io::Error) -> Rejected {
    if is_file_in_use(&error) {
        Rejected::Code("file_in_use")
    } else if error.kind() == std::io::ErrorKind::FileTooLarge {
        Rejected::Code("undo_too_large")
    } else {
        tracing::warn!(%error, "cannot prove lossless tag restoration");
        Rejected::Code("not_undoable")
    }
}

pub(super) fn open_source(
    storage: &dyn mpmp_storage::LibraryStorage,
    path: &Path,
    checked: bool,
) -> std::result::Result<File, Rejected> {
    storage_access::open_tag_source(storage, path, checked).map_err(|error| match error {
        LibraryError::Invalid {
            why: crate::Invalid::LocationChanged,
            ..
        } => Rejected::Code("location_changed"),
        LibraryError::Io(error) if is_file_in_use(&error) => Rejected::Io(error),
        _ => {
            tracing::warn!(%error, "cannot open a stable tag source");
            Rejected::Code("open_failed")
        }
    })
}

pub(super) struct Plan {
    forward: Delta,
    inverse: Vec<u8>,
    before: TagValues,
    after: TagValues,
    bytes: usize,
    encoding_conversion: bool,
    changed_keys: Vec<String>,
    physical: bool,
    check_samples: bool,
}

pub(super) fn plan(
    source: &mut File,
    forced: Option<&'static Encoding>,
    mode: &Mode<'_>,
    limit: usize,
) -> std::result::Result<Plan, Rejected> {
    // A recognizable refused container is classified before the general tag reader.
    tag_edit::plan::classify(source).map_err(snapshot_error)?;
    let mut edited = tag_edit::open_file(source.try_clone()?)
        .map_err(open_code)
        .map_err(Rejected::Code)?;
    let before = edited.shown(forced);
    let decoding = edited.decoding(forced);
    let was_legacy = edited.converts_encoding(decoding);
    let file_type = edited.file_type;
    let mut changed_keys = Vec::new();
    let mut physical = !matches!(mode, Mode::Restore(exact) if exact.before == exact.after);
    let changes = match mode {
        Mode::Edit(changes) => Some(changes.to_vec()),
        Mode::Legacy(old) if old.raw_id3.is_none() => Some(tag_edit::reverse_changes(&old.values)),
        _ => None,
    };
    if let Some(changes) = &changes {
        let keys: Vec<_> = changes.iter().map(TagChange::key).collect();
        let old = edited.primary_values(&keys, decoding);
        match edited.apply_verified(changes, decoding) {
            Ok(false) if matches!(mode, Mode::Legacy(_)) => physical = false,
            Ok(false) => {
                return Ok(Plan {
                    forward: Delta::default(),
                    inverse: Vec::new(),
                    after: before.clone(),
                    before,
                    bytes: 0,
                    encoding_conversion: false,
                    changed_keys,
                    physical: false,
                    check_samples: false,
                });
            }
            Ok(true) => {}
            Err(message) => {
                tracing::warn!(%message, "tag values cannot be stored");
                return Err(Rejected::Code("invalid_tag"));
            }
        }
        let new = edited.primary_values(&keys, edited.decoding(forced));
        changed_keys = old
            .iter()
            .zip(&new)
            .filter(|((_, old), (_, new))| old != new)
            .map(|((key, _), _)| key.clone())
            .collect();
    }
    let existing_pair = match mode {
        Mode::Restore(exact) if exact.image.starts_with(PAIR_MAGIC) => {
            Some(split_pair(&exact.image).map_err(snapshot_error)?)
        }
        _ => None,
    };
    let forward = match mode {
        Mode::Restore(_) if let Some((undo, _)) = existing_pair => {
            Delta::decode(undo).map_err(snapshot_error)?
        }
        Mode::Restore(exact) if exact.image.starts_with(MAGIC) => {
            Delta::decode(&exact.image).map_err(snapshot_error)?
        }
        Mode::Restore(exact) => Layout::read(source, file_type)
            .and_then(|layout| layout.translate(&exact.image))
            .map_err(snapshot_error)?,
        Mode::Legacy(old) if old.raw_id3.is_some() => {
            let raw = old.raw_id3.as_deref().unwrap();
            let payload = tag_edit::raw::payload(raw).map_err(snapshot_error)?;
            let regions =
                tag_edit::raw::regions(source, payload, Some(raw)).map_err(snapshot_error)?;
            Delta::from_regions(source, regions, None).map_err(snapshot_error)?
        }
        _ if !physical => Delta::from_regions(source, Vec::new(), None).map_err(snapshot_error)?,
        _ => edited.plan(source).map_err(snapshot_error)?,
    };
    let literal_limit = tag_edit::MAX_TAG_UNDO_BYTES.saturating_sub(128);
    let inverse = if let Some((undo, redo)) = existing_pair {
        pair(redo, undo)
    } else {
        let backward = forward
            .invert(source, literal_limit)
            .and_then(|undo| undo.encoded(literal_limit))
            .map_err(snapshot_error)?;
        let forward_bytes = forward.encoded(literal_limit).map_err(snapshot_error)?;
        pair(&backward, &forward_bytes)
    };
    let bytes = 128 + inverse.len();
    if bytes > limit {
        return Err(Rejected::Code("undo_too_large"));
    }
    let projected = forward.reader(source)?;
    let reread = tag_edit::open_file(projected).map_err(|_| Rejected::Code("not_undoable"))?;
    if reread.file_type != file_type || changes.is_some() && !edited.same_values(&reread, decoding)
    {
        return Err(Rejected::Code("not_undoable"));
    }
    let encoding_conversion = match mode {
        Mode::Edit(_) => was_legacy,
        _ => was_legacy && !reread.converts_encoding(reread.decoding(forced)),
    };
    Ok(Plan {
        forward,
        inverse,
        bytes,
        before,
        after: reread.shown(forced),
        encoding_conversion,
        changed_keys,
        physical,
        check_samples: matches!(file_type, lofty::file::FileType::Mp4)
            && !matches!(mode, Mode::Restore(_)),
    })
}

impl Plan {
    pub(super) fn physical(&self) -> bool {
        self.physical
    }
    pub(super) fn preview(self) -> (TagValues, TagValues, usize, bool) {
        (
            self.before,
            self.after,
            self.bytes,
            self.encoding_conversion,
        )
    }

    fn stage(
        self,
        source: &mut File,
        candidate: &mut File,
        mode: &Mode<'_>,
        before: [u8; 32],
    ) -> std::result::Result<Option<Prepared>, Rejected> {
        if self.bytes == 0 {
            return Ok(None);
        }
        let after = if self.physical {
            tag_edit::snapshot::render_digest(&self.forward, source, candidate)
        } else {
            projected_digest(&self.forward, source)
        }
        .map_err(snapshot_error)?;
        if self.physical && self.check_samples {
            tag_edit::plan::validate_samples(source, candidate).map_err(snapshot_error)?;
        }
        if let Mode::Restore(exact) = mode
            && after != exact.before
        {
            if digest(source)? != before {
                return Err(Rejected::Changed);
            }
            return Err(Rejected::Code("not_undoable"));
        }
        let (undo, _) = split_pair(&self.inverse).map_err(snapshot_error)?;
        let inverse = Delta::decode(undo).map_err(snapshot_error)?;
        let inverse_source = if self.physical {
            &mut *candidate
        } else {
            &mut *source
        };
        if projected_digest(&inverse, inverse_source).map_err(snapshot_error)? != before {
            return Err(if digest(source)? != before {
                Rejected::Changed
            } else {
                Rejected::Code("not_undoable")
            });
        }
        Ok(Some(Prepared {
            bytes: self.bytes,
            encoding_conversion: self.encoding_conversion,
            physical: self.physical,
            changed_keys: self.changed_keys,
            exact: ByteUndo {
                before,
                after,
                image: self.inverse,
            },
        }))
    }
}

#[derive(Clone, Copy)]
pub(super) struct FileWrite<'a> {
    pub file: &'a FileRef,
    pub path: &'a Path,
    pub found: (u64, i64),
    pub mode: Mode<'a>,
    pub group: &'a intents::Group,
    pub undo_left: usize,
}

pub(super) fn edit_file(inner: &Inner, request: FileWrite<'_>) -> Result<Outcome> {
    let path = request.path;
    let mut result = publish(inner, request)?;
    if matches!(result, Err(Rejected::RetryVolume)) {
        result = publish(inner, request)?;
    }
    match result {
        Ok(outcome) => Ok(outcome),
        Err(Rejected::Changed) => Ok(Outcome::Skip("file_changed")),
        Err(rejected) => {
            if let Rejected::Io(error) = &rejected {
                tracing::warn!(%error, path = %path.display(), "cannot write a file's tags");
            }
            let code = rejected.code();
            Ok(match code {
                "file_in_use" | "unsupported_format" => Outcome::Skip(code),
                _ => Outcome::Fail(code),
            })
        }
    }
}

fn publish(
    inner: &Inner,
    request: FileWrite<'_>,
) -> Result<std::result::Result<Outcome, Rejected>> {
    let FileWrite {
        file,
        path,
        found,
        mode,
        group,
        undo_left,
    } = request;
    let storage = inner.storage.files.as_ref();
    let setup = (|| {
        let mut original = open_source(storage, path, group.checked())?;
        let meta = original.metadata()?;
        if stamp(&meta) != found {
            return Err(Rejected::Changed);
        }
        let baseline =
            intents::inspect_open(storage, path, &mut original).map_err(|e| match e {
                LibraryError::Io(error) => error,
                _ => std::io::Error::other(e),
            })?;
        if baseline.stamp != found {
            return Err(Rejected::Changed);
        }
        mode.verify(&mut original, Some(baseline.hash))?;
        let forced = file.root.as_ref().and_then(|(_, encoding)| *encoding);
        let plan = plan(&mut original, forced, &mode, undo_left)?;
        if plan.bytes == 0 {
            return Ok(None);
        }
        let protocol = plan
            .physical
            .then(|| volume(inner, path, &meta))
            .transpose()?;
        let parent = path.parent().ok_or(Rejected::Code("location_changed"))?;
        let parent_identity = storage.directory_identity(parent)?.parts();
        let temp = tempfile::Builder::new()
            .prefix(".player-tags-")
            .suffix(".tags.tmp")
            .tempfile_in(parent)?;
        let tx = inner
            .storage
            .replacements
            .begin(intents::replacement_paths(path, temp.path()))
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let displaced = protocol.map(|protocol| match protocol {
            TagPublish::Exchange => temp.path().to_owned(),
            _ => temp.path().with_extension("backup"),
        });
        let target = intents::Target {
            path: path.to_owned(),
            staged: temp.path().to_owned(),
            staged_identity: storage.opened_identity(temp.as_file())?.parts(),
            original_identity: baseline.id,
            track: file.track.id,
            root: file.track.root_id,
            stamp: found,
            undo_of: group.undo_of,
            changed: plan.changed_keys.clone(),
            displaced,
            exclusive: matches!(
                protocol,
                Some(TagPublish::Exclusive { .. } | TagPublish::Link)
            ),
            link_fallback: protocol == Some(TagPublish::Link),
            parent_identity: Some(parent_identity),
            stable_ids: !matches!(
                protocol,
                Some(
                    TagPublish::Replace { stable_ids: false }
                        | TagPublish::Exclusive { stable_ids: false }
                )
            ),
            charge: plan.bytes,
        };
        Ok(Some((original, plan, temp, target, tx, baseline)))
    })();
    let (mut original, plan, mut temp, target, tx, baseline) = match setup {
        Ok(Some(setup)) => setup,
        Ok(None) => return Ok(Ok(Outcome::Unchanged)),
        Err(error) => return Ok(Err(error)),
    };
    let intent = intents::register(inner, group, &target)?;
    temp.disable_cleanup(true);
    intents::fault(inner, intents::Point::Building)?;
    let staged = (|| {
        #[cfg(all(test, feature = "desktop-fs"))]
        if let Some(hook) = inner.hooks.tag_after_plan.lock().unwrap().as_ref() {
            hook(path);
        }
        if !intents::unchanged(storage, path, &baseline) {
            return Err(Rejected::Changed);
        }
        let prepared = plan
            .stage(&mut original, temp.as_file_mut(), &mode, baseline.hash)?
            .expect("unchanged plans already returned");
        if prepared.physical {
            tx.prepare_attributes(temp.path(), path)?;
            temp.as_file().sync_all()?;
        }
        drop(temp);
        Ok::<_, Rejected>((prepared, stamp(&std::fs::metadata(&target.staged)?)))
    })();
    let (prepared, candidate_stamp) = match staged {
        Ok(result) => result,
        Err(err) => {
            intents::clean_unpublished(inner, intent, &target, None)?;
            return Ok(Err(err));
        }
    };
    let mut reverse = FileTags {
        track: file.track.id.0,
        size: if prepared.physical {
            candidate_stamp.0
        } else {
            found.0
        },
        mtime_ms: if prepared.physical {
            candidate_stamp.1
        } else {
            found.1
        },
        values: Vec::new(),
        raw_id3: None,
        protected_hash: None,
        exact: Some(Box::new(prepared.exact)),
    };
    if !intents::prepare(inner, intent, &target, &reverse)? {
        let hash = reverse.exact.as_ref().map(|exact| exact.after);
        intents::clean_unpublished(inner, intent, &target, hash)?;
        return Ok(Err(Rejected::Code("undo_too_large")));
    }
    intents::fault(inner, intents::Point::Prepared)?;
    let exact = reverse.exact.as_ref().unwrap();
    if !prepared.physical {
        let current = intents::inspect_file(storage, path)?;
        if current.id != target.original_identity || current.hash != exact.before {
            intents::clean_unpublished(inner, intent, &target, None)?;
            return Ok(Err(Rejected::Changed));
        }
        let retain = !intents::remove_unpublished(storage, &target, None)?;
        intents::mark(inner, intent, "replaced")?;
        return Ok(Ok(Outcome::Written {
            record: reread_record(inner, file, path, current.stamp),
            reverse,
            intent,
            retain,
            changed_keys: prepared.changed_keys,
            bytes: prepared.bytes,
            encoding_conversion: prepared.encoding_conversion,
            physical: false,
        }));
    }
    let displaced = target.displaced.as_ref().unwrap();
    if let Err(err) = intents::check_source(
        storage,
        &original,
        &target,
        candidate_stamp,
        group.checked(),
    ) {
        intents::clean_unpublished(inner, intent, &target, Some(exact.after))?;
        return Ok(Err(err));
    }
    // Check the opened source against the pre-parse digest before any namespace
    // operation. A filesystem may preserve its file stamp across an in-place write.
    match digest(&mut original) {
        Ok(hash) if hash == exact.before => {}
        result => {
            drop(original);
            intents::clean_unpublished(inner, intent, &target, Some(exact.after))?;
            return Ok(Err(result.err().map_or(Rejected::Changed, Rejected::Io)));
        }
    }
    #[cfg(all(test, feature = "desktop-fs"))]
    if let Some(hook) = inner.hooks.tag_before_publish.lock().unwrap().as_ref() {
        hook(&target.path, &target.staged);
    }
    let verified = match intents::inspect_file(storage, &target.staged) {
        Ok(verified) => verified,
        Err(error) => return Ok(Err(std::io::Error::other(error).into())),
    };
    if verified.hash != exact.after || verified.id != target.staged_identity {
        intents::retain(inner, intent, &target)?;
        return Ok(Err(Rejected::Changed));
    }
    intents::mark(inner, intent, "publishing")?;
    intents::fault(inner, intents::Point::Publishing)?;
    if !intents::unchanged(storage, &target.staged, &verified) {
        intents::retain(inner, intent, &target)?;
        return Ok(Err(Rejected::Changed));
    }
    if let Err(error) = target.check_parent(storage) {
        intents::retain(inner, intent, &target)?;
        return Ok(Err(error.into()));
    }
    let _namespace = inner.changing_file_names();
    if target.exclusive {
        let before_move = original.metadata()?;
        intents::mark(inner, intent, "displacing")?;
        intents::fault(inner, intents::Point::Displacing)?;
        let displaced_source = (|| -> Result<bool> {
            tx.displace_tag(path, displaced, target.protocol())?;
            let previous = std::fs::symlink_metadata(displaced)?;
            let still_open = original.metadata()?;
            let id = storage.entry_identity(displaced, &previous)?.parts();
            // Rename can itself change ctime. Compare the two views of the
            // displaced inode, and require the pre-move mtime and size.
            Ok(target.moved_id(id, target.original_identity)
                && intents::same_metadata(&still_open, &previous)
                && before_move.modified()? == previous.modified()?
                && before_move.len() == previous.len()
                && intents::unchanged(storage, &target.staged, &verified))
        })();
        if !matches!(displaced_source, Ok(true)) {
            intents::recover_missing(inner, &target, tx.as_ref(), false)?;
            if displaced.exists() {
                intents::retain(inner, intent, &target)?;
            } else {
                intents::clean_unpublished(inner, intent, &target, Some(exact.after))?;
            }
            let result = match displaced_source {
                Err(LibraryError::Io(error)) if unsupported_protocol(&error) => {
                    inner
                        .storage
                        .replacements
                        .invalidate_tag_publish(path.parent().unwrap());
                    Rejected::RetryVolume
                }
                Err(LibraryError::Io(error)) => Rejected::Io(error),
                Err(error) => std::io::Error::other(error).into(),
                _ => Rejected::Changed,
            };
            return Ok(Err(result));
        }
        intents::mark(inner, intent, "displaced")?;
        intents::fault(inner, intents::Point::DisplaceGap)?;
        #[cfg(all(test, feature = "desktop-fs"))]
        if let Some(hook) = inner.hooks.tag_exclusive_gap.lock().unwrap().as_ref() {
            hook(path);
        }
        if !intents::staged_present(storage, &target) {
            intents::recover_missing(inner, &target, tx.as_ref(), false)?;
            intents::clean_unpublished(inner, intent, &target, Some(exact.after))?;
            return Ok(Err(Rejected::Changed));
        }
        if let Err(error) = target.check_parent(storage) {
            intents::retain(inner, intent, &target)?;
            return Ok(Err(error.into()));
        }
        intents::mark(inner, intent, "installing")?;
        intents::fault(inner, intents::Point::Installing)?;
    }
    let result = tx.publish_tag(target.protocol());
    let published_source = if let Err(error) = &result {
        drop(original);
        if !target.exclusive
            && let Err(recovery) = intents::recover_missing(inner, &target, tx.as_ref(), false)
        {
            tracing::warn!(%error, %recovery, "immediate tag recovery could not restore the name");
            return Ok(Err(std::io::Error::other(recovery).into()));
        }
        None
    } else {
        #[cfg(all(test, feature = "desktop-fs"))]
        if let Some(hook) = inner.hooks.tag_after_publish.lock().unwrap().as_ref() {
            hook(path, displaced);
        }
        intents::fault(inner, intents::Point::AfterSyscall)?;
        Some(original)
    };
    let settled = match intents::settle(
        inner,
        intent,
        &target,
        tx.as_ref(),
        exact,
        if target.exclusive {
            "installing"
        } else {
            "publishing"
        },
        published_source,
    ) {
        Ok(settled) => settled,
        Err(
            error @ (LibraryError::Io(_) | LibraryError::Storage(_) | LibraryError::Invalid { .. }),
        ) => return Ok(Err(std::io::Error::other(error).into())),
        Err(error) => return Err(error),
    };
    let intents::Resolution::Complete { current, retain } = settled else {
        return Ok(Err(match result {
            Ok(()) if matches!(settled, intents::Resolution::Unverified) => {
                Rejected::Code("write_failed")
            }
            Ok(()) => Rejected::Changed,
            Err(error) if unsupported_protocol(&error) => {
                inner
                    .storage
                    .replacements
                    .invalidate_tag_publish(path.parent().unwrap());
                Rejected::RetryVolume
            }
            Err(error) => error.into(),
        }));
    };
    let record = current.and_then(|stamp| {
        (reverse.size, reverse.mtime_ms) = stamp;
        reread_record(inner, file, path, stamp)
    });
    Ok(Ok(Outcome::Written {
        record,
        reverse,
        intent,
        retain,
        physical: true,
        changed_keys: prepared.changed_keys,
        bytes: prepared.bytes,
        encoding_conversion: prepared.encoding_conversion,
    }))
}

fn unsupported_protocol(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::Unsupported | std::io::ErrorKind::InvalidInput
    )
}

/// One English sentence for the action: `edit the tags of 3 files: set ARTIST, remove
/// COMMENT`.
pub(super) fn summary(files: usize, changes: &[TagChange]) -> String {
    const SHOWN: usize = 5;
    let mut parts: Vec<String> = changes
        .iter()
        .take(SHOWN)
        .map(|c| match c {
            TagChange::Set { key, .. } => format!("set {}", tag_edit::display_key(key)),
            TagChange::Remove { key } => format!("remove {}", tag_edit::display_key(key)),
        })
        .collect();
    if changes.len() > SHOWN {
        parts.push(format!("and {} more", changes.len() - SHOWN));
    }
    format!(
        "edit the tags of {}: {}",
        journal::count(files, "file"),
        parts.join(", ")
    )
}

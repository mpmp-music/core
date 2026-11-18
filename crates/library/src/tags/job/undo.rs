use super::*;

pub(super) fn run_undo(
    inner: &Inner,
    id: ActionId,
    meta: &OwnedMeta,
    options: TagJobOptions,
) -> Result<TagEditReport> {
    let _one_at_a_time = inner.tag_jobs.lock().unwrap_or_else(|e| e.into_inner());
    crate::cover::job::recover_inner(inner)?;
    inner.pool.read(intents::require_readable)?;
    let now = now_ms();
    let (description, files) = inner.pool.read(|conn| {
        let read = conn.unchecked_transaction()?;
        let description = if let Some(expected) = &options.undo_guard {
            if expected.action != id || !expected.writes_files {
                return Err(LibraryError::Corrupt(
                    "inconsistent file undo description".into(),
                ));
            }
            journal::check_undo_description(&read, expected, now)?;
            expected.clone()
        } else {
            journal::describe_undo(&read, id, now)?
        };
        let files = journal::tag_undo(&read, id, now)?.ok_or_else(|| {
            LibraryError::not_found(NotFound::Action, format!("action {id} is not a tag edit"))
        })?;
        Ok((description, files))
    })?;
    let checked = meta.meta().checked;
    let mut group = intents::Group::new(meta, Some(id), &[])?;
    group.legacy = files.iter().any(|f| f.exact.is_none());
    group.expected = Some(description);
    let ids: Vec<TrackId> = files.iter().map(|f| TrackId(f.track)).collect();
    let refs: HashMap<TrackId, FileRef> = file_refs(inner, &ids)?
        .into_iter()
        .map(|f| (f.track.id, f))
        .collect();
    let empty = HashMap::new();
    let locations = checked.then(|| options.checked.as_ref().unwrap_or(&empty));
    // A conflict in this preflight prevents all writes. Later failures can leave a remainder.
    let mut changed = Vec::new();
    let mut content_changed = Vec::new();
    let mut in_use = Vec::new();
    let mut external_in_use = Vec::new();
    let mut located = Vec::with_capacity(files.len());
    let mut accesses = Vec::with_capacity(files.len());
    for undo in &files {
        let track = TrackId(undo.track);
        if crate::cover::job::pending(inner, track)? {
            return Err(LibraryError::CoverRecoveryRequired);
        }
        let Some(file) = refs.get(&track) else {
            changed.push(track);
            continue;
        };
        let access = inner
            .file_access
            .acquire(file.track.root_id)
            .ok_or(LibraryError::RootReleased(file.track.root_id))?;
        accesses.push(access);
        match locate(inner.storage.files.as_ref(), file, locations) {
            Located::File { path, stamp } if stamp == (undo.size, undo.mtime_ms) => {
                if intents::pending(inner, track, &path)? {
                    content_changed.push((track, "file_changed"));
                    continue;
                }
                match verify_source(inner, file, &path, checked, undo) {
                    Ok(()) => {}
                    Err(rejected) if rejected.code() == "file_in_use" => {
                        external_in_use.push(track);
                        continue;
                    }
                    _ => {
                        content_changed.push((track, "file_changed"));
                        continue;
                    }
                }
                if (options.in_use)(track) {
                    in_use.push(track);
                }
                located.push((file, path, stamp, undo));
            }
            Located::Fail("location_changed") => {
                return Err(storage_access::location_changed(Path::new(
                    &file.track.rel_path,
                )));
            }
            Located::Skip("file_in_use") => external_in_use.push(track),
            _ => changed.push(track),
        }
    }
    if !changed.is_empty() {
        changed.sort_unstable();
        return Err(LibraryError::ActionConflict {
            action: id,
            changed: changed
                .into_iter()
                .map(|id| journal::ActionTarget::Track { id })
                .collect(),
        });
    }
    if !external_in_use.is_empty() {
        external_in_use.extend(in_use);
        return Err(LibraryError::ExternalInUse(external_in_use));
    }
    if !in_use.is_empty() {
        return Err(LibraryError::InUse(in_use));
    }
    if !content_changed.is_empty() {
        return Ok(TagEditReport {
            skipped: content_changed,
            ..TagEditReport::default()
        });
    }
    let mut progress = Progress {
        report: options.progress,
        total: located.len() as u64,
        last: None,
    };
    progress.tick(0);
    let mut report = TagEditReport::default();
    let mut published = Vec::new();
    let mut undo_left = tag_edit::MAX_TAG_JOB_BYTES;
    let mut storage_left = inner.pool.read(|conn| intents::budget(conn, true))?;
    for (i, (file, path, stamp, undo)) in located.into_iter().enumerate() {
        let track = file.track.id;
        match edit_file(
            inner,
            FileWrite {
                file,
                path: &path,
                found: stamp,
                mode: Mode::undo(undo),
                group: &group,
                undo_left: undo_left.min(storage_left),
            },
        )? {
            Outcome::Written {
                record,
                reverse: file,
                intent,
                retain,
                physical,
                bytes,
                encoding_conversion,
                ..
            } => {
                if encoding_conversion {
                    report.encoding_converted.push(track);
                }
                if physical {
                    report.written.push(track);
                }
                report.completed.push(track);
                undo_left -= bytes;
                storage_left -= bytes;
                published.push(intents::Published {
                    intent,
                    file,
                    record,
                    retain,
                });
            }
            Outcome::Unchanged => report.skipped.push((track, "file_changed")),
            Outcome::Skip(code) => report.skipped.push((track, code)),
            Outcome::Fail(code) => report.failed.push((track, code)),
        }
        progress.tick(i as u64 + 1);
    }
    if !published.is_empty() {
        intents::fault(inner, intents::Point::BeforeAction)?;
        let storage = inner.storage.files.clone();
        report.action = Some(inner.writer.run_data(move |conn| {
            intents::finish(storage.as_ref(), conn, &group, published, &HashSet::new())
        })?);
        intents::fault(inner, intents::Point::AfterAction)?;
    }
    Ok(report)
}

fn verify_source(
    inner: &Inner,
    file: &FileRef,
    path: &Path,
    checked: bool,
    undo: &FileTags,
) -> std::result::Result<(), Rejected> {
    let mut source = open_source(inner.storage.files.as_ref(), path, checked)?;
    let mode = Mode::undo(undo);
    mode.verify(&mut source, None)?;
    if cfg!(windows) {
        let probe = (|| {
            // Exact undo records already say whether the bytes change. Legacy
            // inverses need the same planning as preview to detect a no-op.
            let physical = if let Some(exact) = &undo.exact {
                exact.before != exact.after
            } else {
                let forced = file.root.as_ref().and_then(|(_, f)| *f);
                plan(&mut source, forced, &mode, tag_edit::MAX_TAG_JOB_BYTES)?.physical()
            };
            if physical {
                write::volume(inner, path, &source.metadata()?)?;
            }
            Ok::<_, Rejected>(())
        })();
        // Keep other planning and volume errors in the per-file report.
        if let Err(error) = probe
            && error.code() == "file_in_use"
        {
            return Err(error);
        }
    }
    Ok(())
}

/// The tracks whose files undoing action `id` writes (empty: none, it is no tag edit).
pub(crate) fn tracks_of(inner: &Inner, id: ActionId) -> Result<Vec<TrackId>> {
    inner.pool.read(|conn| journal::tag_tracks(conn, id))
}

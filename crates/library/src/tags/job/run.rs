use super::*;

pub(super) fn run_edit(
    inner: &Inner,
    files: Vec<FileRef>,
    changes: &[TagChange],
    meta: &OwnedMeta,
    options: TagJobOptions,
) -> Result<TagEditReport> {
    let _one_at_a_time = inner.tag_jobs.lock().unwrap_or_else(|e| e.into_inner());
    crate::cover::job::recover_inner(inner)?;
    inner.pool.read(intents::require_readable)?;
    let checked = meta.meta().checked;
    let mut progress = Progress {
        report: options.progress,
        total: files.len() as u64,
        last: None,
    };
    progress.tick(0);
    let mut report = TagEditReport::default();
    let group = intents::Group::new(meta, None, changes)?;
    let mut published = Vec::new();
    let mut leases = Vec::new();
    let mut changed_keys = HashSet::new();
    let mut undo_left = tag_edit::MAX_TAG_JOB_BYTES;
    let mut storage_left = inner.pool.read(|conn| intents::budget(conn, false))?;
    // Checked, but no locations given: nothing was checked, nothing is written.
    let empty = HashMap::new();
    let locations = checked.then(|| options.checked.as_ref().unwrap_or(&empty));
    for (i, file) in files.iter().enumerate() {
        let id = file.track.id;
        let access = inner.file_access.acquire(file.track.root_id);
        let outcome = if crate::cover::job::pending(inner, id)? {
            Outcome::Skip("cover_recovery_required")
        } else if access.is_none() {
            Outcome::Skip("root_released")
        } else {
            match locate(inner.storage.files.as_ref(), file, locations) {
                Located::Skip(code) => Outcome::Skip(code),
                Located::Fail(code) => Outcome::Fail(code),
                Located::File { stamp, .. } if stamp != (file.track.size, file.track.mtime_ms) => {
                    Outcome::Skip("file_changed")
                }
                Located::File { .. } if (options.in_use)(id) => Outcome::Skip("file_in_use"),
                Located::File { path, .. } if intents::pending(inner, id, &path)? => {
                    Outcome::Skip("file_changed")
                }
                Located::File { path, stamp } => edit_file(
                    inner,
                    FileWrite {
                        file,
                        path: &path,
                        found: stamp,
                        mode: Mode::Edit(changes),
                        group: &group,
                        undo_left: undo_left.min(storage_left),
                    },
                )?,
            }
        };
        match outcome {
            Outcome::Written {
                record,
                reverse: file,
                intent,
                retain,
                physical,
                bytes,
                encoding_conversion,
                changed_keys: file_keys,
            } => {
                changed_keys.extend(file_keys);
                if encoding_conversion {
                    report.encoding_converted.push(id);
                }
                if physical {
                    report.written.push(id);
                }
                report.completed.push(id);
                undo_left -= bytes;
                storage_left -= bytes;
                published.push(intents::Published {
                    intent,
                    file,
                    record,
                    retain,
                });
                leases.extend(access);
            }
            Outcome::Unchanged => {}
            Outcome::Skip(code) => report.skipped.push((id, code)),
            Outcome::Fail(code) => report.failed.push((id, code)),
        }
        progress.tick(i as u64 + 1);
    }
    if !published.is_empty() {
        intents::fault(inner, intents::Point::BeforeAction)?;
        let storage = inner.storage.files.clone();
        report.action = Some(inner.writer.run_data(move |conn| {
            intents::finish(storage.as_ref(), conn, &group, published, &changed_keys)
        })?);
        intents::fault(inner, intents::Point::AfterAction)?;
    }
    Ok(report)
}

/// Updates the rows of the files written (not of a root removed meanwhile) and records the
/// action, in one transaction.
pub(super) fn commit(
    storage: &dyn mpmp_storage::LibraryStorage,
    conn: &mut rusqlite::Connection,
    records: &[TrackRecord],
    pending: &[TrackId],
    record: impl FnOnce(&rusqlite::Transaction<'_>, i64) -> Result<ActionId>,
) -> Result<ActionId> {
    const UPDATE_TRACKS: &str = r#"
UPDATE tracks SET artists_parser_version=0, metadata_revision=metadata_revision+1
WHERE id=?1
"#;

    let tx = conn.transaction()?;
    let now = now_ms();
    for r in records {
        let active: Option<bool> = tx
            .prepare_cached("SELECT removed = 0 FROM roots WHERE id = ?1")?
            .query_row([r.root_id.0], |row| row.get(0))
            .ok();
        if active == Some(true) {
            crate::loudness::upsert_retagged(storage, &tx, r, now)?;
        }
    }
    for id in pending {
        tx.execute(UPDATE_TRACKS, [id.0])?;
    }
    schema::refresh_aggregates(&tx)?;
    let action = record(&tx, now)?;
    tx.commit()?;
    Ok(action)
}

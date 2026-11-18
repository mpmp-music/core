//! Library adapters for each catalog domain and asynchronous scan and edit bridges.

mod actions;
mod agent_permissions;
mod browse;
mod catalog_checks;
mod library_roots;
mod loudness;
mod media_resources;
mod playback_history;
mod playlists;
mod tags_and_covers;

use super::convert::{
    action_id, action_meta, context_summary, contract_ids, cover_change, cover_preview,
    cover_results, library_ids, lyrics_info, recorded, sat_u32, scan_error_code, track_info,
    track_query,
};
use super::errors::{
    album_not_found, library_error, playlist_not_found, root_not_found, undo_error,
};
use super::playlists::playlist_error;
use super::query::RunQuery;
use super::{
    Actions, AgentPermissions, Browse, CatalogChecks, LibraryRoots, Loudness, MediaResources,
    PlaybackHistory, TagsAndCovers,
};
use crate::catalog::{
    CoverArt, FileVersion, PlayRecord, Recorded, ScanEvent, ScanTicket, TagFiles, TagJobEvent,
    Undone, check_tracks_exist, track_not_found,
};
use crate::contract::{
    self, ActionId, Command, ContractError, Effect, EndReason, ErrorCode, Expiry, Grant, GrantId,
    JobOutcome, JobReport, MAX_CONTEXT_TRACKS, MAX_TAG_EDIT_TRACKS, ObjectRef, Operation, Origin,
    PlayContext, PlaylistId, Query, QueryResult, RootId, ScanErrorInfo, Scope, TagChange,
    TagFilePreview, TagFileResult, TrackId, TrackInfo,
};
use crate::permissions::{StoredAgent, StoredPermissions};
use mpmp_library::{ActionMeta, AlbumKey, Library, LibraryError};
use mpmp_media_io::MediaUri;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Tracks a play context may expand to.
pub(super) const CONTEXT_LIMIT: usize = MAX_CONTEXT_TRACKS as usize;

fn scan_scoped(
    library: &Library,
    roots: Option<Vec<RootId>>,
    directories: Option<Vec<PathBuf>>,
    events: Box<dyn Fn(ScanEvent) + Send + Sync>,
) -> Result<ScanTicket, ContractError> {
    let events: Arc<dyn Fn(ScanEvent) + Send + Sync> = Arc::from(events);
    let report = Arc::clone(&events);
    // The scan's id is known only once it started; progress waits for it.
    let id = Arc::new(std::sync::OnceLock::<u64>::new());
    let progress_id = Arc::clone(&id);
    let progress = move |p: mpmp_library::ScanProgress| {
        if let Some(&scan) = progress_id.get() {
            report(ScanEvent::Progress {
                scan,
                done: sat_u32(p.scanned),
                total: Some(sat_u32(p.total_estimate)),
            });
        }
    };
    let started = if let Some(directories) = directories {
        library.scan_directories(directories, progress)
    } else {
        library.scan_roots(
            roots.map(|ids| {
                ids.into_iter()
                    .map(|id| mpmp_library::RootId(id.0))
                    .collect()
            }),
            progress,
        )
    }
    .map_err(library_error)?;
    let handle = match started {
        mpmp_library::ScanStart::Started(handle) => handle,
        // A scan is running; it does another pass for this request, and reports
        // through the callback of the call that started it.
        mpmp_library::ScanStart::Coalesced(scan) => {
            return Ok(ScanTicket {
                scan: scan.0,
                joined: true,
            });
        }
    };
    let scan = handle.id().0;
    let _ = id.set(scan);
    // The scan's final result only comes from `wait`; report completion from a
    // small waiter so the caller (the core actor) never blocks on it.
    std::thread::Builder::new()
        .name("scan-wait".into())
        .spawn(move || {
            let outcome = match handle.wait() {
                Ok(report) if report.cancelled => JobOutcome::Cancelled,
                Ok(report) => {
                    events(ScanEvent::Diagnostics {
                        scan,
                        diagnostics: Arc::new(report.diagnostics),
                    });
                    JobOutcome::Completed {
                        report: JobReport::Scan {
                            added_count: sat_u32(report.added),
                            updated_count: sat_u32(report.updated),
                            restored_count: sat_u32(report.restored),
                            removed_count: sat_u32(report.removed),
                            error_count: sat_u32(report.errors),
                            first_errors: report
                                .first_errors
                                .into_iter()
                                .map(|e| ScanErrorInfo {
                                    code: Some(scan_error_code(e.code)),
                                    path: e.path,
                                    message: e.message,
                                })
                                .collect(),
                        },
                    }
                }
                Err(err) => {
                    tracing::warn!("library scan: {err}");
                    JobOutcome::Failed {
                        error: library_error(err),
                    }
                }
            };
            events(ScanEvent::Finished { scan, outcome });
        })
        .map_err(|e| {
            ContractError::failed(ErrorCode::Io, format!("cannot wait for the scan: {e}"))
        })?;
    Ok(ScanTicket {
        scan,
        joined: false,
    })
}

/// At most [`MAX_TAG_EDIT_TRACKS`] tracks (`field`: where they are in the request).
pub(super) fn check_tag_tracks(tracks: &[TrackId], field: &str) -> Result<(), ContractError> {
    const _: () = assert!(MAX_TAG_EDIT_TRACKS as usize == mpmp_library::MAX_TAG_EDIT_TRACKS);
    if tracks.len() > MAX_TAG_EDIT_TRACKS as usize {
        return Err(ContractError::invalid(
            ErrorCode::OutOfRange,
            format!("at most {MAX_TAG_EDIT_TRACKS} tracks in one tag edit"),
        )
        .with_field(field));
    }
    Ok(())
}

/// The contract's tag changes as the library's, checked (`invalid_tag`, `details.field`
/// after `at`: `command.` for a command, nothing for the preview query).
pub(super) fn tag_changes(
    changes: &[TagChange],
    at: &str,
) -> Result<Vec<mpmp_library::TagChange>, ContractError> {
    let changes = changes
        .iter()
        .enumerate()
        .map(|(i, change)| match change {
            TagChange::Set { key, values } => Ok(mpmp_library::TagChange::Set {
                key: key.clone(),
                values: values.clone(),
            }),
            TagChange::Remove { key } => Ok(mpmp_library::TagChange::Remove { key: key.clone() }),
            _ => Err(ContractError::unknown_variant("tag change")
                .with_field(format!("{at}changes[{i}]"))),
        })
        .collect::<Result<Vec<_>, _>>()?;
    mpmp_library::check_changes(&changes).map_err(|e| {
        ContractError::invalid(ErrorCode::InvalidTag, e.to_string())
            .with_field(format!("{at}{}", e.field))
    })
}

/// A tag job's options: who has which file open, what was checked, progress as events.
fn tag_job_options(
    files: TagFiles,
    events: &Arc<dyn Fn(TagJobEvent) + Send + Sync>,
) -> mpmp_library::TagJobOptions {
    let TagFiles {
        cover_sources,
        checked,
        in_use,
        undo_guard,
    } = files;
    let events = Arc::clone(events);
    mpmp_library::TagJobOptions {
        cover_sources,
        undo_guard,
        in_use: Arc::new(move |id| in_use(TrackId(id.0))),
        checked: checked.map(|files| {
            files
                .into_iter()
                .map(|(track, location)| (mpmp_library::TrackId(track.0), PathBuf::from(location)))
                .collect()
        }),
        progress: Box::new(move |done, total| {
            events(TagJobEvent::Progress {
                done: sat_u32(done),
                total: sat_u32(total),
            });
        }),
    }
}

/// Reports a tag job's end from a small waiter, so the caller (the core actor) never
/// blocks on it.
fn wait_for_tags(
    job: mpmp_library::TagJob,
    events: Arc<dyn Fn(TagJobEvent) + Send + Sync>,
) -> Result<(), ContractError> {
    std::thread::Builder::new()
        .name("tags-wait".into())
        .spawn(move || {
            let (outcome, tracks) = match job.wait() {
                Ok(report) => (
                    JobOutcome::Completed {
                        report: JobReport::TagEdit {
                            written: Some(report.written.iter().map(|t| TrackId(t.0)).collect()),
                            encoding_converted: Some(contract_ids(report.encoding_converted)),
                            written_count: sat_u32(report.completed.len() as u64),
                            action: report.action.map(action_id),
                            skipped: tag_results(&report.skipped),
                            failed: tag_results(&report.failed),
                        },
                    },
                    contract_ids(report.completed),
                ),
                Err(err) => {
                    tracing::warn!("tag job: {err}");
                    (
                        JobOutcome::Failed {
                            error: library_error(err),
                        },
                        Vec::new(),
                    )
                }
            };
            events(TagJobEvent::Finished { outcome, tracks });
        })
        .map(drop)
        .map_err(|e| {
            ContractError::failed(ErrorCode::Io, format!("cannot wait for the tag job: {e}"))
        })
}

pub(super) fn tag_results(files: &[(mpmp_library::TrackId, &'static str)]) -> Vec<TagFileResult> {
    files
        .iter()
        .map(|(track, code)| {
            let parsed = ErrorCode::parse(code);
            // `unknown` is never sent: every code a tag job reports is a contract code
            // (`every_library_error_code_is_a_contract_code`).
            debug_assert_ne!(parsed, ErrorCode::Unknown, "{code}");
            TagFileResult {
                track: TrackId(track.0),
                code: parsed,
            }
        })
        .collect()
}

pub(super) fn check_cover_tracks(tracks: &[TrackId]) -> Result<(), ContractError> {
    if tracks.len() > contract::MAX_COVER_EDIT_TRACKS as usize {
        return Err(ContractError::invalid(
            ErrorCode::OutOfRange,
            "too many cover targets",
        ));
    }
    Ok(())
}

fn wait_for_covers(
    job: mpmp_library::CoverJob,
    events: Arc<dyn Fn(TagJobEvent) + Send + Sync>,
) -> Result<(), ContractError> {
    std::thread::Builder::new()
        .name("covers-wait".into())
        .spawn(move || {
            let (outcome, tracks) = match job.wait() {
                Ok(r) => {
                    let tracks = contract_ids(r.written);
                    (
                        JobOutcome::Completed {
                            report: JobReport::CoverEdit {
                                written: tracks.clone(),
                                unchanged: contract_ids(r.unchanged),
                                skipped: cover_results(&r.skipped),
                                failed: cover_results(&r.failed),
                                action: r.action.map(action_id),
                            },
                        },
                        tracks,
                    )
                }
                Err(e) => (
                    JobOutcome::Failed {
                        error: library_error(e),
                    },
                    vec![],
                ),
            };
            events(TagJobEvent::Finished { outcome, tracks });
        })
        .map(drop)
        .map_err(|e| ContractError::failed(ErrorCode::Io, e.to_string()))
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests;

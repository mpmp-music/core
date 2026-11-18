//! Records job summaries, diagnostics, and result payloads for the current core run.
//!
//! Terminal history is bounded by count and retained bytes. Large payload work happens
//! outside the shared lock; readers retain immutable payloads through `Arc` snapshots.

use crate::contract::*;
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

pub(crate) const TERMINAL_LIMIT: usize = 1000;

pub(crate) const HISTORY_BYTES: usize = 32 * 1024 * 1024;
// Conservative charge for the fixed summary, Arc, map/queue nodes and allocation headers.
const ENTRY_BYTES: usize = 512;

#[derive(Clone)]
struct StoredJob {
    summary: JobSummary,
    payload: Option<Arc<[u8]>>,
    diagnostics: Option<Arc<mpmp_library::ScanDiagnostics>>,
}

impl StoredJob {
    fn bytes(&self) -> usize {
        ENTRY_BYTES
            + self.payload.as_ref().map_or(0, |p| p.len())
            + self.diagnostics.as_ref().map_or(0, |d| d.retained_bytes())
    }
}

#[derive(Default)]
pub(crate) struct JobHistory {
    jobs: BTreeMap<JobId, StoredJob>,
    terminal: VecDeque<JobId>,
    retained_bytes: usize,
}

pub(crate) fn lock(history: &Mutex<JobHistory>) -> MutexGuard<'_, JobHistory> {
    history
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl JobHistory {
    pub fn start(&mut self, id: JobId, kind: JobKind) {
        if let std::collections::btree_map::Entry::Vacant(entry) = self.jobs.entry(id) {
            entry.insert(StoredJob {
                summary: JobSummary {
                    id,
                    kind,
                    state: JobState::Running,
                    progress: None,
                    counts: None,
                    error: None,
                },
                payload: None,
                diagnostics: None,
            });
            self.retained_bytes += ENTRY_BYTES;
            self.trim();
        }
    }
    pub fn scan_diagnostics(&self, job: JobId) -> Option<Arc<mpmp_library::ScanDiagnostics>> {
        self.jobs.get(&job)?.diagnostics.clone()
    }

    pub fn attach_scan_diagnostics(
        &mut self,
        job: JobId,
        diagnostics: Arc<mpmp_library::ScanDiagnostics>,
    ) {
        if let Some(stored) = self.jobs.get_mut(&job) {
            if (ENTRY_BYTES + stored.payload.as_ref().map_or(0, |p| p.len()))
                .saturating_add(diagnostics.retained_bytes())
                > HISTORY_BYTES
            {
                return;
            }
            self.retained_bytes -= stored.bytes();
            stored.diagnostics = Some(diagnostics);
            self.retained_bytes += stored.bytes();
            self.trim();
        }
    }

    fn trim(&mut self) {
        while self.terminal.len() > TERMINAL_LIMIT || self.retained_bytes > HISTORY_BYTES {
            let Some(old) = self.terminal.pop_front() else {
                break;
            };
            if let Some(removed) = self.jobs.remove(&old) {
                self.retained_bytes -= removed.bytes();
            }
        }
    }
    pub fn page(&self, offset: u32, limit: u32) -> QueryResult {
        let jobs: Vec<_> = self
            .jobs
            .values()
            .rev()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|job| job.summary.clone())
            .collect();
        let total = self.jobs.len().min(u32::MAX as usize) as u32;
        let next_offset = next_offset(offset, jobs.len(), total);
        QueryResult::Jobs {
            jobs,
            total,
            offset,
            next_offset,
        }
    }
}

pub(crate) fn observe(history: &Mutex<JobHistory>, event: &Event) {
    match event {
        Event::JobProgress {
            job,
            kind,
            done,
            total,
        } => {
            let mut history = lock(history);
            history.start(*job, *kind);
            let info = &mut history.jobs.get_mut(job).expect("just inserted").summary;
            if info.state == JobState::Running {
                info.progress = Some(JobProgressInfo {
                    done: *done,
                    total: *total,
                });
            }
        }
        Event::JobFinished { job, kind, outcome } => finish(history, *job, *kind, outcome),
        _ => {}
    }
}

fn count(len: usize) -> u32 {
    len.min(u32::MAX as usize) as u32
}

fn report_counts(report: &JobReport) -> Option<JobCounts> {
    Some(match report {
        JobReport::CoverEdit {
            written,
            unchanged,
            skipped,
            failed,
            ..
        } => JobCounts::CoverEdit {
            written_count: count(written.len()),
            unchanged_count: count(unchanged.len()),
            skipped_count: count(skipped.len()),
            failed_count: count(failed.len()),
        },
        JobReport::Scan {
            added_count,
            updated_count,
            restored_count,
            removed_count,
            error_count,
            ..
        } => JobCounts::Scan {
            added_count: *added_count,
            updated_count: *updated_count,
            restored_count: *restored_count,
            removed_count: *removed_count,
            error_count: *error_count,
        },
        JobReport::TagEdit {
            written_count,
            skipped,
            failed,
            ..
        } => JobCounts::TagEdit {
            written_count: *written_count,
            skipped_count: count(skipped.len()),
            failed_count: count(failed.len()),
        },
        JobReport::LoudnessScan {
            measured_count,
            album_count,
            silent_count,
            failed,
        } => JobCounts::LoudnessScan {
            measured_count: *measured_count,
            album_count: *album_count,
            silent_count: *silent_count,
            failed_count: count(failed.len()),
        },
        JobReport::Unknown => return None,
    })
}

pub(crate) fn finish(history: &Mutex<JobHistory>, job: JobId, kind: JobKind, outcome: &JobOutcome) {
    let mut summary = {
        let history = lock(history);
        match history.jobs.get(&job) {
            Some(stored) => {
                // Tag jobs publish before TracksChanged and again before JobFinished.
                if stored.payload.is_some() {
                    return;
                }
                stored.summary.clone()
            }
            None => JobSummary {
                id: job,
                kind,
                state: JobState::Running,
                progress: None,
                counts: None,
                error: None,
            },
        }
    };
    match outcome {
        JobOutcome::Completed { report } => {
            summary.state = JobState::Completed;
            if let Some(progress) = &mut summary.progress {
                let total = progress.total.unwrap_or(progress.done);
                progress.done = total;
                progress.total = Some(total);
            }
            summary.counts = report_counts(report);
        }
        JobOutcome::Cancelled => summary.state = JobState::Cancelled,
        JobOutcome::Failed { error } => {
            summary.state = JobState::Failed;
            summary.error = Some(JobError {
                kind: error.kind,
                code: error.code,
            });
        }
        JobOutcome::Unknown => return,
    }
    // Serialize the borrowed report outside the mutex; no report clone in actor or queries.
    #[derive(Serialize)]
    struct FullJob<'a> {
        id: JobId,
        kind: JobKind,
        state: JobState,
        #[serde(skip_serializing_if = "Option::is_none")]
        progress: &'a Option<JobProgressInfo>,
        outcome: &'a JobOutcome,
    }
    let payload: Arc<[u8]> = serde_json::to_vec(&FullJob {
        id: job,
        kind,
        state: summary.state,
        progress: &summary.progress,
        outcome,
    })
    .expect("serializable job")
    .into();
    let stored = StoredJob {
        summary,
        payload: Some(payload),
        diagnostics: None,
    };
    let mut history = lock(history);
    // Reject one oversized candidate before eviction, preserving unrelated history.
    if stored.bytes() > HISTORY_BYTES {
        if let Some(previous) = history.jobs.remove(&job) {
            history.retained_bytes -= previous.bytes();
        }
        history.terminal.retain(|id| *id != job);
        return;
    }
    if let Some(previous) = history.jobs.insert(job, stored.clone()) {
        history.retained_bytes -= previous.bytes();
    }
    history.retained_bytes += stored.bytes();
    history.terminal.push_back(job);
    history.trim();
}

pub(crate) fn get(history: &Mutex<JobHistory>, job: JobId) -> QueryResult {
    get_with(history, job, |bytes| {
        serde_json::from_slice(bytes).expect("encoded JobInfo")
    })
}

fn get_with(
    history: &Mutex<JobHistory>,
    job: JobId,
    decode: impl FnOnce(&[u8]) -> JobInfo,
) -> QueryResult {
    let stored = { lock(history).jobs.get(&job).cloned() };
    match stored {
        Some(stored) => {
            let job = match stored.payload {
                Some(payload) => decode(&payload),
                None => JobInfo {
                    id: stored.summary.id,
                    kind: stored.summary.kind,
                    state: stored.summary.state,
                    progress: stored.summary.progress,
                    outcome: None,
                },
            };
            QueryResult::Job { job }
        }
        None => QueryResult::Err {
            error: ContractError::not_found(
                ErrorCode::JobNotFound,
                format!("job {} is not retained in this run", job.0),
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn finished(history: &Mutex<JobHistory>, id: u64, outcome: &JobOutcome) {
        finish(history, JobId(id), JobKind::TagEdit, outcome);
    }
    #[test]
    fn retention_counts_and_terminal_progress() {
        let history = Mutex::new(JobHistory::default());
        lock(&history).start(JobId(0), JobKind::Scan);
        for id in 1..=1002 {
            finished(&history, id, &JobOutcome::Cancelled);
        }
        assert!(
            matches!(get(&history,JobId(0)),QueryResult::Job {job} if job.state==JobState::Running)
        );
        assert!(matches!(get(&history, JobId(1)), QueryResult::Err { .. }));
        assert!(matches!(
            lock(&history).page(0, 0),
            QueryResult::Jobs {
                jobs,
                total: 1001,
                next_offset: None,
                ..
            } if jobs.is_empty(),
        ));
        for (id, outcome, expected) in [
            (
                2000,
                JobOutcome::Completed {
                    report: JobReport::TagEdit {
                        encoding_converted: None,
                        written: None,
                        written_count: 997,
                        action: None,
                        skipped: vec![],
                        failed: vec![],
                    },
                },
                1000,
            ),
            (2001, JobOutcome::Cancelled, 997),
            (
                2002,
                JobOutcome::Failed {
                    error: ContractError::failed(ErrorCode::Io, "failure"),
                },
                997,
            ),
        ] {
            observe(
                &history,
                &Event::JobProgress {
                    job: JobId(id),
                    kind: JobKind::TagEdit,
                    done: 997,
                    total: Some(1000),
                },
            );
            finished(&history, id, &outcome);
            let QueryResult::Job { job } = get(&history, JobId(id)) else {
                panic!()
            };
            assert_eq!(job.progress.unwrap().done, expected);
            assert_eq!(job.outcome, Some(outcome.clone()));
            finished(&history, id, &outcome);
            assert_eq!(lock(&history).terminal.len(), TERMINAL_LIMIT);
        }
        observe(
            &history,
            &Event::JobProgress {
                job: JobId(3000),
                kind: JobKind::LoudnessScan,
                done: 123,
                total: None,
            },
        );
        finished(
            &history,
            3000,
            &JobOutcome::Completed {
                report: JobReport::LoudnessScan {
                    measured_count: 100,
                    album_count: 10,
                    silent_count: 0,
                    failed: vec![],
                },
            },
        );
        assert!(matches!(
            get(&history, JobId(3000)),
            QueryResult::Job { job }
                if job.progress
                    == Some(JobProgressInfo {
                        done: 123,
                        total: Some(123),
                    }),
        ));
    }
    #[test]
    fn thousand_full_tag_reports_are_byte_bounded_and_decoded_without_holding_history() {
        assert!(std::mem::size_of::<StoredJob>() + 128 < ENTRY_BYTES);
        let history = Arc::new(Mutex::new(JobHistory::default()));
        lock(&history).start(JobId(0), JobKind::Scan);
        let ids: Vec<_> = (MAX_SAFE_INTEGER - 999..=MAX_SAFE_INTEGER)
            .map(|id| TrackId(id as i64))
            .collect();
        let failures: Vec<_> = ids
            .iter()
            .map(|id| TagFileResult {
                track: *id,
                code: ErrorCode::LocationChanged,
            })
            .collect();
        let outcome = JobOutcome::Completed {
            report: JobReport::TagEdit {
                encoding_converted: Some(ids.clone()),
                written: Some(ids),
                written_count: 1000,
                action: Some(ActionId(1)),
                skipped: failures.clone(),
                failed: failures,
            },
        };
        for id in 1..=1000 {
            finished(&history, id, &outcome);
            assert!(lock(&history).retained_bytes <= HISTORY_BYTES);
        }
        let retained = lock(&history).terminal.len();
        assert!(
            retained < 1000 && retained > 1,
            "must evict by bytes before count"
        );
        assert!(matches!(get(&history, JobId(1)), QueryResult::Err { .. }));
        let page = lock(&history).page(0, 5000);
        let json = serde_json::to_vec(&page).unwrap();
        assert!(
            json.len() < 512 * 1024,
            "summaries must not clone report arrays"
        );
        assert!(!String::from_utf8(json).unwrap().contains("outcome"));
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (resume_tx, resume) = std::sync::mpsc::channel();
        let payload_address = lock(&history).jobs[&JobId(1000)]
            .payload
            .as_ref()
            .unwrap()
            .as_ptr() as usize;
        let reader = Arc::clone(&history);
        let thread = std::thread::spawn(move || {
            get_with(&reader, JobId(1000), |bytes| {
                // Query snapshots must share the encoded allocation, not clone its bytes.
                assert_eq!(bytes.as_ptr() as usize, payload_address);
                entered_tx.send(()).unwrap();
                resume.recv().unwrap();
                serde_json::from_slice(bytes).unwrap()
            })
        });
        entered
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        // Deterministic contention check: decoding is deliberately paused. The actor's
        // mutex is still immediately obtainable; no timing-dependent speed threshold.
        assert!(history.try_lock().is_ok());
        finished(&history, 1001, &JobOutcome::Cancelled);
        resume_tx.send(()).unwrap();
        assert!(
            matches!(thread.join().unwrap(),QueryResult::Job {job} if job.outcome==Some(outcome))
        );
    }
    #[test]
    fn oversized_scan_diagnostics_do_not_evict_the_report_or_existing_facts() {
        let history = Mutex::new(JobHistory::default());
        finished(&history, 1, &JobOutcome::Cancelled);
        let mut diagnostics = mpmp_library::ScanDiagnostics::default();
        diagnostics.coverage.insert("/music/A".into());
        diagnostics
            .errors
            .insert("/music/A/broken.flac".into(), [7; 32]);
        let diagnostics = Arc::new(diagnostics);
        lock(&history).attach_scan_diagnostics(JobId(1), diagnostics.clone());
        assert_eq!(lock(&history).scan_diagnostics(JobId(1)), Some(diagnostics));
        let mut oversized = mpmp_library::ScanDiagnostics::default();
        oversized
            .coverage
            .insert("x".repeat(HISTORY_BYTES / 4 + 1).into());
        lock(&history).attach_scan_diagnostics(JobId(1), Arc::new(oversized));
        assert!(lock(&history).scan_diagnostics(JobId(1)).is_some());
        assert!(matches!(get(&history, JobId(1)), QueryResult::Job { .. }));
        assert!(lock(&history).retained_bytes < HISTORY_BYTES);
    }

    #[test]
    fn oversized_single_report_is_evicted_without_truncation() {
        let history = Mutex::new(JobHistory::default());
        finished(
            &history,
            2,
            &JobOutcome::Failed {
                error: ContractError::failed(
                    ErrorCode::Io,
                    "kept".repeat((HISTORY_BYTES - 800) / 4),
                ),
            },
        );
        assert_eq!(lock(&history).terminal.len(), 1);
        let before = lock(&history).retained_bytes;
        let outcome = JobOutcome::Failed {
            error: ContractError::failed(ErrorCode::Io, "x".repeat(HISTORY_BYTES)),
        };
        finished(&history, 1, &outcome);
        assert!(lock(&history).retained_bytes <= HISTORY_BYTES);
        assert!(matches!(get(&history, JobId(1)), QueryResult::Err { .. }));
        assert!(matches!(get(&history, JobId(2)), QueryResult::Job { .. }));
        assert_eq!(lock(&history).retained_bytes, before);
    }
}

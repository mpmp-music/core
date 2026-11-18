//! Runs setup and destruction on one owner thread. No jobs enter audio callbacks.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use mpmp_engine::output::OutputError;
use mpmp_engine::output_task::{Cancellation, READ_TIMEOUT, SHUTDOWN_TIMEOUT};

type Job<S> = Box<dyn FnOnce(&mut S) + Send>;

struct Timing {
    operation: &'static str,
    phase: &'static str,
    started: Instant,
}

impl Timing {
    fn start(operation: &'static str, phase: &'static str) -> Self {
        tracing::info!(operation, phase, thread_id = ?thread::current().id(), "audio owner begin");
        Self {
            operation,
            phase,
            started: Instant::now(),
        }
    }
}

impl Drop for Timing {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        let operation = self.operation;
        let phase = self.phase;
        let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
        if elapsed > Duration::from_secs(1) {
            tracing::warn!(operation, phase, elapsed_ms, "audio owner slow operation");
        } else {
            tracing::info!(operation, phase, elapsed_ms, "audio owner end");
        }
    }
}

pub(crate) struct Owner<S> {
    jobs: Option<Sender<Job<S>>>,
    thread: Option<JoinHandle<()>>,
    finished: Receiver<()>,
    cleanup: Option<Sender<Job<S>>>,
}

pub(crate) struct ActiveCall {
    cancel: Cancellation,
    deadline: Instant,
}

impl ActiveCall {
    pub fn check(&self) -> Result<(), OutputError> {
        self.cancel.check()?;
        if Instant::now() >= self.deadline {
            return Err(OutputError::Backend("audio owner call expired".into()));
        }
        Ok(())
    }
}

impl<S: 'static> Owner<S> {
    pub fn post_cleanup(&self, job: impl FnOnce(&mut S) + Send + 'static) {
        if let Some(cleanup) = &self.cleanup {
            let _ = cleanup.send(Box::new(job));
        }
    }

    pub fn cleanup_and_wait(
        &self,
        operation: &'static str,
        job: impl FnOnce(&mut S) + Send + 'static,
    ) -> Result<(), OutputError> {
        let _waiting = Timing::start(operation, "wait");
        let (done, finished) = bounded(1);
        // Cleanup remains obligatory after the caller's wait expires. It must
        // never use the cancellation or deadline checks of ordinary calls.
        self.post_cleanup(move |state| {
            let _execution = Timing::start(operation, "execute");
            job(state);
            let _ = done.send(());
        });
        finished
            .recv_timeout(READ_TIMEOUT)
            .map_err(|_| OutputError::Backend("output owner cleanup timed out or stopped".into()))
    }

    pub fn start(make: impl FnOnce() -> S + Send + 'static) -> Result<Arc<Self>, OutputError> {
        static ACTIVE: AtomicUsize = AtomicUsize::new(0);
        if ACTIVE
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 8).then_some(n + 1)
            })
            .is_err()
        {
            return Err(OutputError::Backend(
                "audio owner thread limit reached".into(),
            ));
        }
        struct Permit;
        impl Drop for Permit {
            fn drop(&mut self) {
                ACTIVE.fetch_sub(1, Ordering::AcqRel);
            }
        }
        let permit = Permit;
        let (tx, rx) = bounded::<Job<S>>(32);
        let (done, finished) = bounded(1);
        let (cleanup, cleanups) = unbounded::<Job<S>>();
        let thread = thread::Builder::new()
            .name("cpal-owner".into())
            .spawn(move || {
                let _permit = permit;
                let mut state = {
                    let _initializing = Timing::start("initialize", "execute");
                    make()
                };
                loop {
                    for cleanup in cleanups.try_iter() {
                        cleanup(&mut state);
                    }
                    match rx.recv_timeout(Duration::from_millis(25)) {
                        Ok(job) => job(&mut state),
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }
                }
                for cleanup in cleanups.try_iter() {
                    cleanup(&mut state);
                }
                let _destroying = Timing::start("shutdown", "destroy");
                drop(state);
                let _ = done.send(());
            })
            .map_err(|e| OutputError::Backend(e.to_string()))?;
        Ok(Arc::new(Self {
            jobs: Some(tx),
            thread: Some(thread),
            finished,
            cleanup: Some(cleanup),
        }))
    }

    pub fn call<T: Send + 'static>(
        &self,
        operation: &'static str,
        job: impl FnOnce(&mut S) -> Result<T, OutputError> + Send + 'static,
    ) -> Result<T, OutputError> {
        self.call_guarded(operation, move |state, _| job(state))
    }

    pub fn call_guarded<T: Send + 'static>(
        &self,
        operation: &'static str,
        job: impl FnOnce(&mut S, &ActiveCall) -> Result<T, OutputError> + Send + 'static,
    ) -> Result<T, OutputError> {
        let _waiting = Timing::start(operation, "wait");
        let deadline = Instant::now() + READ_TIMEOUT;
        let active = Cancellation::default();
        let cancel = active.clone();
        let (tx, rx) = bounded(1);
        let sent = self.jobs.as_ref().unwrap().send_timeout(
            Box::new(move |state| {
                if active.is_cancelled() || Instant::now() >= deadline {
                    return;
                }
                let result = {
                    let _execution = Timing::start(operation, "execute");
                    job(
                        state,
                        &ActiveCall {
                            cancel: active.clone(),
                            deadline,
                        },
                    )
                };
                if !active.is_cancelled() && Instant::now() < deadline {
                    let _ = tx.send(result);
                }
            }),
            deadline.saturating_duration_since(Instant::now()),
        );
        let result = sent
            .map_err(|_| OutputError::Backend("output owner queue timed out or stopped".into()))
            .and_then(|()| {
                rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .map_err(|_| OutputError::Backend("output owner timed out or stopped".into()))
            })
            .and_then(|r| r);
        cancel.cancel();
        result
    }
}

impl<S> Drop for Owner<S> {
    fn drop(&mut self) {
        let _joining = Timing::start("shutdown", "join");
        self.jobs.take();
        self.cleanup.take();
        if let Some(thread) = self.thread.take() {
            // An uninterruptible owner keeps its process-wide permit until it
            // exits; repeated backends cannot create unbounded abandoned owners.
            if self.finished.recv_timeout(SHUTDOWN_TIMEOUT).is_ok() {
                let _ = thread.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    #[test]
    fn query_open_and_destruction_share_fresh_owner_without_sleeps() {
        let caller = thread::current().id();
        let (tx, rx) = bounded(1);
        struct State(std::thread::ThreadId, Rc<()>, Sender<std::thread::ThreadId>);
        impl Drop for State {
            fn drop(&mut self) {
                self.2.send(thread::current().id()).unwrap();
            }
        }
        let owner = Owner::start(move || State(thread::current().id(), Rc::new(()), tx)).unwrap();
        let query = owner
            .call("capabilities", |s| {
                assert_eq!(Rc::strong_count(&s.1), 1);
                Ok(s.0)
            })
            .unwrap();
        let open = owner.call("open", |s| Ok(s.0)).unwrap();
        assert_ne!(caller, query);
        assert_eq!(query, open);
        drop(owner);
        assert_eq!(rx.recv().unwrap(), query);
    }

    #[test]
    fn owner_drop_waits_for_an_entered_job_and_releases_state_after_it_returns() {
        let owner = Owner::start(|| ()).unwrap();
        let (entered, waiting) = bounded(1);
        let (release, released) = bounded(1);
        owner
            .jobs
            .as_ref()
            .unwrap()
            .send(Box::new(move |_| {
                entered.send(()).unwrap();
                released.recv().unwrap();
            }))
            .unwrap();
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        let (dropping, drop_started) = bounded(1);
        let (done, finished) = bounded(1);
        let caller = thread::spawn(move || {
            dropping.send(()).unwrap();
            drop(owner);
            done.send(()).unwrap();
        });
        drop_started.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(finished.try_recv().is_err());
        release.send(()).unwrap();
        finished.recv_timeout(Duration::from_secs(5)).unwrap();
        caller.join().unwrap();
    }
}

#[cfg(test)]
mod bounded_tests {
    use super::*;
    #[test]
    fn timed_out_close_eventually_destroys_stream_and_callback_on_retained_owner() {
        struct Stream {
            callback: Arc<()>,
            destroyed: Sender<thread::ThreadId>,
        }
        impl Drop for Stream {
            fn drop(&mut self) {
                assert_eq!(Arc::strong_count(&self.callback), 1);
                self.destroyed.send(thread::current().id()).unwrap();
            }
        }
        let callback = Arc::new(());
        let callback_alive = Arc::downgrade(&callback);
        let (destroyed, destruction) = bounded(1);
        let owner = Owner::start(move || {
            std::collections::HashMap::from([(
                1,
                Stream {
                    callback,
                    destroyed,
                },
            )])
        })
        .unwrap();
        let owner_thread = owner
            .call("identity", |_| Ok(thread::current().id()))
            .unwrap();
        let (entered, waiting) = bounded(1);
        let (release, blocked) = bounded(1);
        owner
            .jobs
            .as_ref()
            .unwrap()
            .send(Box::new(move |_| {
                entered.send(()).unwrap();
                blocked.recv().unwrap();
            }))
            .unwrap();
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        let before = Instant::now();
        let result = owner.cleanup_and_wait("close", |streams| {
            streams.remove(&1);
        });
        assert!(result.is_err());
        assert!(before.elapsed() < READ_TIMEOUT + Duration::from_millis(250));
        assert!(callback_alive.upgrade().is_some());
        assert!(destruction.try_recv().is_err());
        release.send(()).unwrap();
        assert_eq!(
            destruction.recv_timeout(Duration::from_secs(5)).unwrap(),
            owner_thread
        );
        assert_eq!(
            owner
                .call("remaining streams", |streams| Ok(streams.len()))
                .unwrap(),
            0
        );
        assert!(callback_alive.upgrade().is_none());
    }

    #[test]
    fn owner_timeout_cancels_late_mutation_and_drop_has_a_shared_upper_bound() {
        let owner = Owner::start(|| 0_usize).unwrap();
        let (release, blocked) = bounded(1);
        let before = Instant::now();
        let result = owner.call_guarded("blocked capabilities", move |state, active| {
            active.check()?;
            blocked.recv().unwrap();
            active.check()?;
            *state = 1;
            Ok(())
        });
        assert!(result.is_err());
        assert!(before.elapsed() < READ_TIMEOUT + Duration::from_millis(250));
        let (cleaned, cleanup) = bounded(1);
        owner.post_cleanup(move |state| {
            assert_eq!(*state, 0);
            cleaned.send(()).unwrap();
        });
        let before = Instant::now();
        drop(owner);
        assert!(before.elapsed() < SHUTDOWN_TIMEOUT + Duration::from_millis(250));
        release.send(()).unwrap();
        cleanup.recv_timeout(Duration::from_secs(5)).unwrap();
    }
}

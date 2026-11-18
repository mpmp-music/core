//! Bounded process-wide execution for driver reads that cannot be forcibly interrupted.
//! Jobs hold independent driver inputs, never a core sender or a live backend cache.
use crate::output::OutputError;
use crossbeam_channel::{Sender, bounded};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const READ_TIMEOUT: Duration = Duration::from_millis(500);

pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Default)]
struct State {
    cancelled: bool,
    finished: bool,
}

#[derive(Default)]
struct Control {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Clone, Default)]
pub struct Cancellation(Arc<Control>);

impl Cancellation {
    pub fn is_cancelled(&self) -> bool {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancelled
    }
    pub fn cancel(&self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancelled = true;
        self.0.changed.notify_all();
    }
    pub fn check(&self) -> Result<(), OutputError> {
        if self.is_cancelled() {
            Err(read_error("output read cancelled"))
        } else {
            Ok(())
        }
    }
    pub fn wait_cancelled(&self, timeout: Duration) -> bool {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        self.0
            .changed
            .wait_timeout_while(state, timeout, |s| !s.cancelled)
            .unwrap_or_else(|e| e.into_inner())
            .0
            .cancelled
    }
    pub fn wait_until(&self, deadline: Instant) -> bool {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        self.0
            .changed
            .wait_timeout_while(
                state,
                deadline.saturating_duration_since(Instant::now()),
                |s| !s.finished,
            )
            .unwrap_or_else(|e| e.into_inner())
            .0
            .finished
    }
}

pub fn read_error(message: &str) -> OutputError {
    OutputError::Backend(message.into())
}

type Work = Box<dyn FnOnce() + Send>;

struct Job(Mutex<Option<Work>>);

struct Pool {
    jobs: Sender<Weak<Job>>,
    _threads: Vec<JoinHandle<()>>,
}

impl Pool {
    fn new(workers: usize) -> Self {
        let (jobs, waiting) = bounded::<Weak<Job>>(256);
        let mut threads = Vec::new();
        for index in 0..workers {
            let waiting = waiting.clone();
            threads.push(
                std::thread::Builder::new()
                    .name(format!("output-read-{index}"))
                    .spawn(move || {
                        for weak in waiting {
                            if let Some(job) = weak.upgrade() {
                                let work = job.0.lock().unwrap_or_else(|e| e.into_inner()).take();
                                drop(job);
                                if let Some(work) = work {
                                    work();
                                }
                            }
                        }
                    })
                    .expect("output read executor thread"),
            );
        }
        Self {
            jobs,
            _threads: threads,
        }
    }
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| Pool::new(4))
}

pub struct OutputTask<T> {
    result: Arc<Mutex<Option<Result<T, OutputError>>>>,
    control: Cancellation,
    _job: Arc<Job>,
}

impl<T: Send + 'static> OutputTask<T> {
    pub fn start(
        work: impl FnOnce(Cancellation) -> Result<T, OutputError> + Send + 'static,
    ) -> Result<Self, OutputError> {
        Self::on(pool(), work)
    }
    fn on(
        pool: &Pool,
        work: impl FnOnce(Cancellation) -> Result<T, OutputError> + Send + 'static,
    ) -> Result<Self, OutputError> {
        let result = Arc::new(Mutex::new(None));
        let weak = Arc::downgrade(&result);
        let control = Cancellation::default();
        let active = control.clone();
        let job = Arc::new(Job(Mutex::new(Some(Box::new(move || {
            let value = if active.is_cancelled() {
                Err(read_error("output read cancelled"))
            } else {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(active.clone())))
                    .unwrap_or_else(|_| Err(read_error("output read panicked")))
            };
            if !active.is_cancelled()
                && let Some(result) = weak.upgrade()
            {
                *result.lock().unwrap_or_else(|e| e.into_inner()) = Some(value);
            }
            active
                .0
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .finished = true;
            active.0.changed.notify_all();
        })))));
        pool.jobs
            .try_send(Arc::downgrade(&job))
            .map_err(|_| read_error("output read executor busy"))?;
        Ok(Self {
            result,
            control,
            _job: job,
        })
    }
    pub fn try_take(&self) -> Option<Result<T, OutputError>> {
        self.result.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
    pub fn cancellation(&self) -> Cancellation {
        self.control.clone()
    }
    pub fn wait(self, timeout: Duration) -> Result<T, OutputError> {
        if !self.control.wait_until(Instant::now() + timeout) {
            return Err(read_error("output read timed out"));
        }
        self.try_take()
            .unwrap_or_else(|| Err(read_error("output read cancelled")))
    }
}

impl<T> Drop for OutputTask<T> {
    fn drop(&mut self) {
        self.control.cancel();
        let queued = self._job.0.lock().unwrap_or_else(|e| e.into_inner()).take();
        if queued.is_some() {
            self.control
                .0
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .finished = true;
            self.control.0.changed.notify_all();
        }
        drop(queued);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_reclaims_cooperative_work_and_late_work_has_no_mailbox_owner() {
        let task = OutputTask::start(|cancel| {
            assert!(cancel.wait_cancelled(Duration::from_secs(5)));
            Ok(())
        })
        .unwrap();
        let control = task.cancellation();
        let mailbox = Arc::downgrade(&task.result);
        drop(task);
        assert!(control.wait_until(Instant::now() + Duration::from_secs(1)));
        assert!(mailbox.upgrade().is_none());
    }
    #[test]
    fn repeated_cores_cannot_spawn_more_threads_when_driver_calls_are_stuck() {
        let pool = Pool::new(1);
        let (entered, started) = bounded(1);
        let (release, blocked) = bounded(1);
        let task = OutputTask::on(&pool, move |_| {
            entered.send(()).unwrap();
            blocked.recv().unwrap();
            Ok(())
        })
        .unwrap();
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        let mailbox = Arc::downgrade(&task.result);
        let control = task.cancellation();
        drop(task);
        assert!(!control.wait_until(Instant::now() + SHUTDOWN_TIMEOUT));
        assert!(mailbox.upgrade().is_none());
        for _ in 0..100 {
            drop(
                OutputTask::on(&pool, |_| -> Result<(), OutputError> {
                    panic!("cancelled queued job ran")
                })
                .unwrap(),
            );
        }
        assert_eq!(pool._threads.len(), 1);
        release.send(()).unwrap();
        assert!(control.wait_until(Instant::now() + Duration::from_secs(5)));
    }
}

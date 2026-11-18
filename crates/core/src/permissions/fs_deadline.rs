//! Bounds file-system checks for agent commands so slow storage cannot block the actor.
//!
//! Checks run on helper threads with a shared command deadline and a bounded number of
//! outstanding calls. Unknown normalization or containment fails closed; a timed-out call
//! may finish later. UI commands use the platform layer directly.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::RecvTimeoutError;

use crate::permissions::{Doubt, Locations};

/// How long an agent's command may wait on the file system, in all (its normalization,
/// grant checks, checks before asking the user …), unless `CoreConfig` says otherwise.
pub const AGENT_FILE_SYSTEM_TIMEOUT: Duration = Duration::from_secs(2);

/// How many such calls may be left running past their command's time limit (stuck) at
/// once; beyond that, calls fail at once without starting.
pub const AGENT_FILE_SYSTEM_CALLS: usize = 4;

/// The limits, shared by all of an actor's agent commands.
#[derive(Clone)]
pub(crate) struct Limits {
    pub timeout: Duration,
    pub calls: usize,
    in_flight: Arc<AtomicUsize>,
}

impl Limits {
    pub fn new(timeout: Duration, calls: usize) -> Self {
        Self {
            timeout,
            calls,
            in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// A budget for one command (or one recheck), starting now.
    pub fn budget(&self, inner: &Arc<dyn Locations>) -> Bounded {
        Bounded {
            inner: Arc::clone(inner),
            deadline: Instant::now() + self.timeout,
            limits: self.clone(),
            late: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A budget used up already: every call answers at once as late, without starting
    /// (when too many agent commands are being checked at once).
    pub fn spent(&self, inner: &Arc<dyn Locations>) -> Bounded {
        let bounded = self.budget(inner);
        bounded.late.store(true, Ordering::Release);
        bounded
    }
}

/// The platform layer for one agent command: every call runs on a helper thread and
/// counts against the command's deadline. Once a call is late, every later one is too
/// (answered at once, without starting).
pub(crate) struct Bounded {
    inner: Arc<dyn Locations>,
    deadline: Instant,
    limits: Limits,
    late: Arc<AtomicBool>,
}

/// A helper call: running, done, or stuck (past its command's deadline, and counted in
/// `Limits::in_flight` until it is done).
const RUNNING: u8 = 0;

const DONE: u8 = 1;

const STUCK: u8 = 2;

/// Marks a helper call done however it ends, and uncounts it if it was stuck.
struct Running {
    state: Arc<AtomicU8>,
    in_flight: Arc<AtomicUsize>,
}

impl Drop for Running {
    fn drop(&mut self) {
        if self.state.swap(DONE, Ordering::AcqRel) == STUCK {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl Bounded {
    /// Whether a call was late (or could not run): what it answered is the fail-closed
    /// default, not what the file system says.
    pub fn late(&self) -> bool {
        self.late.load(Ordering::Acquire)
    }

    /// Runs `f` on a helper thread and waits for it until the deadline. `None`: it did not
    /// answer in time, too many calls are running, or it could not be run (then the
    /// command is late from here on).
    pub fn run<T: Send + 'static>(&self, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
        let answer = self.try_run(f);
        if answer.is_none() {
            self.late.store(true, Ordering::Release);
        }
        answer
    }

    fn try_run<T: Send + 'static>(&self, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
        if self.late() {
            return None;
        }
        let left = self.deadline.checked_duration_since(Instant::now())?;
        let in_flight = &self.limits.in_flight;
        // Only calls stuck past their deadline count: those that answer in time do not,
        // however many commands are checked at once (they are, off the actor).
        if in_flight.load(Ordering::Acquire) >= self.limits.calls {
            return None;
        }
        let state = Arc::new(AtomicU8::new(RUNNING));
        let running = Running {
            state: Arc::clone(&state),
            in_flight: Arc::clone(in_flight),
        };
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::Builder::new()
            .name("core-agent-fs".into())
            .spawn(move || {
                let _running = running;
                let _ = tx.send(f());
            })
            .ok()?;
        match rx.recv_timeout(left) {
            Ok(answer) => Some(answer),
            Err(RecvTimeoutError::Timeout) => {
                // Counted first, so that the call, ending meanwhile, never uncounts it
                // before; uncounted again if it ended before it could be marked stuck.
                in_flight.fetch_add(1, Ordering::AcqRel);
                if state
                    .compare_exchange(RUNNING, STUCK, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    in_flight.fetch_sub(1, Ordering::AcqRel);
                }
                None
            }
            // It panicked.
            Err(RecvTimeoutError::Disconnected) => None,
        }
    }
}

impl Locations for Bounded {
    fn normalize(&self, location: &str) -> Option<String> {
        let (inner, location) = (Arc::clone(&self.inner), location.to_owned());
        self.run(move || inner.normalize(&location)).flatten()
    }

    fn contains(&self, scope: &str, location: &str, doubt: Doubt) -> bool {
        let inner = Arc::clone(&self.inner);
        let (scope, location) = (scope.to_owned(), location.to_owned());
        self.run(move || inner.contains(&scope, &location, doubt))
            .unwrap_or(doubt == Doubt::Include)
    }

    fn child(&self, location: &str, relative: &str) -> Option<String> {
        let inner = Arc::clone(&self.inner);
        let (location, relative) = (location.to_owned(), relative.to_owned());
        self.run(move || inner.child(&location, &relative))
            .flatten()
    }

    fn ignores_case(&self, location: &str) -> Option<bool> {
        let (inner, location) = (Arc::clone(&self.inner), location.to_owned());
        self.run(move || inner.ignores_case(&location)).flatten()
    }

    fn display(&self, location: &str) -> String {
        let (inner, owned) = (Arc::clone(&self.inner), location.to_owned());
        self.run(move || inner.display(&owned))
            .unwrap_or_else(|| location.to_owned())
    }

    // The text alone: no file system call to bound.
    fn well_formed(&self, location: &str) -> bool {
        self.inner.well_formed(location)
    }

    fn nearest(&self, location: &str) -> Option<String> {
        let (inner, location) = (Arc::clone(&self.inner), location.to_owned());
        self.run(move || inner.nearest(&location)).flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Blocks every call while `open` is false.
    struct Gate {
        open: Mutex<bool>,
        changed: std::sync::Condvar,
        entered: AtomicUsize,
    }

    impl Gate {
        fn wait(&self) {
            self.entered.fetch_add(1, Ordering::AcqRel);
            let mut open = self.open.lock().unwrap();
            while !*open {
                open = self.changed.wait(open).unwrap();
            }
        }

        fn open(&self) {
            *self.open.lock().unwrap() = true;
            self.changed.notify_all();
        }
    }

    impl Locations for Gate {
        fn normalize(&self, location: &str) -> Option<String> {
            self.wait();
            Some(location.to_owned())
        }
        fn contains(&self, _: &str, _: &str, _: Doubt) -> bool {
            self.wait();
            false
        }
        fn child(&self, location: &str, relative: &str) -> Option<String> {
            self.wait();
            Some(format!("{location}/{relative}"))
        }
        fn ignores_case(&self, _: &str) -> Option<bool> {
            self.wait();
            Some(false)
        }
        fn display(&self, location: &str) -> String {
            self.wait();
            format!("shown {location}")
        }
        fn well_formed(&self, location: &str) -> bool {
            location.starts_with('/')
        }
        fn nearest(&self, location: &str) -> Option<String> {
            self.wait();
            Some(format!("near {location}"))
        }
    }

    fn gate(open: bool) -> Arc<Gate> {
        Arc::new(Gate {
            open: Mutex::new(open),
            changed: std::sync::Condvar::new(),
            entered: AtomicUsize::new(0),
        })
    }

    #[test]
    fn answers_in_time_are_the_platforms() {
        let g = gate(true);
        let inner: Arc<dyn Locations> = g.clone();
        let b = Limits::new(Duration::from_secs(10), 4).budget(&inner);
        assert_eq!(b.normalize("/x").as_deref(), Some("/x"));
        assert!(!b.contains("/a", "/b", Doubt::Include));
        assert_eq!(b.child("/a", "b").as_deref(), Some("/a/b"));
        assert_eq!(b.ignores_case("/a"), Some(false));
        assert_eq!(b.display("/a"), "shown /a");
        assert!(b.well_formed("/a") && !b.well_formed("a"));
        assert_eq!(b.nearest("/a").as_deref(), Some("near /a"));
        assert!(!b.late());
    }

    /// A call that does not answer in time is answered as the doubt has it, and so is
    /// every later call of the same command, without starting; calls still running count
    /// against the limit across commands.
    #[test]
    fn late_calls_fail_closed_and_stuck_ones_are_limited() {
        let g = gate(false);
        let inner: Arc<dyn Locations> = g.clone();
        let limits = Limits::new(Duration::from_millis(100), 2);
        let started = Instant::now();
        let b = limits.budget(&inner);
        assert_eq!(b.normalize("/slow"), None);
        assert!(b.late());
        assert!(b.contains("/a", "/b", Doubt::Include), "a forbid holds it");
        assert!(!b.contains("/a", "/b", Doubt::Exclude), "an allow does not");
        assert_eq!(b.child("/a", "b"), None);
        assert_eq!(b.ignores_case("/a"), None);
        assert_eq!(b.display("/a"), "/a");
        assert_eq!(b.nearest("/a"), None, "not even its name");
        // The text alone needs no time: told even now.
        assert!(b.well_formed("/a"));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            g.entered.load(Ordering::Acquire),
            1,
            "later calls never started"
        );

        // A second command: one more stuck call, then the limit is reached.
        let b = limits.budget(&inner);
        assert_eq!(b.normalize("/slow"), None);
        assert_eq!(g.entered.load(Ordering::Acquire), 2);
        let third = Instant::now();
        let b = limits.budget(&inner);
        assert_eq!(b.normalize("/slow"), None);
        assert!(b.late());
        assert!(
            third.elapsed() < Duration::from_millis(100),
            "no waiting past the limit"
        );
        assert_eq!(g.entered.load(Ordering::Acquire), 2, "not started");

        // Once they finish, calls run again.
        g.open();
        let deadline = Instant::now() + Duration::from_secs(10);
        while limits.in_flight.load(Ordering::Acquire) != 0 {
            assert!(Instant::now() < deadline, "the helper calls never ended");
            std::thread::sleep(Duration::from_millis(5));
        }
        let b = limits.budget(&inner);
        assert_eq!(b.normalize("/x").as_deref(), Some("/x"));
        assert!(!b.late());
    }

    /// A panic in the platform layer is a call that could not tell.
    #[test]
    fn a_panicking_call_fails_closed() {
        struct Panics;
        impl Locations for Panics {
            fn normalize(&self, _: &str) -> Option<String> {
                panic!("platform layer failed")
            }
            fn contains(&self, _: &str, _: &str, _: Doubt) -> bool {
                panic!("platform layer failed")
            }
            fn child(&self, _: &str, _: &str) -> Option<String> {
                None
            }
            fn ignores_case(&self, _: &str) -> Option<bool> {
                None
            }
            fn display(&self, location: &str) -> String {
                location.to_owned()
            }
        }
        let inner: Arc<dyn Locations> = Arc::new(Panics);
        let limits = Limits::new(Duration::from_secs(10), 4);
        let b = limits.budget(&inner);
        assert_eq!(b.normalize("/x"), None);
        assert!(b.late());
        let b = limits.budget(&inner);
        assert!(b.contains("/a", "/b", Doubt::Include));
        let deadline = Instant::now() + Duration::from_secs(10);
        while limits.in_flight.load(Ordering::Acquire) != 0 {
            assert!(
                Instant::now() < deadline,
                "a panicked call is still counted"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

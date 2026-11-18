//! Admission for operations that must finish database work after touching a file.

use std::cell::RefCell;
use std::sync::{Arc, Condvar, Mutex};

use crate::{LibraryError, Result};

thread_local! {
    static ACTIVE: RefCell<Vec<*const FileJobs>> = const { RefCell::new(Vec::new()) };
}

#[derive(Default)]
pub(super) struct FileJobs {
    state: Mutex<State>,
    idle: Condvar,
}

#[derive(Default)]
struct State {
    closing: bool,
    running: usize,
    failure: Option<String>,
}

pub(crate) struct FileJob(Option<Arc<FileJobs>>);

impl FileJobs {
    pub(super) fn register(self: &Arc<Self>) -> Result<FileJob> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.closing {
            return Err(LibraryError::Closed);
        }
        state.running += 1;
        Ok(FileJob(Some(self.clone())))
    }

    pub(super) fn active_here(&self) -> bool {
        ACTIVE
            .try_with(|active| active.borrow().contains(&std::ptr::from_ref(self)))
            .unwrap_or(false)
    }

    pub(super) fn seal(&self, start: impl FnOnce() -> Result<()>) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if !state.closing {
            start()?;
            state.closing = true;
        }
        Ok(())
    }

    pub(super) fn wait(&self) -> Option<String> {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        self.idle
            .wait_while(state, |state| state.running != 0)
            .unwrap_or_else(|p| p.into_inner())
            .failure
            .take()
    }

    fn settle(&self, failure: Option<String>) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.closing && state.failure.is_none() {
            state.failure = failure;
        }
        state.running -= 1;
        if state.running == 0 {
            self.idle.notify_all();
        }
    }
}

impl FileJob {
    pub(crate) fn run<T>(mut self, job: impl FnOnce() -> Result<T>) -> Result<T> {
        struct Active;
        impl Drop for Active {
            fn drop(&mut self) {
                ACTIVE.with(|active| active.borrow_mut().pop());
            }
        }

        let jobs = self.0.as_ref().expect("file job permit present");
        ACTIVE.with(|active| active.borrow_mut().push(Arc::as_ptr(jobs)));
        let active = Active;
        let result = job();
        let failure = result.as_ref().err().map(ToString::to_string);
        drop(active);
        if let Some(jobs) = self.0.take() {
            jobs.settle(failure);
        }
        result
    }
}

impl Drop for FileJob {
    fn drop(&mut self) {
        if let Some(jobs) = self.0.take() {
            jobs.settle(std::thread::panicking().then(|| "library file job panicked".into()));
        }
    }
}

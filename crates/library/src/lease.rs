//! Session-local root release barrier. Mark before waiting, so queued reads cannot reopen
//! a folder while existing file handles drain. Database-only reads remain available.
use crate::RootId;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex};

#[derive(Default)]
pub(crate) struct FileAccess {
    state: Mutex<State>,
    idle: Condvar,
}

#[derive(Default)]
struct State {
    released: HashSet<RootId>,
    active: HashMap<RootId, usize>,
}

/// Keeps a root's files in use until dropped. Acquisition never accesses the filesystem.
pub struct FileLease {
    access: Arc<FileAccess>,
    root: RootId,
}

impl FileAccess {
    pub fn acquire(self: &Arc<Self>, root: RootId) -> Option<FileLease> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.released.contains(&root) {
            return None;
        }
        *state.active.entry(root).or_default() += 1;
        Some(FileLease {
            access: Arc::clone(self),
            root,
        })
    }
    pub fn released(&self, root: RootId) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .released
            .contains(&root)
    }
    pub fn suspend(&self, root: RootId) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .released
            .insert(root)
    }
    pub fn wait_idle(&self, root: RootId, timeout: std::time::Duration) -> bool {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let (state, _) = self
            .idle
            .wait_timeout_while(state, timeout, |state| {
                state.active.get(&root).copied().unwrap_or(0) != 0
            })
            .unwrap_or_else(|p| p.into_inner());
        state.active.get(&root).copied().unwrap_or(0) == 0
    }
    pub fn resume(&self, root: RootId) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .released
            .remove(&root)
    }
}

impl Drop for FileLease {
    fn drop(&mut self) {
        let mut state = self.access.state.lock().unwrap_or_else(|p| p.into_inner());
        let count = state
            .active
            .get_mut(&self.root)
            .expect("acquired file lease");
        *count -= 1;
        if *count == 0 {
            state.active.remove(&self.root);
            self.access.idle.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn release_blocks_new_access_and_waits_for_existing_handles() {
        let access = Arc::new(FileAccess::default());
        let lease = access.acquire(RootId(1)).unwrap();
        let other = Arc::clone(&access);
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let changed = other.suspend(RootId(1));
            assert!(other.wait_idle(RootId(1), std::time::Duration::from_secs(5)));
            tx.send(changed).unwrap();
        });
        while !access.released(RootId(1)) {
            std::thread::yield_now();
        }
        assert!(access.acquire(RootId(1)).is_none());
        assert!(access.acquire(RootId(2)).is_some());
        assert!(rx.try_recv().is_err());
        drop(lease);
        assert!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap());
        thread.join().unwrap();
        assert!(!access.suspend(RootId(1)));
        assert!(access.resume(RootId(1)));
        assert!(access.acquire(RootId(1)).is_some());
    }
}

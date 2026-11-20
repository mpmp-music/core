use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

pub(super) struct RetiredDecode {
    thread: Mutex<Option<JoinHandle<()>>>,
    joined: AtomicBool,
}

impl RetiredDecode {
    pub(super) fn new(thread: JoinHandle<()>) -> Self {
        Self {
            thread: Mutex::new(Some(thread)),
            joined: AtomicBool::new(false),
        }
    }

    fn is_finished(&self) -> bool {
        self.joined.load(Ordering::Acquire)
            || self
                .thread
                .try_lock()
                .is_ok_and(|thread| thread.as_ref().is_some_and(JoinHandle::is_finished))
    }

    fn join(&self) {
        let thread = self
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(thread) = thread {
            let _ = thread.join();
            self.joined.store(true, Ordering::Release);
        }
    }
}

pub(super) fn wait_for_retired_decodes(decodes: &[Arc<RetiredDecode>], deadline: Instant) -> bool {
    loop {
        if decodes.iter().all(|decode| decode.is_finished()) {
            return true;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        std::thread::sleep(left.min(Duration::from_millis(5)));
    }
}

pub(super) struct ReleaseStart {
    pub(super) audit: Option<ActionId>,
    pub(super) deadline: Instant,
    pub(super) root: RootId,
    pub(super) changed: bool,
    pub(super) origin: Origin,
    pub(super) confirmation: Option<(ConfirmationOutcome, Option<IdempotencyKey>)>,
}

pub(super) struct RemovalRecovery {
    pub(super) path: String,
    pub(super) participants: Option<std::collections::BTreeSet<u64>>,
}

impl RemovalRecovery {
    pub(super) fn new(path: String) -> Self {
        Self {
            path,
            participants: None,
        }
    }
}

pub(super) struct Releasing {
    pub(super) start: ReleaseStart,
    pub(super) answer: Answer,
}

impl Actor {
    pub(super) fn root_transition_pending(&self, root: RootId) -> Result<bool, ContractError> {
        Ok(self.roots.releasing.contains_key(&root)
            || self
                .catalog
                .root_location(root)?
                .is_some_and(|path| self.root_access.in_progress(&path)))
    }

    /// Only take finished handles; blocked sources are retained for a release retry.
    pub(super) fn reap_retired_decodes(&mut self) {
        self.roots.retired_decodes.retain(|_, handles| {
            handles.retain(|decode| {
                if decode.is_finished() {
                    decode.join();
                    false
                } else {
                    true
                }
            });
            !handles.is_empty()
        });
    }

    pub(super) fn join_retired_decodes(&mut self) {
        for decode in self
            .roots
            .retired_decodes
            .drain()
            .flat_map(|(_, handles)| handles)
        {
            decode.join();
        }
    }

    /// Removal has already committed. Keep failed recovery work retryable even though
    /// another RemoveRoot is now a database no-op; events are published before this call.
    pub(super) fn retry_removed_root(&mut self, root: RootId) -> Result<(), ContractError> {
        self.reap_retired_decodes();
        if let Some(recovery) = self.roots.removed_root_notifications.get_mut(&root) {
            self.root_access
                .notify_recovery(&recovery.path, &mut recovery.participants)
                .map_err(|e| {
                    ContractError::failed(
                    ErrorCode::Io,
                    format!(
                        "root removal committed; participant recovery failed: {}; retry RemoveRoot",
                        e.message
                    ),
                )
                .retryable()
                })?;
            self.roots.removed_root_notifications.remove(&root);
        }
        Ok(())
    }

    pub(super) fn root_drained(&mut self, root: RootId, result: Result<(), ContractError>) {
        let Some(Releasing { start, answer }) = self.roots.releasing.remove(&root) else {
            return;
        };
        if result.is_ok() {
            self.reap_retired_decodes();
        }
        self.publication.cause = Some(start.origin);
        let result = result.map(|()| Receipt {
            root: Some(root),
            audit: start.audit,
            changed: start.changed,
            queue_revision: self.playback.queue.revision(),
            ..Receipt::default()
        });
        let result = if let Some((mut outcome, key)) = start.confirmation {
            outcome.result = Some(Box::new(match result {
                Ok(receipt) => CommandResult::Ok(receipt),
                Err(error) => CommandResult::Err { error },
            }));
            self.publish_confirmation(&outcome, key.as_ref());
            Ok(self.confirmed(outcome))
        } else {
            result
        };
        self.reply(answer, result, true);
        self.publication.cause = None;
    }

    /// Music folders were added, removed or brought back: an agent command's checks that
    /// ran meanwhile are out of date.
    pub(super) fn folders_changed(&mut self) {
        self.generation += 1;
        self.emit(Event::LibraryChanged);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_drain_deadline_survives_shutdown_join() {
        let (unblock, blocked) = crossbeam_channel::bounded::<()>(1);
        let decode = Arc::new(RetiredDecode::new(std::thread::spawn(move || {
            let _ = blocked.recv();
        })));
        let retire = Arc::clone(&decode);
        let joiner = std::thread::spawn(move || retire.join());
        let deadline = Instant::now() + Duration::from_secs(2);
        while !decode
            .thread
            .try_lock()
            .is_ok_and(|thread| thread.is_none())
        {
            assert!(
                Instant::now() < deadline,
                "join did not take the decoder handle"
            );
            std::thread::yield_now();
        }
        let decodes = [decode];
        assert!(!wait_for_retired_decodes(
            &decodes,
            Instant::now() + Duration::from_millis(20)
        ));
        unblock.send(()).unwrap();
        joiner.join().unwrap();
        assert!(wait_for_retired_decodes(&decodes, Instant::now()));
    }
}

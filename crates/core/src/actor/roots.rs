use super::*;

pub(super) type RetiredDecode = Arc<Mutex<Option<std::thread::JoinHandle<()>>>>;

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
                if let Ok(mut thread) = decode.try_lock() {
                    if thread.as_ref().is_some_and(|t| t.is_finished()) {
                        let _ = thread.take().unwrap().join();
                    }
                    thread.is_some()
                } else {
                    true
                }
            });
            !handles.is_empty()
        });
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
            self.roots.retired_decodes.remove(&root);
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

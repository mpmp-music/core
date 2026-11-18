use super::*;

/// A command held for the user's confirmation.
pub(super) struct Held {
    pub(super) sources: Option<Vec<mpmp_library::CoverSource>>,
    pub(super) purge: Option<mpmp_library::PurgeGuard>,
    pub(super) recovery: Option<mpmp_library::CoverRecoveryGuard>,
    pub(super) arrival: u64,
    pub(super) view: PendingConfirmation,
    pub(super) _session: Option<Arc<std::sync::atomic::AtomicBool>>,
    pub(super) command: Command,
    pub(super) origin: Origin,
    pub(super) expires: Instant,
    /// The request's idempotency key, whose cached result follows the answer.
    pub(super) key: Option<IdempotencyKey>,
    /// What an agent's command acts on, as checked and shown to the user: checked again
    /// when the user approves ([`Actor::recheck`]).
    pub(super) targets: Option<Vec<Target>>,
    /// An agent's registration instance when the command was held. If
    /// it is no longer, or registered again, before the command runs (also while it is
    /// checked again once approved), the command is withdrawn.
    pub(super) registered: Option<u64>,
}

impl Actor {
    /// A `Confirm`'s receipt.
    pub(super) fn confirmed(&self, outcome: ConfirmationOutcome) -> Receipt {
        Receipt {
            confirmed: Some(outcome),
            changed: true,
            queue_revision: self.playback.queue.revision(),
            ..Receipt::default()
        }
    }

    /// Withdraws the held commands of the origins `which` picks: refused as if the user had
    /// rejected them (reason `withdrawn`).
    pub(super) fn reject_held(&mut self, which: impl Fn(&Origin) -> bool) {
        let mut tokens: Vec<String> = self
            .confirmations
            .held
            .iter()
            .filter(|(_, held)| which(&held.origin))
            .map(|(token, _)| token.clone())
            .collect();
        tokens.sort();
        for token in tokens {
            let _ = self.resolve_confirmation(&token, ConfirmationReason::Withdrawn);
        }
        let mut ids: Vec<u64> = self
            .requests
            .waiting
            .iter()
            .filter_map(|(id, waiting)| match &waiting.work {
                Work::Recheck { held, .. } if which(&held.origin) => Some(*id),
                _ => None,
            })
            .collect();
        ids.sort();
        for id in ids {
            let Some(waiting) = self.requests.waiting.remove(&id) else {
                continue;
            };
            let Work::Recheck { token, held } = waiting.work else {
                unreachable!("only approval rechecks are withdrawn here");
            };
            let outcome = self.settle(&token, *held, ConfirmationReason::Withdrawn, None);
            let receipt = self.confirmed(outcome);
            self.finish(waiting.answer, Ok(receipt));
        }
    }

    /// Holds a command for the user's confirmation (`view`: what the confirmation shows;
    /// `targets`: for an agent's command, what it acts on as checked and shown, checked
    /// again when the user approves).
    pub(super) fn hold(
        &mut self,
        mut command: Command,
        requester: (&Origin, Option<u64>),
        key: Option<IdempotencyKey>,
        class: OperationClass,
        targets: Option<Vec<Target>>,
        mut view: View,
    ) -> ContractError {
        let (origin, registered) = requester;
        let purge = if let Command::PurgePlaylists { playlists } = &command {
            let guard = match self.catalog.describe_purge(playlists.as_deref()) {
                Ok(guard) => guard,
                Err(error) => return error,
            };
            // Bind `null` now. The held command and all its views name concrete IDs.
            command = Command::PurgePlaylists {
                playlists: Some(
                    guard
                        .playlists
                        .iter()
                        .map(|p| crate::contract::PlaylistId(p.id.0))
                        .collect(),
                ),
            };
            view.summary = format!(
                concat!(
                    "Permanently delete these playlists from the trash (irreversible; review ",
                    "again if the selection changes): {}",
                ),
                serde_json::to_string(
                    &guard
                        .playlists
                        .iter()
                        .map(|p| serde_json::json!({
                        "id":p.id,
                        "name":p.name,
                        "track_count":p.track_count}
                        ))
                        .collect::<Vec<_>>()
                )
                .unwrap_or_default()
            );
            Some(guard)
        } else {
            None
        };
        let recovery = if let Command::ResolveCoverRecovery {
            track,
            resolution,
            selection,
        } = &command
        {
            if !matches!(origin, Origin::Ui) {
                return ContractError::forbidden(
                    ErrorCode::OriginNotAllowed,
                    "cover recovery resolution is UI-only",
                );
            }
            if *resolution == crate::contract::CoverRecoveryResolution::Unknown {
                return ContractError::unknown_variant("cover recovery resolution")
                    .with_field("command.resolution");
            }
            match self.catalog.describe_cover_recovery(
                *track,
                resolution
                    .library_resolution()
                    .expect("validated resolution"),
                selection.as_ref().and_then(|s| s.library_selection()),
            ) {
                Ok(guard) => {
                    view.summary = guard.summary.clone();
                    Some(guard)
                }
                Err(error) => return error,
            }
        } else {
            None
        };
        self.expire_confirmations();
        let cover_bytes = |c: &Command| match c {
            Command::EditCover {
                change: crate::contract::CoverChange::Set { image },
                ..
            } => image.data_base64.len(),
            _ => 0,
        };
        let retained: usize = self
            .confirmations
            .held
            .values()
            .map(|h| cover_bytes(&h.command))
            .sum();
        if retained.saturating_add(cover_bytes(&command)) > 8 * 1024 * 1024 {
            return ContractError::failed(
                ErrorCode::JobBusy,
                "cover confirmation memory budget exceeded",
            )
            .retryable();
        }
        self.confirmations.next_token += 1;
        let token = format!("confirm-{}-{}", unix_ms(), self.confirmations.next_token);
        let expires_at_ms = unix_ms() + self.access.confirmation_ttl.as_millis() as i64;
        let View {
            sources,
            summary,
            scope,
            display_location,
        } = view;
        let confirmation = PendingConfirmation {
            token: token.clone(),
            summary: summary.clone(),
            command: command.clone(),
            requested_by: origin.clone(),
            expires_at_ms,
            operation: Some(OperationInfo {
                class,
                command: command.name().to_owned(),
            }),
            scope,
            display_location,
        };
        self.emit(Event::ConfirmationRequested {
            confirmation: Box::new(confirmation.clone()),
        });
        self.confirmations.held.insert(
            token.clone(),
            Held {
                sources,
                purge,
                recovery,
                arrival: self.confirmations.next_token,
                view: confirmation,
                _session: lock_permissions(&self.access.permissions).pin_session(origin),
                command,
                origin: origin.clone(),
                expires: Instant::now() + self.access.confirmation_ttl,
                key,
                targets,
                registered,
            },
        );
        let mut error = ContractError::new(
            ErrorKind::NeedsConfirmation,
            ErrorCode::NeedsConfirmation,
            format!("waits for the user's confirmation: {summary}"),
        );
        error.details.confirmation = Some(ConfirmationTicket {
            token,
            summary,
            expires_at_ms,
        });
        error
    }

    /// Runs (approved) or drops a held command as its requester, and tells everyone the
    /// outcome.
    pub(super) fn resolve_confirmation(
        &mut self,
        token: &str,
        reason: ConfirmationReason,
    ) -> Result<ConfirmationOutcome, ContractError> {
        self.expire_confirmations();
        let held = self.confirmations.held.remove(token).ok_or_else(|| {
            ContractError::not_found(
                ErrorCode::ConfirmationNotFound,
                format!("confirmation {token} (answered or expired)"),
            )
            .with_missing(vec![ObjectRef::Confirmation {
                token: token.to_owned(),
            }])
        })?;
        // An agent's command checked again here: one whose checks call the file system is
        // checked again off the actor instead (`Flow::Recheck`).
        let checked = (reason == ConfirmationReason::Approved).then(|| {
            if held.targets.is_none() {
                return Ok(());
            }
            self.recheck_of(&held)
                .and_then(|recheck| recheck.run(false))
        });
        Ok(self.settle(token, held, reason, checked))
    }

    /// Runs (approved: `checked` is what its check again found) or drops a held command
    /// as its requester, and tells everyone the outcome.
    pub(super) fn settle(
        &mut self,
        token: &str,
        held: Held,
        reason: ConfirmationReason,
        checked: Option<Result<(), ContractError>>,
    ) -> ConfirmationOutcome {
        // An approval does not survive losing its session or registration before execution.
        let mut reason = match reason {
            ConfirmationReason::Approved if self.withdrawn(&held.origin, held.registered) => {
                ConfirmationReason::Withdrawn
            }
            reason => reason,
        };
        self.confirmations.host_cover_sources = held.sources;
        self.confirmations.host_recovery_guard = held.recovery;
        let mut approved = reason == ConfirmationReason::Approved;
        let result = if approved {
            // The held command's events carry its requester's origin.
            let cause = self.publication.cause.replace(held.origin.clone());
            let outcome = checked.unwrap_or(Ok(())).and_then(|()| {
                match self.command(
                    held.command,
                    &held.origin,
                    held.registered,
                    None,
                    held.targets.as_deref(),
                    held.purge.as_ref(),
                ) {
                    Flow::Done(result) => result,
                    Flow::Withdrawn => {
                        reason = ConfirmationReason::Withdrawn;
                        approved = false;
                        Err(withdrawn())
                    }
                    _ => unreachable!("execution cannot start checks"),
                }
            });
            self.announce_queue();
            self.publication.cause = cause;
            match outcome {
                Ok(receipt) => CommandResult::Ok(receipt),
                Err(error) => {
                    self.note(&error);
                    CommandResult::Err { error }
                }
            }
        } else {
            let message = match reason {
                ConfirmationReason::Withdrawn => {
                    "withdrawn before it ran: its session ended or its agent was removed"
                }
                _ => "the user rejected the command",
            };
            CommandResult::Err {
                error: ContractError::forbidden(ErrorCode::ConfirmationRejected, message),
            }
        };
        self.confirmations.host_cover_sources = None;
        self.confirmations.host_recovery_guard = None;
        let outcome = ConfirmationOutcome {
            token: token.to_owned(),
            approved,
            result: approved.then(|| Box::new(result)),
            reason: Some(reason),
        };
        if let Some(start) = &mut self.roots.release_start {
            start.confirmation = Some((outcome.clone(), held.key));
        } else {
            self.publish_confirmation(&outcome, held.key.as_ref());
        }
        outcome
    }

    pub(super) fn publish_confirmation(
        &mut self,
        outcome: &ConfirmationOutcome,
        key: Option<&IdempotencyKey>,
    ) {
        if let Some(key) = key {
            if outcome.reason == Some(ConfirmationReason::Withdrawn) {
                self.confirmations.idempotency.forget(key);
            } else if let Some(result) = &outcome.result {
                self.confirmations.idempotency.update(key, *result.clone());
            } else {
                self.confirmations.idempotency.update(
                    key,
                    CommandResult::Err {
                        error: ContractError::forbidden(
                            ErrorCode::ConfirmationRejected,
                            "the user rejected the command",
                        ),
                    },
                );
            }
        }
        self.emit(Event::ConfirmationResolved {
            outcome: outcome.clone(),
        });
    }

    pub(super) fn expire_confirmations(&mut self) {
        let now = Instant::now();
        let expired: Vec<String> = self
            .confirmations
            .held
            .iter()
            .filter(|(_, h)| h.expires <= now)
            .map(|(token, _)| token.clone())
            .collect();
        for token in expired {
            let Some(held) = self.confirmations.held.remove(&token) else {
                continue;
            };
            if let Some(key) = &held.key {
                self.confirmations.idempotency.update(
                    key,
                    CommandResult::Err {
                        error: ContractError::forbidden(
                            ErrorCode::ConfirmationExpired,
                            "the user did not answer in time",
                        ),
                    },
                );
            }
            self.emit_as(
                None,
                Event::ConfirmationResolved {
                    outcome: ConfirmationOutcome {
                        token,
                        approved: false,
                        result: None,
                        reason: Some(ConfirmationReason::Expired),
                    },
                },
            );
        }
    }
}

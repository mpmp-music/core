use super::*;

/// A request and who waits for its answer.
pub(super) struct Answer {
    pub(super) request_id: u64,
    pub(super) reply: Sender<Response>,
    pub(super) key: Option<IdempotencyKey>,
    /// As sent (the idempotency cache keeps it so).
    pub(super) command: Command,
    pub(super) origin: Origin,
    /// An agent's registration instance when the request came (if it
    /// is no longer, or registered again, when its checks are done, it is withdrawn).
    pub(super) registered: Option<u64>,
    /// Requests with the same idempotency key and command that came meanwhile, from the
    /// same origin or another session of it: answered as a retry once this one is.
    pub(super) joined: Vec<(u64, Sender<Response>, Origin, Option<u64>)>,
}

/// A request whose checks run off the actor (docs/design/agent-permissions.md).
pub(super) struct Waiting {
    pub(super) answer: Answer,
    /// [`Actor::generation`] when the checks started: if it changed, their answer is out
    /// of date.
    pub(super) generation: u64,
    pub(super) attempts: u32,
    pub(super) work: Work,
}

pub(super) enum Work {
    /// An agent's command.
    Check,
    /// The user's `Confirm` of a held agent command (`token`), checked again.
    Recheck { token: String, held: Box<Held> },
}

/// Checks to run off the actor.
#[derive(Clone)]
enum CheckJob {
    Check(Check),
    Recheck(Recheck),
}

/// What a [`CheckJob`] found.
enum CheckDone {
    Checked(Checked),
    Rechecked(Result<(), ContractError>),
}

impl CheckJob {
    fn run(self, spent: bool) -> CheckDone {
        match self {
            CheckJob::Check(check) => CheckDone::Checked(check.run(spent)),
            CheckJob::Recheck(recheck) => CheckDone::Rechecked(recheck.run(spent)),
        }
    }
}

/// An agent's command whose session ended, or whose agent was removed, before it ran or
/// was held (as for a held one: `confirmation_rejected`; not kept for a retry with its
/// idempotency key, which runs it).
pub(super) fn withdrawn() -> ContractError {
    ContractError::forbidden(
        ErrorCode::ConfirmationRejected,
        "withdrawn: its session ended or its agent was removed",
    )
}

impl Actor {
    /// Answers a request: from the idempotency cache, by joining the same request still
    /// being checked, or by running its command.
    pub(super) fn request(
        &mut self,
        request_id: u64,
        key: Option<IdempotencyKey>,
        command: Command,
        origin: Origin,
        registered: Option<u64>,
        reply: Sender<Response>,
    ) {
        if self.withdrawn(&origin, registered) {
            let _ = reply.send(Response {
                request_id,
                result: CommandResult::Err { error: withdrawn() },
            });
            return;
        }
        // JSON cannot carry non-finite numbers, but Rust callers can. Reject before
        // any retry lookup: serde_json otherwise collapses Some(NaN/Infinity) and
        // None to the same null in the compact fingerprint. Do not cache rejection.
        let non_finite_field = match &command {
            Command::SetVolume { volume } if !volume.is_finite() => Some("command.volume"),
            Command::SetReplayGain {
                preamp_db,
                fallback_preamp_db,
                ..
            } => {
                if preamp_db.is_some_and(|value| !value.is_finite()) {
                    Some("command.preamp_db")
                } else if fallback_preamp_db.is_some_and(|value| !value.is_finite()) {
                    Some("command.fallback_preamp_db")
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(field) = non_finite_field {
            let _ = reply.send(Response {
                request_id,
                result: CommandResult::Err {
                    error: ContractError::invalid(ErrorCode::OutOfRange, "number must be finite")
                        .with_field(field),
                },
            });
            return;
        }
        // The same key while its first request is still being checked: answered as a retry
        // once that one is (the same command), or refused (another).
        if let Some(key) = &key
            && let Some(waiting) = self
                .requests
                .waiting
                .values_mut()
                .map(|w| &mut w.answer)
                .chain(self.roots.releasing.values_mut().map(|r| &mut r.answer))
                .find(|answer| answer.key.as_ref() == Some(key))
        {
            if waiting.command == command {
                waiting.joined.push((request_id, reply, origin, registered));
            } else {
                let _ = reply.send(Response {
                    request_id,
                    result: CommandResult::Err {
                        error: key_reused(),
                    },
                });
            }
            return;
        }
        let cached = key.as_ref().and_then(|key| {
            let (first, result) = self.confirmations.idempotency.get(key)?;
            Some(if first == &command_fingerprint(&command) {
                // `None`: its token is shown once, so it runs again.
                result.clone()?
            } else {
                CommandResult::Err {
                    error: key_reused(),
                }
            })
        });
        match cached {
            Some(result) => {
                // Publish before replying: a query right after the receipt must see the
                // command's effect (read your own write, ).
                self.publish();
                let _ = reply.send(Response { request_id, result });
            }
            None => {
                self.proceed(
                    Answer {
                        request_id,
                        reply,
                        key,
                        command,
                        origin,
                        registered,
                        joined: Vec::new(),
                    },
                    0,
                );
            }
        }
    }

    /// Runs a request's command (`attempts`: how often its checks ran already), and
    /// answers it, now or once its checks off the actor are done.
    fn proceed(&mut self, answer: Answer, attempts: u32) {
        // Its session ended, or its agent was removed (before it came, or while it was
        // checked): withdrawn, whatever the command.
        if self.withdrawn(&answer.origin, answer.registered) {
            return self.reply(answer, Err(withdrawn()), false);
        }
        self.publication.cause = Some(answer.origin.clone());
        let flow = self.run_command(
            answer.command.clone(),
            &answer.origin,
            answer.registered,
            answer.key.clone(),
        );
        match flow {
            Flow::Withdrawn => self.reply(answer, Err(withdrawn()), false),
            Flow::Done(result) => self.finish(answer, result),
            Flow::Check(check) => {
                self.wait(answer, attempts, Work::Check, CheckJob::Check(check));
            }
            Flow::Recheck { token, held } => match self.recheck_of(&held) {
                Ok(recheck) => {
                    self.wait(
                        answer,
                        attempts,
                        Work::Recheck { token, held },
                        CheckJob::Recheck(recheck),
                    );
                }
                Err(error) => {
                    let outcome = self.settle(
                        &token,
                        *held,
                        ConfirmationReason::Approved,
                        Some(Err(error)),
                    );
                    let receipt = self.confirmed(outcome);
                    self.finish(answer, Ok(receipt));
                }
            },
        }
        self.publication.cause = None;
    }

    /// Answers a request; then those that joined it, as retries (from the idempotency
    /// cache, or run: a token is shown once).
    pub(super) fn finish(&mut self, answer: Answer, result: Result<Receipt, ContractError>) {
        if let Some(start) = self.roots.release_start.take() {
            debug_assert!(result.is_ok());
            let root = start.root;
            let deadline = start.deadline;
            let decodes = self
                .roots
                .retired_decodes
                .get(&root)
                .cloned()
                .unwrap_or_default();
            self.roots
                .releasing
                .insert(root, Releasing { start, answer });
            let catalog = Arc::clone(&self.catalog);
            let native = Arc::clone(&self.root_access);
            let tx = self.tx.clone();
            let spawned = std::thread::Builder::new()
                .name("root-drain".into())
                .spawn(move || {
                    struct CompletionGuard {
                        tx: Sender<Msg>,
                        root: RootId,
                        armed: bool,
                    }
                    impl Drop for CompletionGuard {
                        fn drop(&mut self) {
                            if !self.armed {
                                return;
                            }
                            let _ = self.tx.send(Msg::RootDrained {
                                root: self.root,
                                result: Err(ContractError::failed(
                                    ErrorCode::Io,
                                    "root drain worker panicked",
                                )
                                .retryable()),
                            });
                        }
                    }
                    let mut guard = CompletionGuard {
                        tx,
                        root,
                        armed: true,
                    };
                    let joined = loop {
                        let joined = decodes.iter().all(|decode| {
                            let mut thread = decode.lock().unwrap_or_else(|e| e.into_inner());
                            if thread.as_ref().is_some_and(|t| t.is_finished()) {
                                let _ = thread.take().unwrap().join();
                            }
                            thread.is_none()
                        });
                        if joined || Instant::now() >= deadline {
                            break joined;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    };
                    let drained = if joined {
                        catalog.wait_root_idle(
                            root,
                            deadline.saturating_duration_since(Instant::now()),
                        )
                    } else {
                        Ok(false)
                    };
                    let result = match drained {
                        Ok(true) =>
                        catalog
                            .root_location(root)
                            .and_then(|path| native.notify(&path.unwrap_or_default(), true)),
                        Err(error) => Err(error),
                        Ok(false) => Err(ContractError::failed(ErrorCode::Io,
                        concat!(
"decode retirement and root file access did not drain within a total of 5 ",
"seconds; access remains suspended",
)).retryable()),
                    };
                    let _ = guard.tx.send(Msg::RootDrained { root, result });
                    guard.armed = false;
                });
            if let Err(error) = spawned {
                self.root_drained(
                    root,
                    Err(ContractError::failed(ErrorCode::Io, error.to_string()).retryable()),
                );
            }
            return;
        }
        self.reply(answer, result, true);
    }

    /// [`Actor::finish`]; `cache`: the result is kept for retries with the request's
    /// idempotency key (not a withdrawal: the command never ran, so a retry runs it).
    pub(super) fn reply(
        &mut self,
        answer: Answer,
        result: Result<Receipt, ContractError>,
        cache: bool,
    ) {
        let result = match result {
            Ok(receipt) => CommandResult::Ok(receipt),
            Err(error) => CommandResult::Err { error },
        };
        self.announce_queue();
        if let CommandResult::Err { error } = &result {
            self.note(error);
        }
        let Answer {
            request_id,
            reply,
            key,
            command,
            joined,
            ..
        } = answer;
        // A token is shown once: a retry registers or renews again (the key stays this
        // command's).
        let shows_token = matches!(
            command,
            Command::RegisterAgent { .. } | Command::RenewAgentToken { .. }
        );
        if let Some(key) = &key
            && cache
        {
            let kept = (!shows_token).then(|| result.clone());
            self.confirmations
                .idempotency
                .insert(key.clone(), command.clone(), kept);
        }
        // Publish before replying: a query right after the receipt must see the command's
        // effect (read your own write, ).
        self.publish();
        let _ = reply.send(Response { request_id, result });
        for (request_id, reply, origin, registered) in joined {
            self.request(
                request_id,
                key.clone(),
                command.clone(),
                origin,
                registered,
                reply,
            );
        }
    }

    /// Runs `job` off the actor for `answer`; its result comes back as a message. With
    /// too many running, or no thread to run on (or after `MAX_ATTEMPTS`), it runs here:
    /// with a time limit used up already, or (after `MAX_ATTEMPTS`) a fresh one.
    fn wait(&mut self, answer: Answer, attempts: u32, work: Work, job: CheckJob) {
        let id = self.requests.next_waiting;
        self.requests.next_waiting += 1;
        self.requests.waiting.insert(
            id,
            Waiting {
                answer,
                generation: self.generation,
                attempts,
                work,
            },
        );
        if attempts >= MAX_ATTEMPTS {
            return self.deliver(id, job.run(false));
        }
        if self.requests.waiting.len() > MAX_CHECKS {
            return self.deliver(id, job.run(true));
        }
        let tx = self.tx.clone();
        let off = job.clone();
        let spawned = std::thread::Builder::new()
            .name("core-agent-check".into())
            .spawn(move || {
                let _ = tx.send(match off.run(false) {
                    CheckDone::Checked(checked) => Msg::Checked { id, checked },
                    CheckDone::Rechecked(result) => Msg::Rechecked { id, result },
                });
            });
        if spawned.is_err() {
            self.deliver(id, job.run(true));
        }
    }

    fn deliver(&mut self, id: u64, done: CheckDone) {
        match done {
            CheckDone::Checked(checked) => self.on_checked(id, checked),
            CheckDone::Rechecked(result) => self.on_rechecked(id, result),
        }
    }

    /// An agent command's checks are done: it is refused, held for the user or run, unless
    /// what they depended on changed meanwhile (then they run again), or its session ended
    /// or its agent was removed (then it is withdrawn).
    pub(super) fn on_checked(&mut self, id: u64, checked: Checked) {
        let Some(waiting) = self.requests.waiting.remove(&id) else {
            return;
        };
        let Waiting {
            answer,
            generation,
            attempts,
            work,
        } = waiting;
        let Work::Check = work else {
            unreachable!("a check's answer for a check again");
        };
        // Out of date: run again (`proceed` first withdraws it if its origin is gone).
        if generation != self.generation {
            return self.proceed(answer, attempts + 1);
        }
        if self.withdrawn(&answer.origin, answer.registered) {
            return self.reply(answer, Err(withdrawn()), false);
        }
        self.publication.cause = Some(answer.origin.clone());
        match self.conclude(
            checked,
            &answer.origin,
            answer.registered,
            answer.key.clone(),
        ) {
            Flow::Done(result) => self.finish(answer, result),
            Flow::Withdrawn => self.reply(answer, Err(withdrawn()), false),
            _ => unreachable!("a concluded check cannot start more checks"),
        }
        self.publication.cause = None;
    }

    /// An approved command's check again is done: it runs (or fails), and the `Confirm`
    /// is answered; unless grants or music folders changed meanwhile (then it is checked
    /// again).
    pub(super) fn on_rechecked(&mut self, id: u64, result: Result<(), ContractError>) {
        let Some(waiting) = self.requests.waiting.remove(&id) else {
            return;
        };
        let Waiting {
            answer,
            generation,
            attempts,
            work,
        } = waiting;
        let Work::Recheck { token, held } = work else {
            unreachable!("a check again's answer for a check");
        };
        self.publication.cause = Some(answer.origin.clone());
        if self.withdrawn(&held.origin, held.registered) {
            // Its origin ended meanwhile: withdrawn before checking grants again.
            let outcome = self.settle(&token, *held, ConfirmationReason::Withdrawn, None);
            let receipt = self.confirmed(outcome);
            self.finish(answer, Ok(receipt));
        } else if generation != self.generation {
            match self.recheck_of(&held) {
                Ok(recheck) => self.wait(
                    answer,
                    attempts + 1,
                    Work::Recheck { token, held },
                    CheckJob::Recheck(recheck),
                ),
                Err(error) => {
                    let outcome = self.settle(
                        &token,
                        *held,
                        ConfirmationReason::Approved,
                        Some(Err(error)),
                    );
                    let receipt = self.confirmed(outcome);
                    self.finish(answer, Ok(receipt));
                }
            }
        } else {
            let outcome = self.settle(&token, *held, ConfirmationReason::Approved, Some(result));
            let receipt = self.confirmed(outcome);
            self.finish(answer, Ok(receipt));
        }
        self.publication.cause = None;
    }

    /// Whether an agent command's origin is gone: its session ended, or its agent (that
    /// was registered when the command came, `registered`) removed, or registered again.
    pub(super) fn withdrawn(&self, origin: &Origin, registered: Option<u64>) -> bool {
        let Origin::Agent { name, session } = origin else {
            return false;
        };
        let permissions = lock_permissions(&self.access.permissions);
        session
            .as_deref()
            .is_some_and(|s| permissions.session_ended(name, s))
            || registered.is_some_and(|at| permissions.registration(name) != Some(at))
    }

    /// What follows an agent command's checks: refused, held, or run.
    pub(super) fn conclude(
        &mut self,
        checked: Checked,
        origin: &Origin,
        registered: Option<u64>,
        key: Option<IdempotencyKey>,
    ) -> Flow {
        if self.withdrawn(origin, registered) {
            return Flow::Withdrawn;
        }
        match checked {
            Checked::Refused(error) => Flow::Done(Err(error)),
            Checked::Run {
                command,
                grant,
                targets,
            } => self.command(command, origin, registered, grant, Some(&targets), None),
            Checked::Hold {
                command,
                class,
                targets,
                view,
            } => Flow::Done(Err(self.hold(
                command,
                (origin, registered),
                key,
                class,
                Some(targets),
                *view,
            ))),
        }
    }

    /// The check again of a held agent command, with its grants as they are now.
    pub(super) fn recheck_of(&self, held: &Held) -> Result<Recheck, ContractError> {
        let class = self.class_of(&held.command)?;
        let candidates = self.candidates(&held.command, &held.origin, class)?;
        Ok(Recheck {
            sources: held.sources.clone(),
            command: held.command.clone(),
            origin: held.origin.clone(),
            shown: held.targets.clone().unwrap_or_default(),
            class,
            candidates,
            catalog: Arc::clone(&self.catalog),
            locations: Arc::clone(&self.access.locations),
            limits: self.access.agent_fs.clone(),
        })
    }
}

use super::*;

/// Locks the permissions; a panic while they were locked leaves them usable.
pub(crate) fn lock_permissions(permissions: &Mutex<Permissions>) -> MutexGuard<'_, Permissions> {
    permissions.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The user's commands about agents and grants (not answers to confirmations): they need
/// the permissions to have been read.
pub(super) fn manages_permissions(command: &Command) -> bool {
    matches!(
        command,
        Command::RegisterAgent { .. }
            | Command::RenewAgentToken { .. }
            | Command::RemoveAgent { .. }
            | Command::GrantPermission { .. }
            | Command::RevokePermission { .. }
    )
}

impl Actor {
    /// Checks a `GrantPermission` and makes its grant (not yet stored).
    pub(super) fn new_grant(
        &mut self,
        agent: String,
        session: Option<String>,
        operation: Operation,
        scope: Scope,
        until: Expiry,
        effect: Effect,
    ) -> Result<Grant, ContractError> {
        let invalid = |what: String, field: &str| {
            ContractError::invalid(ErrorCode::InvalidRequest, what).with_field(field)
        };
        // Values this build does not know first, as for every request.
        if matches!(operation, Operation::Unknown) {
            return Err(ContractError::unknown_variant("operation").with_field("command.operation"));
        }
        if matches!(
            operation,
            Operation::Class {
                class: OperationClass::Unknown
            }
        ) {
            return Err(ContractError::unknown_variant("operation class")
                .with_field("command.operation.class"));
        }
        if matches!(scope, Scope::Unknown) {
            return Err(ContractError::unknown_variant("scope").with_field("command.scope"));
        }
        if matches!(until, Expiry::Unknown) {
            return Err(ContractError::unknown_variant("expiry").with_field("command.until"));
        }
        if !matches!(effect, Effect::Allow | Effect::Forbid) {
            return Err(ContractError::unknown_variant("effect").with_field("command.effect"));
        }
        permissions::check_grant_agent(&agent, "command.agent")?;
        if let Some(session) = &session {
            if session.trim().is_empty() || session.chars().count() > MAX_SESSION {
                return Err(invalid(
                    format!("a session is 1 to {MAX_SESSION} characters, not blank"),
                    "command.session",
                ));
            }
            // A session ends with its connection: a grant for it cannot outlast core.
            if until != Expiry::Session {
                return Err(invalid(
                    "a grant for one session lasts for the session (`until`: `session`)".into(),
                    "command.until",
                ));
            }
            // … nor be given once it has ended (the dialog answered after the agent left).
            if lock_permissions(&self.access.permissions).session_ended(&agent, session) {
                return Err(ContractError::invalid(
                    ErrorCode::SessionEnded,
                    format!(
                        "{agent}'s session {session} has ended: a grant for it could never apply"
                    ),
                )
                .with_field("command.session"));
            }
        }
        if let Operation::Command { command } = &operation
            && !COMMANDS.iter().any(|s| s.name == command)
        {
            return Err(ContractError::unsupported(
                ErrorCode::UnknownCommand,
                format!("this core does not know the command {command:?}"),
            )
            .with_field("command.operation.command"));
        }
        if !permissions::grantable(effect, &operation) {
            let why = match effect {
                Effect::Allow => {
                    "an agent may do this without asking anyway, or it is never allowed \
                     without asking (permanently deleting library data, the user's own \
                     commands)"
                }
                _ => "only the user may send the user's own commands",
            };
            return Err(ContractError::invalid(
                ErrorCode::NotGrantable,
                format!("this grant could never apply: {why}"),
            )
            .with_field("command.operation"));
        }
        if scope != Scope::All && !permissions::takes_scope(&operation) {
            return Err(invalid(
                "this operation acts on nothing in particular: only the scope `all` applies".into(),
                "command.scope",
            ));
        }
        let scope = match scope {
            Scope::Library { root, prefix } => {
                let Some(root_location) = self.catalog.root_location(root)? else {
                    return Err(ContractError::not_found(
                        ErrorCode::RootNotFound,
                        format!("library root {}", root.0),
                    )
                    .with_missing(vec![ObjectRef::Root { id: root }])
                    .with_field("command.scope.root"));
                };
                let prefix = prefix.unwrap_or_default();
                // Also a place in the folder on this platform (no `C:`, no stream names):
                // a prefix that could name no place would never apply.
                let prefix = (prefix.chars().count() <= MAX_PREFIX)
                    .then(|| permissions::normalize_prefix(&prefix).ok())
                    .flatten()
                    .filter(|p| {
                        p.as_deref().is_none_or(|p| {
                            self.access.locations.child(&root_location, p).is_some()
                        })
                    })
                    .ok_or_else(|| {
                        invalid(
                            format!(
                                "a prefix is up to {MAX_PREFIX} characters of `/`-separated \
                                 names of folders this platform can have (none empty, `.` or \
                                 `..`, and no backslash)"
                            ),
                            "command.scope.prefix",
                        )
                    })?;
                Scope::Library { root, prefix }
            }
            Scope::Location { location } => {
                let normal = (location.chars().count() <= MAX_LOCATION)
                    .then(|| self.access.locations.normalize(&location))
                    .flatten()
                    .ok_or_else(|| {
                        ContractError::invalid(
                            ErrorCode::InvalidLocation,
                            format!(
                                "{location:?} is not a location this platform can read (at \
                                 most {MAX_LOCATION} characters)"
                            ),
                        )
                        .with_field("command.scope.location")
                    })?;
                if normal != location {
                    return Err(ContractError::invalid(
                        ErrorCode::LocationNotNormal,
                        "grant location is not normal; normalize and review the scope again",
                    )
                    .with_field("command.scope.location"));
                }
                Scope::Location { location: normal }
            }
            other => other,
        };
        let now = unix_ms();
        match until {
            Expiry::At { at_ms } if at_ms <= now || at_ms > MAX_SAFE_INTEGER as i64 => {
                return Err(ContractError::invalid(
                    ErrorCode::OutOfRange,
                    "a grant must end in the future",
                )
                .with_field("command.until.at_ms"));
            }
            _ => {}
        }
        let id = match self.catalog.next_grant_id()? {
            Some(id) => id,
            None => {
                self.access.next_grant += 1;
                GrantId(self.access.next_grant)
            }
        };
        Ok(Grant {
            id,
            agent,
            session,
            operation,
            scope,
            until,
            effect,
            created_at_ms: now,
        })
    }

    /// An agent's session ended: its grants for the session go, and what it still waits
    /// for is refused, as if the user had rejected it.
    pub(super) fn end_session(&mut self, agent: &str, session: &str) {
        let removed = lock_permissions(&self.access.permissions).end_session(agent, session);
        if !removed.is_empty() {
            self.emit_as(None, Event::PermissionsChanged);
        }
        self.reject_held(|o| {
            matches!(o, Origin::Agent { name, session: Some(s) } if name == agent && s == session)
        });
    }

    /// Prunes the grants that expired, and says so.
    pub(super) fn expire_grants(&mut self) {
        let expired = lock_permissions(&self.access.permissions).take_expired(unix_ms());
        if expired.is_empty() {
            return;
        }
        let stored: Vec<GrantId> = expired
            .iter()
            .filter(|g| g.until != Expiry::Session)
            .map(|g| g.id)
            .collect();
        if !stored.is_empty()
            && let Err(err) = self.catalog.remove_grants(&stored)
        {
            tracing::warn!("cannot delete expired grants: {err}");
            self.note(&err);
        }
        self.emit_as(None, Event::PermissionsChanged);
    }
}

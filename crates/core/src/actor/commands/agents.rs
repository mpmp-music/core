use super::super::*;
use crate::contract::*;

impl Actor {
    pub(in crate::actor) fn command_confirm(
        &mut self,
        token: String,
        receipt: &mut Receipt,
    ) -> Result<bool, ContractError> {
        let changed = {
            receipt.confirmed =
                Some(self.resolve_confirmation(&token, ConfirmationReason::Approved)?);
            true
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_reject(
        &mut self,
        token: String,
        receipt: &mut Receipt,
    ) -> Result<bool, ContractError> {
        let changed = {
            receipt.confirmed =
                Some(self.resolve_confirmation(&token, ConfirmationReason::Rejected)?);
            true
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_register_agent(
        &mut self,
        name: String,
        receipt: &mut Receipt,
    ) -> Result<bool, ContractError> {
        let changed = {
            permissions::check_agent_name(&name, "command.name")?;
            let taken = || {
                ContractError::invalid(
                    ErrorCode::AlreadyExists,
                    format!("an agent named {name:?} is registered already"),
                )
                .with_field("command.name")
            };
            if lock_permissions(&self.access.permissions)
                .agent(&name)
                .is_some()
            {
                return Err(taken());
            }
            let token = permissions::new_token()?;
            let agent = StoredAgent {
                name,
                token_hash: Some(permissions::token_hash(&token)),
                created_at_ms: unix_ms(),
                last_seen_at_ms: None,
            };
            self.catalog.add_agent(&agent)?;
            lock_permissions(&self.access.permissions).add_agent(agent);
            receipt.agent_token = Some(token);
            self.emit(Event::AgentsChanged);
            true
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_renew_agent_token(
        &mut self,
        agent: String,
        receipt: &mut Receipt,
    ) -> Result<bool, ContractError> {
        let changed = {
            if lock_permissions(&self.access.permissions)
                .agent(&agent)
                .is_none()
            {
                return Err(agent_not_found(&agent, "command.agent"));
            }
            let token = permissions::new_token()?;
            let hash = permissions::token_hash(&token);
            self.catalog.set_agent_token(&agent, &hash)?;
            lock_permissions(&self.access.permissions).set_token(&agent, hash);
            receipt.agent_token = Some(token);
            self.emit(Event::AgentsChanged);
            true
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_remove_agent(
        &mut self,
        agent: String,
    ) -> Result<bool, ContractError> {
        let changed = {
            if lock_permissions(&self.access.permissions)
                .agent(&agent)
                .is_none()
            {
                return Err(agent_not_found(&agent, "command.agent"));
            }
            self.catalog.remove_agent(&agent)?;
            let revoked = lock_permissions(&self.access.permissions).remove_agent(&agent);
            self.emit(Event::AgentsChanged);
            if !revoked.is_empty() {
                self.emit(Event::PermissionsChanged);
            }
            // What it still waits for is refused, as if the user had rejected it.
            self.reject_held(|o| matches!(o, Origin::Agent { name, .. } if *name == agent));
            true
        };
        Ok(changed)
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::actor) fn command_grant_permission(
        &mut self,
        agent: String,
        session: Option<String>,
        operation: Operation,
        scope: Scope,
        until: Expiry,
        effect: Effect,
        receipt: &mut Receipt,
    ) -> Result<bool, ContractError> {
        let changed = {
            let grant = self.new_grant(agent, session, operation, scope, until, effect)?;
            if grant.until != Expiry::Session {
                self.catalog.add_grant(&grant)?;
            }
            receipt.grant = Some(grant.id);
            lock_permissions(&self.access.permissions).add_grant(grant);
            self.emit(Event::PermissionsChanged);
            true
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_revoke_permission(
        &mut self,
        grant: GrantId,
    ) -> Result<bool, ContractError> {
        let changed = {
            let until = lock_permissions(&self.access.permissions)
                .grant(grant, unix_ms())
                .map(|g| g.until)
                .ok_or_else(|| grant_not_found(grant, "command.grant"))?;
            if until != Expiry::Session {
                self.catalog.remove_grants(&[grant])?;
            }
            lock_permissions(&self.access.permissions).remove_grant(grant);
            self.emit(Event::PermissionsChanged);
            true
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_unknown(&mut self) -> Result<bool, ContractError> {
        Err(ContractError::unsupported(
            ErrorCode::UnknownCommand,
            "this core does not know that command",
        ))
    }
}

use super::*;

impl AgentPermissions for Library {
    fn load_permissions(&self) -> Result<StoredPermissions, ContractError> {
        let agents = Library::agents(self)
            .map_err(library_error)?
            .into_iter()
            .map(|a| {
                // Unreadable: listed (so it can be removed or given a new token), but no
                // token matches it.
                let token_hash = <[u8; 32]>::try_from(a.token_hash.as_slice()).ok();
                if token_hash.is_none() {
                    tracing::warn!(agent = %a.name, "an agent's token hash is unreadable");
                }
                StoredAgent {
                    name: a.name,
                    token_hash,
                    created_at_ms: a.created_at,
                    last_seen_at_ms: a.last_seen_at,
                }
            })
            .collect();
        let grants = Library::grants(self)
            .map_err(library_error)?
            .into_iter()
            .map(|g| Grant {
                id: GrantId(g.id),
                agent: g.agent,
                session: g.session,
                // Written by core in the contract's shapes; a shape from a newer build
                // reads as `unknown` and never matches.
                operation: serde_json::from_str(&g.operation).unwrap_or(Operation::Unknown),
                scope: serde_json::from_str(&g.scope).unwrap_or(Scope::Unknown),
                until: g
                    .expires_at
                    .map_or(Expiry::Forever, |at_ms| Expiry::At { at_ms }),
                effect: serde_json::from_value(serde_json::Value::String(g.effect))
                    .unwrap_or(Effect::Unknown),
                created_at_ms: g.created_at,
            })
            .collect();
        Ok(StoredPermissions { agents, grants })
    }

    fn add_agent(&self, agent: &StoredAgent) -> Result<(), ContractError> {
        let added = Library::add_agent(
            self,
            mpmp_library::AgentRecord {
                name: agent.name.clone(),
                token_hash: agent.token_hash.map(|h| h.to_vec()).unwrap_or_default(),
                created_at: agent.created_at_ms,
                last_seen_at: agent.last_seen_at_ms,
            },
        )
        .map_err(library_error)?;
        if added {
            Ok(())
        } else {
            Err(ContractError::invalid(
                ErrorCode::AlreadyExists,
                format!("an agent named {:?} is registered already", agent.name),
            )
            .with_field("command.name"))
        }
    }

    fn set_agent_token(&self, name: &str, token_hash: &[u8; 32]) -> Result<(), ContractError> {
        Library::set_agent_token(self, name, token_hash.to_vec())
            .map(drop)
            .map_err(library_error)
    }

    fn set_agent_seen(&self, name: &str, at_ms: i64) -> Result<(), ContractError> {
        Library::set_agent_seen(self, name, at_ms).map_err(library_error)
    }

    fn remove_agent(&self, name: &str) -> Result<(), ContractError> {
        Library::remove_agent(self, name)
            .map(drop)
            .map_err(library_error)
    }

    fn add_grant(&self, grant: &Grant) -> Result<(), ContractError> {
        let json = |v: serde_json::Result<String>| {
            v.map_err(|e| ContractError::failed(ErrorCode::Io, e.to_string()))
        };
        let expires_at = match grant.until {
            Expiry::At { at_ms } => Some(at_ms),
            _ => None,
        };
        Library::add_grant(
            self,
            mpmp_library::GrantRecord {
                id: grant.id.0,
                agent: grant.agent.clone(),
                session: grant.session.clone(),
                operation: json(serde_json::to_string(&grant.operation))?,
                scope: json(serde_json::to_string(&grant.scope))?,
                expires_at,
                effect: match grant.effect {
                    Effect::Allow => "allow",
                    Effect::Forbid => "forbid",
                    _ => {
                        return Err(ContractError::invalid(
                            ErrorCode::InvalidRequest,
                            "a grant's effect must be allow or forbid",
                        ));
                    }
                }
                .into(),
                created_at: grant.created_at_ms,
            },
        )
        .map_err(library_error)
    }

    fn remove_grants(&self, ids: &[GrantId]) -> Result<(), ContractError> {
        let ids: Vec<i64> = ids.iter().map(|g| g.0).collect();
        Library::remove_grants(self, &ids)
            .map(drop)
            .map_err(library_error)
    }

    fn next_grant_id(&self) -> Result<Option<GrantId>, ContractError> {
        Ok(Some(GrantId(
            Library::next_grant_id(self).map_err(library_error)?,
        )))
    }
}

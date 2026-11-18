//! Storage of agent permissions for core (introduced in schema v7; docs/design/agent-permissions.md):
//! registered agents with the hash of their token, and the user's grants that outlive core
//! (until a time, or forever). Core owns the meaning: the library stores what it is given
//! (`operation`, `scope` and `effect` are opaque text) and hands out grant ids.

use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;
use crate::handle::Library;

/// A registered agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRecord {
    /// Unique.
    pub name: String,
    /// A hash of the agent's token (core computes it; the token itself is never stored).
    pub token_hash: Vec<u8>,
    /// Unix ms.
    pub created_at: i64,
    /// Unix ms of the last time the agent's token was presented.
    pub last_seen_at: Option<i64>,
}

/// A stored grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRecord {
    /// From [`Library::next_grant_id`].
    pub id: i64,
    pub agent: String,
    pub session: Option<String>,
    pub operation: String,
    pub scope: String,
    /// Unix ms; `None`: never.
    pub expires_at: Option<i64>,
    pub effect: String,
    /// Unix ms.
    pub created_at: i64,
}

const GRANT_ID_KEY: &str = "grant_id";

impl Library {
    /// The registered agents, by name.
    pub fn agents(&self) -> Result<Vec<AgentRecord>> {
        self.inner().pool.read(|conn| {
            let rows = conn
                .prepare_cached(
                    "SELECT name, token_hash, created_at, last_seen_at FROM agents ORDER BY name",
                )?
                .query_map([], |row| {
                    Ok(AgentRecord {
                        name: row.get(0)?,
                        token_hash: row.get(1)?,
                        created_at: row.get(2)?,
                        last_seen_at: row.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })
    }

    /// Registers an agent: false (and nothing changes) if one of that name exists.
    pub fn add_agent(&self, agent: AgentRecord) -> Result<bool> {
        self.inner().writer.run(move |conn| {
            Ok(conn.execute(
                "INSERT OR IGNORE INTO agents (name, token_hash, created_at, last_seen_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    agent.name,
                    agent.token_hash,
                    agent.created_at,
                    agent.last_seen_at
                ],
            )? > 0)
        })
    }

    /// Replaces an agent's token hash: false if there is no such agent.
    pub fn set_agent_token(&self, name: &str, token_hash: Vec<u8>) -> Result<bool> {
        let name = name.to_owned();
        self.inner().writer.run(move |conn| {
            Ok(conn.execute(
                "UPDATE agents SET token_hash = ?2 WHERE name = ?1",
                params![name, token_hash],
            )? > 0)
        })
    }

    /// Records when an agent was last seen (its token presented).
    pub fn set_agent_seen(&self, name: &str, at: i64) -> Result<()> {
        let name = name.to_owned();
        self.inner().writer.run(move |conn| {
            conn.execute(
                "UPDATE agents SET last_seen_at = ?2 WHERE name = ?1",
                params![name, at],
            )?;
            Ok(())
        })
    }

    /// Removes an agent and every grant stored for it, in one transaction: false if there
    /// was no such agent (its grants, if any, are removed all the same).
    pub fn remove_agent(&self, name: &str) -> Result<bool> {
        let name = name.to_owned();
        self.inner().writer.run(move |conn| {
            let tx = conn.transaction()?;
            tx.execute("DELETE FROM grants WHERE agent = ?1", [&name])?;
            let removed = tx.execute("DELETE FROM agents WHERE name = ?1", [&name])? > 0;
            tx.commit()?;
            Ok(removed)
        })
    }

    /// The stored grants, by id (expired ones included: the caller prunes them).
    pub fn grants(&self) -> Result<Vec<GrantRecord>> {
        self.inner().pool.read(grants)
    }

    /// Stores a grant (its id from [`Library::next_grant_id`]).
    pub fn add_grant(&self, grant: GrantRecord) -> Result<()> {
        self.inner().writer.run(move |conn| {
            conn.execute(
                "INSERT INTO grants
                     (id, agent, session, operation, scope, expires_at, effect, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    grant.id,
                    grant.agent,
                    grant.session,
                    grant.operation,
                    grant.scope,
                    grant.expires_at,
                    grant.effect,
                    grant.created_at
                ],
            )?;
            Ok(())
        })
    }

    /// Removes stored grants: how many were there.
    pub fn remove_grants(&self, ids: &[i64]) -> Result<u64> {
        let ids = ids.to_vec();
        self.inner().writer.run(move |conn| {
            let tx = conn.transaction()?;
            let mut removed = 0;
            {
                let mut delete = tx.prepare_cached("DELETE FROM grants WHERE id = ?1")?;
                for id in ids {
                    removed += delete.execute([id])? as u64;
                }
            }
            tx.commit()?;
            Ok(removed)
        })
    }

    /// A new grant id, never handed out before (also not for a grant that is not stored,
    /// such as one for a session only).
    pub fn next_grant_id(&self) -> Result<i64> {
        self.inner().writer.run(|conn| next_grant_id(conn))
    }

    /// The location of a library root, active or removed; `None` if unknown.
    pub fn root_path(&self, id: crate::RootId) -> Result<Option<String>> {
        self.inner().pool.read(|conn| {
            Ok(conn
                .prepare_cached("SELECT path FROM roots WHERE id = ?1")?
                .query_row([id.0], |row| row.get(0))
                .optional()?)
        })
    }

    /// The library roots an action changed: the root of a `root_add` / `root_remove`, or
    /// of an undo of one; none for other actions.
    pub fn action_roots(&self, id: crate::ActionId) -> Result<Vec<crate::RootId>> {
        self.inner()
            .pool
            .read(|conn| crate::journal::roots_of(conn, id))
    }
}

fn grants(conn: &Connection) -> Result<Vec<GrantRecord>> {
    let rows = conn
        .prepare_cached(
            "SELECT id, agent, session, operation, scope, expires_at, effect, created_at
             FROM grants ORDER BY id",
        )?
        .query_map([], |row| {
            Ok(GrantRecord {
                id: row.get(0)?,
                agent: row.get(1)?,
                session: row.get(2)?,
                operation: row.get(3)?,
                scope: row.get(4)?,
                expires_at: row.get(5)?,
                effect: row.get(6)?,
                created_at: row.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

fn next_grant_id(conn: &Connection) -> Result<i64> {
    // Past every stored id too, should the counter ever be lost.
    Ok(conn.query_row(
        "INSERT INTO meta (key, value)
         VALUES (?1, (SELECT COALESCE(MAX(id), 0) + 1 FROM grants))
         ON CONFLICT (key) DO UPDATE SET value = value + 1
         RETURNING value",
        [GRANT_ID_KEY],
        |row| row.get(0),
    )?)
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests {
    use super::*;
    use crate::tests::fixture;

    fn agent(name: &str, hash: u8) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            token_hash: vec![hash; 32],
            created_at: 10,
            last_seen_at: None,
        }
    }

    fn grant(id: i64, agent: &str) -> GrantRecord {
        GrantRecord {
            id,
            agent: agent.into(),
            session: Some("s1".into()),
            operation: r#"{"type":"class","class":"local_path"}"#.into(),
            scope: r#"{"type":"all"}"#.into(),
            expires_at: Some(99),
            effect: "allow".into(),
            created_at: 20,
        }
    }

    #[test]
    fn agents_and_grants_are_stored() {
        let f = fixture();
        let lib = &f.lib;
        assert!(lib.add_agent(agent("codex", 1)).unwrap());
        assert!(lib.add_agent(agent("claude", 2)).unwrap());
        assert!(
            !lib.add_agent(agent("claude", 3)).unwrap(),
            "names are unique"
        );
        assert!(lib.set_agent_token("claude", vec![4; 32]).unwrap());
        assert!(!lib.set_agent_token("nobody", vec![4; 32]).unwrap());
        lib.set_agent_seen("codex", 50).unwrap();
        let agents = lib.agents().unwrap();
        assert_eq!(
            agents,
            [
                AgentRecord {
                    token_hash: vec![4; 32],
                    ..agent("claude", 2)
                },
                AgentRecord {
                    last_seen_at: Some(50),
                    ..agent("codex", 1)
                }
            ]
        );

        let ids: Vec<i64> = (0..3).map(|_| lib.next_grant_id().unwrap()).collect();
        assert_eq!(ids, [1, 2, 3]);
        // Id 2 went to a grant that is not stored (a session's): never handed out again.
        lib.add_grant(grant(1, "claude")).unwrap();
        lib.add_grant(grant(3, "codex")).unwrap();
        lib.add_grant(GrantRecord {
            session: None,
            expires_at: None,
            ..grant(lib.next_grant_id().unwrap(), "claude")
        })
        .unwrap();
        let stored: Vec<(i64, String)> = lib
            .grants()
            .unwrap()
            .into_iter()
            .map(|g| (g.id, g.agent))
            .collect();
        assert_eq!(
            stored,
            [
                (1, "claude".into()),
                (3, "codex".into()),
                (4, "claude".into())
            ]
        );
        assert_eq!(lib.grants().unwrap()[2].expires_at, None);

        // Removing an agent removes its grants.
        assert!(lib.remove_agent("claude").unwrap());
        assert!(!lib.remove_agent("claude").unwrap());
        assert_eq!(lib.grants().unwrap(), [grant(3, "codex")]);
        assert_eq!(lib.remove_grants(&[3, 3, 42]).unwrap(), 1);
        assert!(lib.grants().unwrap().is_empty());
        // The counter keeps going.
        assert_eq!(lib.next_grant_id().unwrap(), 5);
    }
}

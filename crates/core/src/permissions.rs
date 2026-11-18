//! Agent permissions (docs/design/agent-permissions.md):
//! registered agents and their tokens, the user's grants, and how they decide an agent's
//! command: a `forbid` refuses any command, an `allow` runs one that would otherwise wait
//! for the user's confirmation.
//!
//! Core keeps every agent and grant in memory, shared by the actor (which changes them)
//! and the handle (queries, [`crate::CoreHandle::authenticate_agent`]). Agents and lasting
//! grants (until a time, or forever) are also stored by the catalog; grants for a session
//! are not, and end with the session or core. When the catalog cannot read them at
//! startup, agents are refused everything (`permissions_unavailable`).
//!
//! Tokens: 256 random bits from the operating system's CSPRNG, written as
//! `pmpa_<64 hex digits>`. Core stores only their SHA-256 and compares hashes in constant
//! time.

pub(crate) mod checks;
pub(crate) mod fs_deadline;

mod scope;
pub(crate) use scope::{Reach, Target, common_scope, normalize_prefix, scope_reach};
mod matching;
pub(crate) use matching::decide;
use matching::live;
pub use matching::{grantable, takes_scope};
mod identity;
use identity::same_hash;
pub(crate) use identity::{check_agent_name, check_grant_agent, new_token, token_hash};

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, Ordering},
};

use sha2::{Digest, Sha256};

use crate::contract::{
    AgentInfo, COMMANDS, ContractError, Covers, Effect, ErrorCode, Expiry, Grant, GrantId,
    ObjectRef, Operation, OperationClass, Origin, Policy, QueryResult, RootId, Scope, next_offset,
};

/// What a containment check answers when it cannot tell (whether letter case counts in a
/// folder, a location the platform layer cannot read): an `allow` grant must surely contain
/// what it allows (`Exclude`), a `forbid` grant applies to whatever it may contain
/// (`Include`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Doubt {
    Exclude,
    Include,
}

/// The platform layer's rules for location strings (contract guarantee 16), given to core
/// when it starts ([`crate::CoreConfig::locations`]). Core never interprets a location
/// itself; it asks these, for the scopes of permission grants and for the locations agents
/// name.
pub trait Locations: Send + Sync + 'static {
    /// The location in the platform's normal form: what the platform would act on (desktop:
    /// a canonical path, symlinks and `..` resolved as the system resolves them). Core
    /// normalizes the location an agent's command names, then checks grants, asks the user
    /// and runs the command with the normal form; it normalizes it again when the user
    /// approves, and refuses the command if the result differs (`location_changed`).
    /// `None`: not a location this layer can read (relative, `..` past what exists, a
    /// symlink to nothing …).
    fn normalize(&self, location: &str) -> Option<String>;
    /// Whether `location` is `scope` or inside it (a file in a folder, a subfolder); both in
    /// normal form. `doubt` decides what cannot be told.
    fn contains(&self, scope: &str, location: &str, doubt: Doubt) -> bool;
    /// `relative` (a library prefix: `/`-separated, checked) as a location inside
    /// `location`; `None` when it cannot be one.
    fn child(&self, location: &str, relative: &str) -> Option<String>;
    /// Whether letter case is ignored for names at `location` (and inside it, for a
    /// folder); `None` when that cannot be told.
    fn ignores_case(&self, location: &str) -> Option<bool>;
    /// A location as the user should read it (in a confirmation dialog).
    fn display(&self, location: &str) -> String;
    /// Whether `location` can name a place at all, told from its text alone, without the
    /// file system (absolute, not empty, no `..` part …). Core refuses an agent's
    /// location that cannot (`invalid_location`) before it looks at any grant: the answer
    /// depends on nothing else, so it tells nothing of them. The default takes every one
    /// (then `normalize` alone decides).
    fn well_formed(&self, _location: &str) -> bool {
        true
    }
    /// For a well-formed location `normalize` cannot read (a link to nothing, a drive that
    /// is not there): where it would be by its name, as far as can be told (the normal form
    /// of its longest part that resolves, then the rest as written). Core judges `forbid`
    /// grants by it (it refuses the command anyway: nothing is acted on there). `None`:
    /// that cannot be told either; then every `forbid` with a location may contain it. The
    /// default is `None`.
    fn nearest(&self, _location: &str) -> Option<String> {
        None
    }
}

/// A registered agent as core and the catalog keep it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAgent {
    pub name: String,
    /// SHA-256 of its token; `None` when the stored hash cannot be read (no token matches;
    /// the agent is listed and can be given a new token or removed).
    pub token_hash: Option<[u8; 32]>,
    pub created_at_ms: i64,
    pub last_seen_at_ms: Option<i64>,
}

/// What the catalog stores of the permissions: agents, and grants that outlive core.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredPermissions {
    pub agents: Vec<StoredAgent>,
    pub grants: Vec<Grant>,
}

/// Longest agent name, in characters (also the name part of `<platform>:<name>`).
pub const MAX_AGENT_NAME: usize = 64;

/// Longest platform part of an adapter's own agent name (`<platform>:<name>`).
pub const MAX_PLATFORM: usize = 32;

/// Longest grant `session`, location and library prefix, in characters.
pub const MAX_SESSION: usize = 128;

pub const MAX_LOCATION: usize = 4096;

pub const MAX_PREFIX: usize = 1024;

/// An agent's last sighting is written to the catalog at most this often.
const SEEN_WRITE_EVERY_MS: u64 = 60_000;

/// What the user's grants say about a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Grants forbid it: every one that does, by id, with how much of it each covers.
    Forbid(Vec<(GrantId, Covers)>),
    /// A grant allows it (and none forbids it).
    Allow(GrantId),
    /// No grant applies: ask the user (or run it, if that is the default).
    Ask,
}

/// An agent's command, as its candidate grants are matched against it.
pub(crate) struct Request<'a> {
    pub command: &'a str,
    pub class: OperationClass,
    /// What it acts on; empty: nothing in particular.
    pub targets: &'a [Target],
}

/// Every agent and grant core knows.
#[derive(Debug, Default)]
pub(crate) struct Permissions {
    /// `false`: they could not be read when core started; agents are then refused
    /// everything (and no token is recognised) until core restarts.
    available: bool,
    /// By name.
    agents: Vec<StoredAgent>,
    registrations: HashMap<String, u64>,
    /// By id. Expired ones are no longer in effect, and are removed by the actor's tick
    /// ([`Permissions::take_expired`]).
    grants: Vec<Grant>,
    /// When each agent's last sighting was last written to the catalog.
    seen_written: HashMap<String, i64>,
    /// Sessions that have ended (`end_agent_session`), in memory only: a grant for one
    /// could never apply (`session_ended`).
    ended: EndedSessions,
    sessions: HashMap<(String, String), Weak<AtomicBool>>,
}

/// The most ended sessions core remembers; the oldest are forgotten first.
pub(crate) const MAX_ENDED_SESSIONS: usize = 10_000;

/// The agents' sessions that have ended, oldest first, at most [`MAX_ENDED_SESSIONS`].
///
/// Live requests and confirmation tickets pin a separate irreversible flag, so FIFO
/// eviction cannot restore their execution eligibility. Session ids are never reused.
#[derive(Debug, Default)]
struct EndedSessions {
    order: VecDeque<(String, String)>,
    known: HashSet<(String, String)>,
}

impl EndedSessions {
    fn add(&mut self, agent: &str, session: &str) {
        let key = (agent.to_owned(), session.to_owned());
        if !self.known.insert(key.clone()) {
            return;
        }
        self.order.push_back(key);
        while self.order.len() > MAX_ENDED_SESSIONS {
            if let Some(oldest) = self.order.pop_front() {
                self.known.remove(&oldest);
            }
        }
    }

    fn contains(&self, agent: &str, session: &str) -> bool {
        self.known.contains(&(agent.to_owned(), session.to_owned()))
    }
}

/// Registration identities are unique across all core instances in this process.
fn next_registration() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let mut current = NEXT.load(Ordering::Relaxed);
    loop {
        let next = current
            .checked_add(1)
            .expect("agent registration identities exhausted");
        match NEXT.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return current,
            Err(actual) => current = actual,
        }
    }
}

impl Permissions {
    /// The stored permissions; grants already expired at `now` are returned apart, to be
    /// deleted.
    pub fn new(stored: StoredPermissions, now: i64) -> (Self, Vec<Grant>) {
        let mut permissions = Self {
            available: true,
            seen_written: stored
                .agents
                .iter()
                .filter_map(|a| Some((a.name.clone(), a.last_seen_at_ms?)))
                .collect(),
            registrations: stored
                .agents
                .iter()
                .map(|a| (a.name.clone(), next_registration()))
                .collect(),
            agents: stored.agents,
            grants: stored.grants,
            ended: EndedSessions::default(),
            sessions: HashMap::new(),
        };
        permissions.agents.sort_by(|a, b| a.name.cmp(&b.name));
        permissions.grants.sort_by_key(|g| g.id);
        let expired = permissions.take_expired(now);
        (permissions, expired)
    }

    /// Permissions that could not be read: nothing is known, and agents get nothing.
    pub fn unavailable() -> Self {
        Self::default()
    }

    pub fn available(&self) -> bool {
        self.available
    }

    pub fn agent(&self, name: &str) -> Option<&StoredAgent> {
        self.agents.iter().find(|a| a.name == name)
    }

    pub fn registration(&self, name: &str) -> Option<u64> {
        self.registrations.get(name).copied()
    }

    pub fn add_agent(&mut self, agent: StoredAgent) {
        self.registrations
            .insert(agent.name.clone(), next_registration());
        let at = self.agents.partition_point(|a| a.name < agent.name);
        self.agents.insert(at, agent);
    }

    pub fn set_token(&mut self, name: &str, token_hash: [u8; 32]) {
        if let Some(agent) = self.agents.iter_mut().find(|a| a.name == name) {
            agent.token_hash = Some(token_hash);
        }
    }

    /// Removes an agent and all its grants: the grants removed.
    pub fn remove_agent(&mut self, name: &str) -> Vec<Grant> {
        self.agents.retain(|a| a.name != name);
        self.registrations.remove(name);
        self.seen_written.remove(name);
        let (removed, kept) = std::mem::take(&mut self.grants)
            .into_iter()
            .partition(|g| g.agent == name);
        self.grants = kept;
        removed
    }

    /// The agent whose token this is, now seen at `now`, and whether that sighting should
    /// be written to the catalog (at most once a minute per agent). `None` also while the
    /// permissions are unavailable.
    pub fn authenticate(&mut self, token: &str, now: i64) -> Option<(String, bool)> {
        if !self.available {
            return None;
        }
        let hash = token_hash(token);
        // Every agent is compared, each in constant time.
        let mut found = None;
        for (i, agent) in self.agents.iter().enumerate() {
            let stored = agent.token_hash.unwrap_or([0; 32]);
            if same_hash(&stored, &hash) && agent.token_hash.is_some() {
                found = Some(i);
            }
        }
        let agent = &mut self.agents[found?];
        agent.last_seen_at_ms = Some(now);
        let write = self
            .seen_written
            .get(&agent.name)
            .is_none_or(|&at| at.abs_diff(now) >= SEEN_WRITE_EVERY_MS);
        if write {
            self.seen_written.insert(agent.name.clone(), now);
        }
        Some((agent.name.clone(), write))
    }

    pub fn agents(&self) -> Vec<AgentInfo> {
        self.agents
            .iter()
            .map(|a| AgentInfo {
                name: a.name.clone(),
                created_at_ms: a.created_at_ms,
                last_seen_at_ms: a.last_seen_at_ms,
            })
            .collect()
    }

    pub fn add_grant(&mut self, grant: Grant) {
        let at = self.grants.partition_point(|g| g.id < grant.id);
        self.grants.insert(at, grant);
    }

    /// A grant in effect at `now`.
    pub fn grant(&self, id: GrantId, now: i64) -> Option<&Grant> {
        self.grants.iter().find(|g| g.id == id && live(g, now))
    }

    pub fn remove_grant(&mut self, id: GrantId) -> Option<Grant> {
        let at = self.grants.iter().position(|g| g.id == id)?;
        Some(self.grants.remove(at))
    }

    /// Removes the grants expired at `now`: those removed.
    pub fn take_expired(&mut self, now: i64) -> Vec<Grant> {
        if self.grants.iter().all(|g| live(g, now)) {
            return Vec::new();
        }
        let (kept, expired) = std::mem::take(&mut self.grants)
            .into_iter()
            .partition(|g| live(g, now));
        self.grants = kept;
        expired
    }

    /// Removes the grants for one session of an agent that last as long as the session:
    /// those removed.
    /// The session is remembered as ended ([`Permissions::session_ended`]).
    pub fn mark_session_ended(&mut self, agent: &str, session: &str) {
        if let Some(state) = self
            .sessions
            .get(&(agent.to_owned(), session.to_owned()))
            .and_then(Weak::upgrade)
        {
            state.store(true, Ordering::Release);
        }
        self.ended.add(agent, session);
    }

    pub fn end_session(&mut self, agent: &str, session: &str) -> Vec<Grant> {
        self.mark_session_ended(agent, session);
        let (removed, kept) = std::mem::take(&mut self.grants).into_iter().partition(|g| {
            g.agent == agent && g.session.as_deref() == Some(session) && g.until == Expiry::Session
        });
        self.grants = kept;
        removed
    }

    /// Whether this session ended: a pinned request's flag, or the bounded history
    /// ([`MAX_ENDED_SESSIONS`]) after its last request and ticket have gone.
    pub fn session_ended(&self, agent: &str, session: &str) -> bool {
        self.ended.contains(agent, session)
            || self
                .sessions
                .get(&(agent.to_owned(), session.to_owned()))
                .and_then(Weak::upgrade)
                .is_some_and(|state| state.load(Ordering::Acquire))
    }

    /// Pins a session while a request or confirmation can still start executing.
    pub fn pin_session(&mut self, origin: &Origin) -> Option<Arc<AtomicBool>> {
        let Origin::Agent {
            name,
            session: Some(session),
        } = origin
        else {
            return None;
        };
        self.sessions.retain(|_, state| state.strong_count() > 0);
        let key = (name.clone(), session.clone());
        if let Some(state) = self.sessions.get(&key).and_then(Weak::upgrade) {
            return Some(state);
        }
        let state = Arc::new(AtomicBool::new(self.ended.contains(name, session)));
        self.sessions.insert(key, Arc::downgrade(&state));
        Some(state)
    }

    /// A page of the grants in effect at `now` (one agent's, or all), oldest first.
    pub fn page(&self, agent: Option<&str>, offset: u32, limit: u32, now: i64) -> QueryResult {
        let matching: Vec<&Grant> = self
            .grants
            .iter()
            .filter(|g| live(g, now) && agent.is_none_or(|a| g.agent == a))
            .collect();
        let total = u32::try_from(matching.len()).unwrap_or(u32::MAX);
        let grants: Vec<Grant> = matching
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .cloned()
            .collect();
        QueryResult::Permissions {
            next_offset: next_offset(offset, grants.len(), total),
            grants,
            offset,
            total,
        }
    }

    /// The grants in effect at `now` for this agent's session that name one of these
    /// commands (the command, and what it amounts to: undoing a folder change is adding or
    /// removing that folder) or this class: the candidates [`decide`] looks at (scopes are
    /// left to it, outside the lock: they may need the catalog and the file system).
    pub fn candidates(
        &self,
        agent: &str,
        session: Option<&str>,
        commands: &[&str],
        class: OperationClass,
        now: i64,
    ) -> Vec<Grant> {
        self.grants
            .iter()
            .filter(|g| {
                g.agent == agent
                    && g.session.as_deref().is_none_or(|s| session == Some(s))
                    && match &g.operation {
                        Operation::Command { command: c } => commands.contains(&c.as_str()),
                        Operation::Class { class: c } => *c == class,
                        _ => false,
                    }
                    && live(g, now)
            })
            .cloned()
            .collect()
    }
}

pub(crate) fn agent_not_found(name: &str, field: &str) -> ContractError {
    ContractError::not_found(ErrorCode::AgentNotFound, format!("no agent named {name:?}"))
        .with_missing(vec![ObjectRef::Agent {
            name: name.to_owned(),
        }])
        .with_field(field)
}

pub(crate) fn grant_not_found(id: GrantId, field: &str) -> ContractError {
    ContractError::not_found(ErrorCode::GrantNotFound, format!("no grant {}", id.0))
        .with_missing(vec![ObjectRef::Grant { id }])
        .with_field(field)
}

#[cfg(test)]
mod tests;

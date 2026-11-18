//! The adapter: accepts connections at the endpoint, knows each by its agent's token, runs
//! an MCP service per connection, and keeps the obligations core leaves to adapters
//! (docs/design/agent-permissions.md): a random session per connection, ended when the
//! connection ends; every connection checked again when the agents change.

mod connection;
use connection::{Active, connection};

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use mpmp_core::contract::{Event, Origin};
use mpmp_core::{CoreHandle, SubscribeOptions, Subscription};
use rmcp::ServiceExt;
use rmcp::transport::async_rw::AsyncRwTransport;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::watch;

use crate::confirm::{Confirmations, Owner};
use crate::endpoint::Endpoint;
use crate::handshake::{self, Refusal};
use crate::listener::{self, ServerStream};
use crate::service::{Gate, Session};
use crate::tools::MAX_WAIT;

/// The longest MCP message the adapter reads (one line of JSON).
const MAX_MESSAGE: usize = 8 * 1024 * 1024;

/// Connections beyond the most served at once that still get an answer (`busy`); more are
/// closed at once, without one.
const REFUSED_AT_ONCE: usize = 16;

/// The most a refused peer may still send while the adapter waits for it to hang up.
const REFUSED_READ: u64 = 4 * handshake::MAX_LINE as u64;

/// How long `shutdown` waits for sessions cut short to end once their commands are done.
const ENDING_WAIT: Duration = Duration::from_secs(10);

/// How the adapter serves agents.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ServerOptions {
    /// Where to listen; `None`: [`Endpoint::from_env`] (the user's default endpoint unless
    /// `PLAYER_MCP_ENDPOINT` names another).
    pub endpoint: Option<Endpoint>,
    /// Most connections at once (also those still handshaking); more are refused (`busy`).
    pub max_connections: usize,
    /// The longest a `wait_for_confirmation` call waits (at most 50 s; tests use less). A call
    /// without `timeout_s` waits [`DEFAULT_WAIT`](crate::DEFAULT_WAIT) (25 s), or this if shorter.
    pub max_wait: Duration,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            endpoint: None,
            max_connections: 16,
            max_wait: MAX_WAIT,
        }
    }
}

impl ServerOptions {
    pub fn with_endpoint(mut self, endpoint: Endpoint) -> Self {
        self.endpoint = Some(endpoint);
        self
    }

    pub fn with_max_connections(mut self, max: usize) -> Self {
        self.max_connections = max;
        self
    }

    pub fn with_max_wait(mut self, max_wait: Duration) -> Self {
        self.max_wait = max_wait.min(MAX_WAIT);
        self
    }
}

/// The MCP service for agents (`mpmp_agent_mcp::Server::start`).
pub struct Server;

impl Server {
    /// Starts serving agents at the endpoint, on threads of its own (a small tokio runtime;
    /// calls into core run on its blocking threads). Fails when the endpoint cannot be
    /// listened on (another player serves there, a folder that is not the user's …) or core
    /// has stopped. The app decides whether to start it.
    pub fn start(core: CoreHandle, options: ServerOptions) -> io::Result<ServerHandle> {
        let endpoint = match options.endpoint.clone() {
            Some(endpoint) => endpoint,
            None => Endpoint::from_env()?,
        };
        // Before any connection: no answer to a confirmation is missed.
        let subscription = core
            .subscribe(
                // Not the UI: the adapter must not see other origins' confirmation requests.
                Origin::System,
                SubscribeOptions::default().with_positions(false),
            )
            .ok_or_else(|| io::Error::other("core has stopped"))?;
        let (shutdown, _) = watch::channel(false);
        let shared = Arc::new(Shared {
            core,
            options,
            confirmations: Arc::new(Confirmations::new()),
            connections: Mutex::new(HashMap::new()),
            next_connection: AtomicU64::new(1),
            active: AtomicUsize::new(0),
            shutdown,
            ending: Mutex::new(Vec::new()),
        });
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = {
            let shared = Arc::clone(&shared);
            let endpoint = endpoint.clone();
            std::thread::Builder::new()
                .name("agent-mcp".into())
                .spawn(move || run(shared, endpoint, ready_tx))?
        };
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                let _ = thread.join();
                return Err(err);
            }
            Err(_) => {
                let _ = thread.join();
                return Err(io::Error::other("the agent service thread stopped"));
            }
        }
        let events = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("agent-mcp-events".into())
                .spawn(move || pump(&shared, subscription))
        };
        let events = match events {
            Ok(events) => events,
            Err(err) => {
                stop_thread(&shared, Some(thread));
                return Err(err);
            }
        };
        tracing::info!("serving agents at {endpoint}");
        Ok(ServerHandle {
            endpoint,
            shared,
            thread: Some(thread),
            events: Some(events),
        })
    }
}

/// The running service; dropping it shuts it down too.
pub struct ServerHandle {
    endpoint: Endpoint,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    events: Option<JoinHandle<()>>,
}

impl ServerHandle {
    /// Where the service listens (for the UI and the setup instructions).
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Seals admission immediately. Call `shutdown` to wait for session cleanup.
    pub fn request_shutdown(&self) {
        self.shared.request_shutdown();
    }

    /// Stops accepting connections, closes every connection (ending its session in core),
    /// removes the Unix socket, and returns when all that is done.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        stop_thread(&self.shared, self.thread.take());
        if let Some(events) = self.events.take() {
            let _ = events.join();
        }
        // Sessions cut short end once the commands they had in core are done
        // (`SessionGuard::drop`); give them a while.
        let ending = std::mem::take(&mut *self.shared.ending());
        let deadline = std::time::Instant::now() + ENDING_WAIT;
        while ending.iter().any(|t| !t.is_finished()) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        for thread in ending {
            if thread.is_finished() {
                let _ = thread.join();
            } else {
                tracing::warn!(
                    "an agent's session has not ended yet: its command is still in core"
                );
            }
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn stop_thread(shared: &Shared, thread: Option<JoinHandle<()>>) {
    shared.request_shutdown();
    for connection in shared.connections().values() {
        shared
            .core
            .invalidate_agent_session(connection.agent.name(), &connection.session);
    }
    if let Some(thread) = thread {
        let _ = thread.join();
    }
}

struct Shared {
    core: CoreHandle,
    options: ServerOptions,
    confirmations: Arc<Confirmations>,
    connections: Mutex<HashMap<u64, Connection>>,
    next_connection: AtomicU64,
    /// Connections being served or handshaking.
    active: AtomicUsize,
    shutdown: watch::Sender<bool>,
    /// Threads that end a session cut short once its command in core is done.
    ending: Mutex<Vec<JoinHandle<()>>>,
}

/// A connection that got in: who it is, and the token it showed (kept in memory only, to
/// check it again when the agents change; one copy, shared).
struct Connection {
    token: Arc<str>,
    agent: mpmp_core::AgentHandle,
    session: String,
    close: watch::Sender<bool>,
}

impl Shared {
    fn connections(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Connection>> {
        self.connections.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn ending(&self) -> std::sync::MutexGuard<'_, Vec<JoinHandle<()>>> {
        self.ending.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn stopping(&self) -> bool {
        *self.shutdown.borrow()
    }

    fn request_shutdown(&self) {
        self.shutdown.send_replace(true);
        for connection in self.connections().values() {
            connection.close.send_replace(true);
        }
    }

    /// What the event thread does with an event: answers to confirmations, and checking
    /// connections again when the agents change (or events were lost, which may have been
    /// such a change).
    fn on_event(&self, event: Event) {
        match event {
            Event::ConfirmationResolved { outcome } => self.confirmations.answered(outcome),
            Event::AgentsChanged => self.recheck(),
            Event::Resync { .. } => {
                self.confirmations.lost_events();
                self.recheck();
            }
            _ => {}
        }
    }

    /// Checks every connection's token again; closes those that no longer name their agent
    /// (token renewed, agent removed, permissions unreadable).
    fn recheck(&self) {
        let all: Vec<(u64, Arc<str>, mpmp_core::AgentHandle)> = self
            .connections()
            .iter()
            .map(|(id, c)| (*id, Arc::clone(&c.token), c.agent.clone()))
            .collect();
        for (id, token, agent) in all {
            if !self
                .core
                .authenticate_agent_connection(&token)
                .is_some_and(|now| now.same_registration(&agent))
            {
                tracing::info!(
                    "closing a connection of agent {}: its token no longer works",
                    agent.name()
                );
                if let Some(connection) = self.connections().get(&id) {
                    self.core
                        .invalidate_agent_session(connection.agent.name(), &connection.session);
                    connection.close.send_replace(true);
                }
            }
        }
    }
}

/// The service thread: binds, reports, then accepts until shutdown.
fn run(shared: Arc<Shared>, endpoint: Endpoint, ready: std::sync::mpsc::Sender<io::Result<()>>) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(32)
        .thread_name("agent-mcp-worker")
        .enable_all()
        .build();
    let runtime = match runtime {
        Ok(runtime) => runtime,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };
    let bound = {
        let _entered = runtime.enter();
        listener::bind(&endpoint)
    };
    let listener = match bound {
        Ok(listener) => listener,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    runtime.block_on(accept_loop(Arc::clone(&shared), listener));
    runtime.shutdown_timeout(Duration::from_secs(2));
}

async fn accept_loop(shared: Arc<Shared>, mut listener: listener::Listener) {
    let mut shutdown = shared.shutdown.subscribe();
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|s| *s) => break,
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            accepted = listener.accept() => {
                let accepted = match accepted {
                    Ok(accepted) => accepted,
                    Err(err) => {
                        tracing::warn!("cannot accept an agent connection: {err}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                // Windows: known only once the client has written (`connection`).
                match accepted.same_user {
                    Some(Ok(true)) | None => {}
                    Some(Ok(false)) => {
                        tracing::warn!("refused a connection from another user");
                        continue;
                    }
                    Some(Err(err)) => {
                        tracing::warn!("refused a connection whose user is unknown: {err}");
                        continue;
                    }
                }
                let active = Active::enter(&shared);
                if active.count > shared.options.max_connections.saturating_add(REFUSED_AT_ONCE) {
                    // A flood: not even an answer (each would keep a task a while).
                    continue;
                }
                tasks.spawn(connection(Arc::clone(&shared), accepted.stream, active));
            }
        }
    }
    // No new connections (on Unix the socket file goes too).
    drop(listener);
    for connection in shared.connections().values() {
        connection.close.send_replace(true);
    }
    // Each ends its session; a refusal still waiting for its peer to hang up is cut short
    // (sessions cut short end in `SessionGuard::drop`).
    let drained = async { while tasks.join_next().await.is_some() {} };
    if tokio::time::timeout(Duration::from_secs(3), drained)
        .await
        .is_err()
    {
        tasks.shutdown().await;
    }
}

/// The event thread ([`Shared::on_event`]).
fn pump(shared: &Shared, subscription: Subscription) {
    let events = subscription.events;
    while !shared.stopping() {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(envelope) => shared.on_event(envelope.event),
            Err(err) if err.is_timeout() => continue,
            Err(_) => break,
        }
    }
}

/// Ends a connection's session however the connection ends: forgets it, and tells core
/// (`end_agent_session`: the session's grants go, what it waits for is withdrawn).
struct SessionGuard {
    shared: Arc<Shared>,
    id: u64,
    owner: Owner,
    /// Waited on after core is told the session ended, before tokens are forgotten:
    /// even a call already past the gate is withdrawn by core if not executing yet,
    /// and no late claim can leave a token after cleanup.
    gate: Arc<Gate>,
    ended: bool,
}

impl SessionGuard {
    async fn withdraw(&self) {
        self.close();
        let core = self.shared.core.clone();
        let Owner { agent, session } = self.owner.clone();
        let _ = tokio::task::spawn_blocking(move || core.end_agent_session(&agent, &session)).await;
    }

    async fn end(&mut self) {
        self.withdraw().await;
        *self.gate.write().await = false;
        self.shared.confirmations.forget(&self.owner);
        self.ended = true;
    }

    /// No longer a connection: no new checks, and its waits still running stop.
    fn close(&self) {
        self.shared
            .core
            .invalidate_agent_session(&self.owner.agent, &self.owner.session);
        if let Some(connection) = self.shared.connections().remove(&self.id) {
            connection.close.send_replace(true);
        }
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        // The task was cut short before `end` was done: `shutdown` gave up waiting for the
        // connections (3 s) and aborted them, or it panicked. End the session all the same,
        // before waiting for commands still past the gate. They cannot start executing
        // after core sees the session end. A separate thread waits for late token claims,
        // then forgets them; `shutdown` waits for that thread (a while).
        if self.ended {
            return;
        }
        self.close();
        let shared = Arc::clone(&self.shared);
        let owner = self.owner.clone();
        let gate = Arc::clone(&self.gate);
        let waiter = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("agent-mcp-end".into())
            .spawn(move || {
                waiter.core.end_agent_session(&owner.agent, &owner.session);
                *gate.blocking_write() = false;
                waiter.confirmations.forget(&owner);
            });
        match spawned {
            Ok(thread) => shared.ending().push(thread),
            Err(err) => {
                tracing::warn!("cannot spawn session cleanup: {err}");
                shared
                    .core
                    .end_agent_session(&self.owner.agent, &self.owner.session);
                shared.confirmations.forget(&self.owner);
            }
        }
    }
}

/// A random UUID (version 4): sessions are unique and never reused.
fn new_session_id() -> Option<String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).ok()?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

#[cfg(test)]
mod tests;

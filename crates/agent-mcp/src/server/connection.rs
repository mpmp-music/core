use super::*;

/// Counts a connection while it lives.
pub(super) struct Active {
    shared: Arc<Shared>,
    pub(super) count: usize,
}

impl Active {
    pub(super) fn enter(shared: &Arc<Shared>) -> Self {
        let count = shared.active.fetch_add(1, Ordering::SeqCst) + 1;
        Self {
            shared: Arc::clone(shared),
            count,
        }
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        self.shared.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Answers `ERR` and closes, after the peer has read it (or a short while).
async fn refuse_split<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    read: R,
    mut write: W,
    refusal: Refusal,
) {
    let _ = write
        .write_all(handshake::err_line(refusal).as_bytes())
        .await;
    let _ = write.flush().await;
    let _ = write.shutdown().await;
    // Closing at once could drop the answer before it is read (named pipes; on Unix,
    // closing with unread input resets the connection): wait for the peer to hang up
    // first, a little, reading (and dropping) at most a few lines meanwhile.
    let mut rest = read.take(REFUSED_READ);
    let _ = tokio::time::timeout(
        handshake::TIMEOUT,
        tokio::io::copy(&mut rest, &mut tokio::io::sink()),
    )
    .await;
}

/// One connection: handshake, then the agent's MCP session until either side closes it.
pub(super) async fn connection(shared: Arc<Shared>, stream: ServerStream, active: Active) {
    let late = listener::Late::of(&stream);
    let (read, mut write) = tokio::io::split(stream);
    let mut reader = BufReader::new(read);
    let mut shutdown = shared.shutdown.subscribe();
    let line = tokio::select! {
        line = tokio::time::timeout(handshake::TIMEOUT, handshake::read_line(&mut reader)) => line,
        _ = shutdown.wait_for(|s| *s) => return,
    };
    let line = match line {
        Ok(Ok(line)) => line,
        // Silent or broken: just close.
        Ok(Err(_)) | Err(_) => return,
    };
    // Windows identifies a pipe client only once something was read from it: now. Nothing
    // is answered before (Unix checked when it accepted the connection).
    match late.same_user() {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!("refused a connection from another user");
            return;
        }
        Err(err) => {
            tracing::warn!("refused a connection whose user is unknown: {err}");
            return;
        }
    }
    let Some(line) = line else {
        return refuse_split(reader, write, Refusal::BadHandshake).await;
    };
    // The one copy of the token kept (`Connection`); the line it came in is wiped.
    let parsed = handshake::parse_hello(&line).map(Arc::<str>::from);
    handshake::wipe(line);
    let token = match parsed {
        Ok(token) => token,
        Err(refusal) => return refuse_split(reader, write, refusal).await,
    };
    if active.count > shared.options.max_connections {
        return refuse_split(reader, write, Refusal::Busy).await;
    }
    if shared.stopping() {
        return refuse_split(reader, write, Refusal::ShuttingDown).await;
    }
    let Some(agent) = authenticate(&shared, &token).await else {
        return refuse_split(reader, write, Refusal::InvalidToken).await;
    };
    let Some(session) = new_session_id() else {
        tracing::error!("no random numbers for a session id: closing a connection");
        return;
    };
    let owner = Owner {
        agent: agent.name().to_owned(),
        session,
    };
    let (close, closed) = watch::channel(false);
    let id = shared.next_connection.fetch_add(1, Ordering::Relaxed);
    {
        let mut connections = shared.connections();
        if shared.stopping() {
            return;
        }
        connections.insert(
            id,
            Connection {
                token: Arc::clone(&token),
                agent: agent.clone(),
                session: owner.session.clone(),
                close,
            },
        );
    }
    let gate = Arc::new(Gate::new(true));
    let mut guard = SessionGuard {
        shared: Arc::clone(&shared),
        id,
        owner: owner.clone(),
        gate: Arc::clone(&gate),
        ended: false,
    };
    // From now on an `AgentsChanged` closes it; one that came just before is caught here.
    let again = authenticate(&shared, &token).await;
    drop(token);
    if !again.is_some_and(|now| now.same_registration(&agent)) {
        refuse_split(reader, write, Refusal::InvalidToken).await;
        return guard.end().await;
    }
    let hello = handshake::ok_line(&owner.agent);
    if write.write_all(hello.as_bytes()).await.is_err() || write.flush().await.is_err() {
        return guard.end().await;
    }
    tracing::info!(
        "agent {} connected (session {})",
        owner.agent,
        owner.session
    );
    let session = Session::new(
        shared.core.clone(),
        agent,
        owner.clone(),
        Arc::clone(&shared.confirmations),
        shared.options.max_wait,
        closed.clone(),
        gate,
    );
    let transport = AsyncRwTransport::new_server(reader, write).with_max_line_length(MAX_MESSAGE);
    let closing = {
        let mut closed = closed.clone();
        async move {
            tokio::select! {
                _ = closed.wait_for(|c| *c) => {}
                _ = shutdown.wait_for(|s| *s) => {}
            }
        }
    };
    tokio::pin!(closing);
    let running = tokio::select! {
        running = session.serve(transport) => running,
        () = &mut closing => return guard.end().await,
    };
    match running {
        Ok(running) => {
            let cancel = running.cancellation_token();
            let waiting = running.waiting();
            tokio::pin!(waiting);
            tokio::select! {
                _ = &mut waiting => {}
                () = &mut closing => {
                    guard.withdraw().await;
                    cancel.cancel();
                    let _ = tokio::time::timeout(Duration::from_secs(2), waiting).await;
                }
            }
        }
        Err(err) => tracing::debug!("agent {}'s MCP session did not start: {err}", owner.agent),
    }
    tracing::info!(
        "agent {} disconnected (session {})",
        owner.agent,
        owner.session
    );
    guard.end().await;
}

async fn authenticate(shared: &Arc<Shared>, token: &Arc<str>) -> Option<mpmp_core::AgentHandle> {
    let shared = Arc::clone(shared);
    let token = Arc::clone(token);
    tokio::task::spawn_blocking(move || {
        if shared.stopping() {
            return None;
        }
        shared.core.authenticate_agent_connection(&token)
    })
    .await
    .ok()
    .flatten()
}

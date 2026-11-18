use super::*;

/// Copies bytes both ways until either side is done. After the framework closes stdin
/// (it is done), the player's answers to what it asked still go through: the bridge exits
/// once every request it saw was answered (or cancelled) and the player has then been quiet
/// for [`QUIET`] (or closed the connection), at most [`ANSWERS`] after stdin closed. On
/// Unix the player is told at once (half close): it answers what it was asked, then closes;
/// a named pipe cannot be half closed, so on Windows the player learns it when the bridge
/// exits, and the bridge itself must notice the last answer.
pub(super) async fn forward(mut reader: Reader, mut writer: Writer) -> ExitCode {
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let pending = RefCell::new(Pending::default());
    // When the player last sent something.
    let heard = Cell::new(Instant::now());
    // The player sent something: the wait after stdin closed is worked out again.
    let activity = Notify::new();
    let upstream = async {
        // A request is known before the player can answer it.
        let copied = pump(&mut stdin, &mut writer, Note::BeforeWriting, |bytes| {
            pending.borrow_mut().sent(bytes);
        })
        .await;
        #[cfg(unix)]
        let _ = writer.shutdown().await;
        copied
    };
    // An answer counts once it was passed on: the bridge never exits before that.
    let downstream = pump(&mut reader, &mut stdout, Note::AfterWriting, |bytes| {
        pending.borrow_mut().answered(bytes);
        heard.set(Instant::now());
        activity.notify_one();
    });
    tokio::pin!(upstream, downstream);
    tokio::select! {
            copied = &mut upstream => {
                if let Err(err) = copied {
                    eprintln!("player-mcp: reading stdin failed: {err}");
                }
            }
            copied = &mut downstream => {
                match copied {
                    Ok(()) => eprintln!(concat!(
    "player-mcp: mpmp closed the connection (it stopped, or this agent's token ",
    "was renewed or removed)",
    )),
                    Err(err) => eprintln!("player-mcp: the connection to mpmp failed: {err}"),
                }
                return ExitCode::FAILURE;
            }
        }
    // The framework is done: let the player answer what it was asked.
    let closed = Instant::now();
    heard.set(heard.get().max(closed));
    loop {
        // Worked out again whenever the player sends something: the last answer turns the
        // wait for answers into the short wait for quiet.
        let until = if pending.borrow().is_empty() {
            heard.get() + QUIET
        } else {
            closed + ANSWERS
        };
        let until = until.min(closed + ANSWERS);
        tokio::select! {
            // The player closed (or failed) after the framework was done: nothing more to
            // pass on.
            _ = &mut downstream => return ExitCode::SUCCESS,
            () = activity.notified() => {}
            () = tokio::time::sleep_until(until) => {
                let quiet = pending.borrow().is_empty() && Instant::now() >= heard.get() + QUIET;
                if quiet || Instant::now() >= closed + ANSWERS {
                    return ExitCode::SUCCESS;
                }
            }
        }
    }
}

/// When [`pump`] hands a chunk to `seen`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Note {
    /// Before passing it on: a request is waited for even when its answer comes back while
    /// The rest of the chunk is still being written.
    BeforeWriting,
    /// Once it was passed on: an answer only counts once the framework has it.
    AfterWriting,
}

/// Copies `from` to `to` (flushing each chunk) until `from` ends, passing each chunk to
/// `seen` ([`Note`]).
async fn pump<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    from: &mut R,
    to: &mut W,
    note: Note,
    mut seen: impl FnMut(&[u8]),
) -> io::Result<()> {
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let n = from.read(&mut buffer).await?;
        if n == 0 {
            to.flush().await?;
            return Ok(());
        }
        if note == Note::BeforeWriting {
            seen(&buffer[..n]);
        }
        to.write_all(&buffer[..n]).await?;
        to.flush().await?;
        if note == Note::AfterWriting {
            seen(&buffer[..n]);
        }
    }
}

/// The requests the framework sent that the player has not answered yet, by id (JSON-RPC
/// messages are lines). A request the framework cancels (`notifications/cancelled`) is not
/// waited for: rmcp does not answer it. Only the ids are read; a line too long to read
/// ([`MAX_TRACKED_LINE`]) is passed on unread: a request is then not waited for, and an
/// answer too long to read is taken as answering everything (the bridge would rather exit
/// than wait for an answer it cannot see).
#[derive(Default)]
pub(super) struct Pending {
    pub(super) ids: HashSet<String>,
    up: Lines,
    down: Lines,
}

impl Pending {
    pub(super) fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Bytes the framework sent.
    pub(super) fn sent(&mut self, bytes: &[u8]) {
        let ids = &mut self.ids;
        self.up.feed(bytes, |line| {
            for message in messages(line?) {
                if let Some(id) = message.cancels {
                    ids.remove(&id.to_string());
                } else if message.method
                    && let Some(id) = message.id.filter(|id| !id.is_null())
                {
                    ids.insert(id.to_string());
                }
            }
            Some(())
        });
    }

    /// Bytes the player sent.
    pub(super) fn answered(&mut self, bytes: &[u8]) {
        let ids = &mut self.ids;
        self.down.feed(bytes, |line| {
            let Some(line) = line else {
                ids.clear();
                return None;
            };
            for message in messages(line) {
                if !message.method
                    && let Some(id) = message.id
                {
                    ids.remove(&id.to_string());
                }
            }
            Some(())
        });
    }
}

/// What the bridge reads of a JSON-RPC message: its id, whether it is a request or a
/// notification (it has a method) or an answer, and the request a `notifications/cancelled`
/// cancels.
struct Probe {
    id: Option<Value>,
    method: bool,
    cancels: Option<Value>,
}

/// The method of the notification that cancels a request (`params.requestId`).
const CANCELLED: &str = "notifications/cancelled";

/// The messages in a line: one, or a batch; nothing for a line that is not JSON-RPC.
fn messages(line: &[u8]) -> Vec<Probe> {
    let probe = |message: Value| match message {
        Value::Object(mut map) => {
            let method = map.contains_key("method");
            let cancels = if map.get("method").and_then(Value::as_str) == Some(CANCELLED) {
                map.get_mut("params")
                    .and_then(Value::as_object_mut)
                    .and_then(|params| params.remove("requestId"))
                    .filter(|id| !id.is_null())
            } else {
                None
            };
            Some(Probe {
                method,
                id: map.remove("id"),
                cancels,
            })
        }
        _ => None,
    };
    match serde_json::from_slice::<Value>(line) {
        Ok(Value::Array(batch)) => batch.into_iter().filter_map(probe).collect(),
        Ok(message) => probe(message).into_iter().collect(),
        Err(_) => Vec::new(),
    }
}

/// Splits a byte stream into lines.
#[derive(Default)]
struct Lines {
    line: Vec<u8>,
    /// The current line grew too long: it is dropped up to its end.
    skipping: bool,
}

impl Lines {
    /// Calls `each` with every line `bytes` completes (without its `\n`), or `None` for a
    /// line too long to keep.
    fn feed(&mut self, mut bytes: &[u8], mut each: impl FnMut(Option<&[u8]>) -> Option<()>) {
        while !bytes.is_empty() {
            let (part, end) = match bytes.iter().position(|&b| b == b'\n') {
                Some(at) => (&bytes[..at], Some(at + 1)),
                None => (bytes, None),
            };
            if !self.skipping {
                if self.line.len() + part.len() > MAX_TRACKED_LINE {
                    self.skipping = true;
                    self.line = Vec::new();
                    let _ = each(None);
                } else {
                    self.line.extend_from_slice(part);
                }
            }
            match end {
                Some(end) => {
                    if !self.skipping {
                        let _ = each(Some(&self.line));
                    }
                    self.line.clear();
                    self.skipping = false;
                    bytes = &bytes[end..];
                }
                None => bytes = &[],
            }
        }
    }
}

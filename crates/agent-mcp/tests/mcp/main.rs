//! The MCP service against a real core and library (null output): an rmcp client connects
//! through the real endpoint (a Unix socket, a named pipe on Windows), presents an agent's
//! token, and calls the tools (docs/design/agent-permissions.md).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use mpmp_agent_mcp::endpoint::{self, ClientStream, Endpoint};
use mpmp_agent_mcp::handshake;
use mpmp_agent_mcp::{Server, ServerHandle, ServerOptions};
use mpmp_core::contract::*;
use mpmp_core::{
    Catalog, CoreConfig, CoreHandle, Doubt, Locations, SubscribeOptions, Subscription,
};
use mpmp_engine::NullOutput;
use mpmp_engine::output::{SampleType, StreamFormat};
use mpmp_media_io::FileResolver;
use rmcp::model::{CallToolRequestParams, ProtocolVersion};
use rmcp::service::RunningService;
use rmcp::{ClientLifecycleMode, ClientServiceExt, Peer, RoleClient, ServiceExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};

/// Every test as a whole, and every wait for an event.
const TIMEOUT: Duration = Duration::from_secs(60);

type Client = RunningService<RoleClient, ()>;

struct Harness {
    core: CoreHandle,
    /// The UI: sees confirmation requests, answers them.
    ui: Subscription,
    server: Option<ServerHandle>,
    endpoint: Endpoint,
    /// The token of agent `claude`.
    token: String,
    /// Track ids by title.
    tracks: Vec<(String, TrackId)>,
    // Clean up after the server and core release the catalog.
    dir: mpmp_test_support::TempDir,
}

fn test_endpoint(dir: &Path) -> Endpoint {
    #[cfg(unix)]
    {
        Endpoint::new(dir.join("run").join("mcp.sock")).unwrap()
    }
    #[cfg(windows)]
    {
        use std::sync::atomic::{AtomicU32, Ordering};
        let _ = dir;
        static NEXT: AtomicU32 = AtomicU32::new(0);
        Endpoint::new(format!(
            r"\\.\pipe\pmp-mcp-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
        .unwrap()
    }
}

/// A silent WAV; its title is its file name.
fn wav(path: &Path) {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 8_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for _ in 0..8_000 {
        w.write_sample(0i16).unwrap();
    }
    w.finalize().unwrap();
}

fn folder(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::create_dir_all(&path).unwrap();
    PathBuf::from(mpmp_fs_desktop::DesktopLocations.display(&path.display().to_string()))
}

/// The desktop rules, with a gate that holds normalization until the test releases it.
#[derive(Default)]
struct SlowLocations {
    blocked: std::sync::Mutex<bool>,
    released: std::sync::Condvar,
    entered: AtomicU64,
}

impl SlowLocations {
    fn block(&self) {
        *self.blocked.lock().unwrap() = true;
    }

    fn release(&self) {
        *self.blocked.lock().unwrap() = false;
        self.released.notify_all();
    }

    async fn wait_until_entered(&self, count: u64) {
        tokio::time::timeout(TIMEOUT, async {
            while self.entered.load(Ordering::SeqCst) < count {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the location check never started");
    }
}

fn desktop() -> &'static dyn Locations {
    &mpmp_fs_desktop::DesktopLocations
}

impl Locations for SlowLocations {
    fn normalize(&self, location: &str) -> Option<String> {
        let blocked = self.blocked.lock().unwrap();
        if *blocked {
            self.entered.fetch_add(1, Ordering::SeqCst);
            let (_blocked, timeout) = self
                .released
                .wait_timeout_while(blocked, TIMEOUT, |b| *b)
                .unwrap();
            assert!(!timeout.timed_out(), "location check was never released");
        } else {
            drop(blocked);
        }
        desktop().normalize(location)
    }

    fn contains(&self, scope: &str, location: &str, doubt: Doubt) -> bool {
        desktop().contains(scope, location, doubt)
    }

    fn child(&self, location: &str, relative: &str) -> Option<String> {
        desktop().child(location, relative)
    }

    fn ignores_case(&self, location: &str) -> Option<bool> {
        desktop().ignores_case(location)
    }

    fn display(&self, location: &str) -> String {
        desktop().display(location)
    }
}

impl Harness {
    fn new(options: ServerOptions) -> Self {
        Self::with_config(
            options,
            CoreConfig {
                automatic_loudness_scan: Some(false),
                ..Default::default()
            },
        )
    }

    fn with_config(options: ServerOptions, config: CoreConfig) -> Self {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let library = mpmp_library::Library::open(&dir.path().join("library.db")).unwrap();
        let output = NullOutput::new(StreamFormat {
            sample_rate: 44_100,
            channels: 2,
            sample_type: SampleType::F32,
        });
        let core = CoreHandle::start(
            Box::new(output),
            Arc::new(FileResolver),
            Arc::new(library) as Arc<dyn Catalog>,
            config,
        )
        .unwrap();
        let ui = core
            .subscribe(
                Origin::Ui,
                SubscribeOptions::default().with_positions(false),
            )
            .unwrap();
        let music = folder(dir.path(), "music");
        for title in ["Blue Moon", "Moon Song", "Red River"] {
            wav(&music.join(format!("{title}.wav")));
        }
        let mut h = Self {
            endpoint: test_endpoint(dir.path()),
            core,
            ui,
            server: None,
            token: String::new(),
            tracks: Vec::new(),
            dir,
        };
        h.ui_ok(Command::AddRoot {
            path: music.display().to_string(),
        });
        h.wait_ui(|e| matches!(e, Event::JobFinished { .. }));
        h.token = h.register("claude");
        h.tracks = match h.core.query(Query::Tracks {
            query: TrackQuery::default(),
            offset: 0,
            limit: 100,
            tags: Vec::new(),
        }) {
            QueryResult::Tracks { tracks, .. } => {
                tracks.into_iter().map(|t| (t.title, t.id)).collect()
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(h.tracks.len(), 3);
        let options = options.with_endpoint(h.endpoint.clone());
        h.server = Some(Server::start(h.core.clone(), options).unwrap());
        h
    }

    fn track(&self, title: &str) -> i64 {
        self.tracks.iter().find(|(t, _)| t == title).unwrap().1.0
    }

    fn register(&self, name: &str) -> String {
        self.ui_ok(Command::RegisterAgent { name: name.into() })
            .agent_token
            .unwrap()
    }

    fn ui_run(&self, command: Command) -> CommandResult {
        self.core
            .execute(Request::new(1, command), Origin::Ui)
            .result
    }

    fn ui_ok(&self, command: Command) -> Receipt {
        match self.ui_run(command) {
            CommandResult::Ok(receipt) => receipt,
            other => panic!("{other:?}"),
        }
    }

    /// Waits (blocking this worker, which the multi-threaded test runtime allows) for a UI
    /// event.
    fn wait_ui(&self, pred: impl Fn(&Event) -> bool) -> EventEnvelope {
        tokio::task::block_in_place(|| {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                assert!(Instant::now() < deadline, "event never came");
                if let Ok(envelope) = self.ui.events.recv_timeout(Duration::from_millis(20))
                    && pred(&envelope.event)
                {
                    return envelope;
                }
            }
        })
    }

    fn confirmation_request(&self) -> PendingConfirmation {
        match self
            .wait_ui(|e| matches!(e, Event::ConfirmationRequested { .. }))
            .event
        {
            Event::ConfirmationRequested { confirmation } => *confirmation,
            _ => unreachable!(),
        }
    }

    async fn client(&self) -> Client {
        self.client_with(&self.token).await
    }

    async fn client_with(&self, token: &str) -> Client {
        let (read, write) = open(&self.endpoint, token).await.unwrap();
        ().serve((read, write)).await.unwrap()
    }

    fn grants(&self) -> Vec<Grant> {
        match self.core.query(Query::Permissions {
            agent: None,
            offset: 0,
            limit: 100,
        }) {
            QueryResult::Permissions { grants, .. } => grants,
            other => panic!("{other:?}"),
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            tokio::task::block_in_place(|| server.shutdown());
        }
        // The server may still have a session finishing on another thread.
        // Close the catalog even if that session holds a clone of the core.
        let closed = self.core.shutdown_with_result(TIMEOUT);
        if !std::thread::panicking() {
            closed.unwrap();
        }
    }
}

type Halves = (BufReader<ReadHalf<ClientStream>>, WriteHalf<ClientStream>);

/// Connects and presents `token`: the connection once accepted, else the adapter's line.
async fn open(endpoint: &Endpoint, token: &str) -> Result<Halves, String> {
    open_with(endpoint, &handshake::hello_line(token)).await
}

async fn open_with(endpoint: &Endpoint, hello: &str) -> Result<Halves, String> {
    let stream = endpoint::connect(endpoint)
        .await
        .map_err(|e| e.to_string())?;
    let (read, mut write) = tokio::io::split(stream);
    let mut read = BufReader::new(read);
    write.write_all(hello.as_bytes()).await.unwrap();
    let line = handshake::read_line(&mut read)
        .await
        .unwrap()
        .unwrap_or_default();
    if line.starts_with("OK ") {
        Ok((read, write))
    } else {
        // Refused: the adapter closes the connection.
        let mut rest = Vec::new();
        let closed = tokio::time::timeout(TIMEOUT, read.read_to_end(&mut rest)).await;
        assert!(matches!(closed, Ok(Ok(0))), "{closed:?}");
        Err(line)
    }
}

/// Calls a tool: whether it failed, and its JSON (text and structured content agree).
async fn call(client: &Peer<RoleClient>, name: &str, args: Value) -> (bool, Value) {
    let Value::Object(args) = args else {
        panic!("arguments must be an object")
    };
    let result = client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(args))
        .await
        .unwrap();
    let text = &result.content[0].as_text().unwrap().text;
    let value: Value = serde_json::from_str(text).unwrap();
    assert_eq!(result.structured_content.as_ref(), Some(&value));
    (result.is_error == Some(true), value)
}

async fn ok(client: &Peer<RoleClient>, name: &str, args: Value) -> Value {
    let (failed, value) = call(client, name, args).await;
    assert!(!failed, "{name}: {value}");
    value
}

/// A failed call: its contract error's kind and code.
async fn error(client: &Peer<RoleClient>, name: &str, args: Value) -> (String, String) {
    let (failed, value) = call(client, name, args).await;
    assert!(failed, "{name} succeeded: {value}");
    let kind = value["kind"].as_str().unwrap().to_owned();
    let code = value["code"].as_str().unwrap().to_owned();
    assert!(value["message"].is_string(), "{value}");
    (kind, code)
}

fn pair(kind: &str, code: &str) -> (String, String) {
    (kind.to_owned(), code.to_owned())
}

/// Runs a test body with a deadline: nothing here may hang.
async fn within<F: Future<Output = ()>>(body: F) {
    tokio::time::timeout(TIMEOUT * 2, body)
        .await
        .expect("the test took too long");
}

/// Asks for something that needs the user, approves it as the user, and waits for it:
/// the receipt.
async fn approved(h: &Harness, client: &Peer<RoleClient>, tool: &str, args: Value) -> Value {
    let asked = ok(client, tool, args).await;
    assert_eq!(asked["status"], "needs_confirmation", "{asked}");
    let token = asked["token"].as_str().unwrap().to_owned();
    h.confirmation_request();
    h.ui_ok(Command::Confirm {
        token: token.clone(),
    });
    let answer = ok(client, "wait_for_confirmation", json!({"token": token})).await;
    assert_eq!(answer["status"], "approved", "{answer}");
    answer["receipt"].clone()
}

mod tools;

mod confirmations;

mod connections;

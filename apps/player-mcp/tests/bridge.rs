//! The bridge as a real process (`player-mcp`): an MCP client talks to it over stdio, and it
//! reaches an adapter serving a real core through the endpoint (docs/design/agent-permissions.md
//!). When it cannot, it is an MCP server of its own whose one tool says why.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mpmp_agent_mcp::{Endpoint, Server, ServerHandle, ServerOptions};
use mpmp_core::contract::{Command, CommandResult, Event, Origin, Request};
use mpmp_core::{Catalog, CoreConfig, CoreHandle, SubscribeOptions};
use mpmp_engine::NullOutput;
use mpmp_engine::output::{SampleType, StreamFormat};
use mpmp_media_io::FileResolver;
use rmcp::model::{CallToolRequestParams, ProtocolVersion};
use rmcp::{ClientLifecycleMode, ClientServiceExt, ServiceExt};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command as Process};

const TIMEOUT: Duration = Duration::from_secs(60);

struct Player {
    core: CoreHandle,
    server: Option<ServerHandle>,
    endpoint: Endpoint,
    token: String,
    // Clean up after the server and core release the catalog.
    dir: mpmp_test_support::TempDir,
}

fn endpoint(dir: &Path, name: &str) -> Endpoint {
    #[cfg(unix)]
    {
        Endpoint::new(dir.join(name).join("mcp.sock")).unwrap()
    }
    #[cfg(windows)]
    {
        let _ = dir;
        Endpoint::new(format!(
            r"\\.\pipe\pmp-bridge-test-{}-{name}",
            std::process::id()
        ))
        .unwrap()
    }
}

fn ui(core: &CoreHandle, command: Command) -> mpmp_core::contract::Receipt {
    match core.execute(Request::new(1, command), Origin::Ui).result {
        CommandResult::Ok(receipt) => receipt,
        other => panic!("{other:?}"),
    }
}

impl Player {
    /// A core with one track ("Red River") and agent `claude`, served at an endpoint named
    /// `name`.
    fn start(name: &str) -> Self {
        Self::start_at(|dir| endpoint(dir, name))
    }

    /// … served at `at(its folder)`.
    fn start_at(at: impl FnOnce(&Path) -> Endpoint) -> Self {
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
            CoreConfig {
                automatic_loudness_scan: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
        let events = core
            .subscribe(
                Origin::Ui,
                SubscribeOptions::default().with_positions(false),
            )
            .unwrap();
        let music = dir.path().join("music");
        std::fs::create_dir_all(&music).unwrap();
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut wav = hound::WavWriter::create(music.join("Red River.wav"), spec).unwrap();
        for _ in 0..8_000 {
            wav.write_sample(0i16).unwrap();
        }
        wav.finalize().unwrap();
        ui(
            &core,
            Command::AddRoot {
                path: music.display().to_string(),
            },
        );
        loop {
            let envelope = events.events.recv_timeout(TIMEOUT).unwrap();
            if matches!(envelope.event, Event::JobFinished { .. }) {
                break;
            }
        }
        let token = ui(
            &core,
            Command::RegisterAgent {
                name: "claude".into(),
            },
        )
        .agent_token
        .unwrap();
        let endpoint = at(dir.path());
        let server = Server::start(
            core.clone(),
            ServerOptions::default().with_endpoint(endpoint.clone()),
        )
        .unwrap();
        Self {
            core,
            server: Some(server),
            endpoint,
            token,
            dir,
        }
    }
}

impl Drop for Player {
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

/// Starts the bridge; `token: None` leaves `PLAYER_MCP_TOKEN` unset.
fn bridge(endpoint: &Endpoint, token: Option<&str>) -> Child {
    let mut process = bridge_process(token);
    process.env("PLAYER_MCP_ENDPOINT", endpoint.path());
    process.spawn().unwrap()
}

/// The bridge's command, without an endpoint.
fn bridge_process(token: Option<&str>) -> Process {
    let mut process = Process::new(env!("CARGO_BIN_EXE_player-mcp"));
    process
        .env_remove("PLAYER_MCP_ENDPOINT")
        .env_remove("PLAYER_DATA_DIR")
        .env_remove("PLAYER_MANUAL_DATA_DIR")
        .env_remove("PLAYER_MCP_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(token) = token {
        process.env("PLAYER_MCP_TOKEN", token);
    }
    process
}

async fn stderr_of(child: &mut Child) -> String {
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .await
        .unwrap();
    stderr
}

async fn within<F: Future<Output = ()>>(body: F) {
    tokio::time::timeout(TIMEOUT * 2, body)
        .await
        .expect("the test took too long");
}

async fn call(client: &rmcp::Peer<rmcp::RoleClient>, name: &str, args: Value) -> Value {
    let Value::Object(args) = args else {
        panic!("arguments must be an object")
    };
    let result = client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(args))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true), "{result:?}");
    serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_reaches_the_player_through_the_bridge() {
    let player = Player::start("ok");
    within(async {
        let mut child = bridge(&player.endpoint, Some(&player.token));
        let stdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take().unwrap();
        let client = ().serve((stdout, stdin)).await.unwrap();
        let status = call(&client, "player_status", json!({})).await;
        assert_eq!(status["state"], "stopped");
        let found = call(&client, "search_tracks", json!({"text": "river"})).await;
        assert_eq!(found["tracks"][0]["title"], "Red River");
        // The framework closes stdin: the bridge ends cleanly.
        client.cancel().await.unwrap();
        let status = tokio::time::timeout(TIMEOUT, child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success(), "{status:?}");
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .await
            .unwrap();
        assert!(stderr.contains("as agent claude"), "{stderr}");
        assert!(!stderr.contains(&player.token));
    })
    .await;
}

/// What the bridge that could not reach the player told an MCP client: the description of
/// its one tool, the error any call gets (also a tool of a session that worked, as a
/// framework may still call), and, once the client closed stdin, whether it exited with
/// success and its stderr. `discover`: the 2026-07-28 protocol, without `initialize`.
struct Diagnosis {
    description: String,
    error: Value,
    success: bool,
    stderr: String,
}

async fn diagnose(mut child: Child, discover: bool) -> Diagnosis {
    let stdout = child.stdout.take().unwrap();
    let stdin = child.stdin.take().unwrap();
    let client = if discover {
        ().serve_with_lifecycle(
            (stdout, stdin),
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .unwrap()
    } else {
        ().serve((stdout, stdin)).await.unwrap()
    };
    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 1, "{tools:?}");
    assert_eq!(tools[0].name, "player_mcp_status");
    let description = tools[0].description.as_deref().unwrap().to_owned();
    let mut errors = Vec::new();
    for name in ["player_mcp_status", "player_status"] {
        let result = client
            .call_tool(CallToolRequestParams::new(name.to_owned()))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true), "{result:?}");
        let text: Value = serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(result.structured_content.as_ref(), Some(&text));
        errors.push(text);
    }
    assert_eq!(errors[0], errors[1], "any tool: the same answer");
    // The framework is done: stdin closes, the bridge exits.
    client.cancel().await.unwrap();
    let status = tokio::time::timeout(TIMEOUT, child.wait())
        .await
        .expect("the bridge stayed after stdin closed")
        .unwrap();
    let stderr = stderr_of(&mut child).await;
    Diagnosis {
        description,
        error: errors.remove(0),
        success: status.success(),
        stderr,
    }
}

impl Diagnosis {
    /// The failure with this code, whose reason says `says`: in the tool's description, the
    /// call's error and stderr.
    fn check(&self, code: i64, says: &str) {
        assert!(!self.success);
        assert_eq!(self.error["code"], code, "{}", self.error);
        let reason = self.error["reason"].as_str().unwrap();
        let remedy = self.error["tell_the_user"].as_str().unwrap();
        assert!(reason.contains(says), "{reason}");
        assert!(
            self.description
                .contains(&format!("not available right now: {reason}")),
            "{}",
            self.description
        );
        assert!(
            self.description
                .contains(&format!("Tell the user: {remedy}")),
            "{}",
            self.description
        );
        assert_eq!(self.error["message"], self.description);
        assert!(
            self.stderr
                .contains(&format!("player-mcp: {reason} — {remedy}")),
            "{}",
            self.stderr
        );
    }
}

async fn send(stdin: &mut tokio::process::ChildStdin, message: Value) {
    stdin
        .write_all(format!("{message}\n").as_bytes())
        .await
        .unwrap();
    stdin.flush().await.unwrap();
}

fn initialize(id: u64) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": {
        "protocolVersion": "2025-11-25", "capabilities": {},
        "clientInfo": {"name": "test", "version": "1"}}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_player_the_bridge_says_so() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    within(async {
        let nowhere = endpoint(dir.path(), "nobody");
        // Both lifecycles: with initialize, and 2026-07-28 without it.
        for discover in [false, true] {
            let child = bridge(&nowhere, Some("pmpa_0123"));
            let diagnosis = diagnose(child, discover).await;
            diagnosis.check(-32000, "not running");
            assert!(
                diagnosis.description.contains("start mpmp"),
                "{}",
                diagnosis.description
            );
        }
    })
    .await;
}

/// The player starts after the bridge did (a framework restarted the bridge while the
/// player was restarting): the next tool call finds it listening, says to call again, and
/// the bridge exits, so that a framework that restarts MCP servers (Claude Code) starts a
/// new one, which connects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_player_that_starts_later_is_found_at_the_next_call() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    within(async {
        let later = endpoint(dir.path(), "later");
        let mut child = bridge(&later, Some("pmpa_0123"));
        let stdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take().unwrap();
        let client = ().serve((stdout, stdin)).await.unwrap();
        let status = || CallToolRequestParams::new("player_status".to_owned());
        // Not yet: the diagnosis, and the bridge stays.
        let result = client.call_tool(status()).await.unwrap();
        assert_eq!(result.is_error, Some(true), "{result:?}");
        assert_eq!(result.structured_content.as_ref().unwrap()["code"], -32000);
        let remedy = result.structured_content.as_ref().unwrap()["tell_the_user"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            remedy.contains("call a tool of this server again"),
            "{remedy}"
        );
        assert!(child.try_wait().unwrap().is_none());
        // The player starts: "call again", and the bridge exits without stdin closing.
        let player = Player::start_at(|_| later.clone());
        let started = Instant::now();
        let result = client.call_tool(status()).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(result.is_error, Some(true), "{result:?}");
        let answer = result.structured_content.unwrap();
        assert_eq!(answer["retry"], true, "{answer}");
        let message = answer["message"].as_str().unwrap();
        assert!(message.contains("running now"), "{message}");
        assert!(message.contains("call the tool again"), "{message}");
        assert!(answer.get("code").is_none(), "{answer}");
        let exited = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .expect("the bridge stayed after the player started")
            .unwrap();
        assert!(!exited.success(), "{exited:?}");
        let stderr = stderr_of(&mut child).await;
        assert!(stderr.contains("running now"), "{stderr}");
        drop(client);
        // What the framework then starts connects.
        let mut child = bridge(&player.endpoint, Some(&player.token));
        let stdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take().unwrap();
        let client = ().serve((stdout, stdin)).await.unwrap();
        let status = call(&client, "player_status", json!({})).await;
        assert_eq!(status["state"], "stopped");
        client.cancel().await.unwrap();
        let _ = tokio::time::timeout(TIMEOUT, child.wait()).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_token_is_explained() {
    let player = Player::start("bad");
    within(async {
        let wrong = format!("pmpa_{}", "0".repeat(64));
        // Wrong, missing, or a reference the framework did not expand (Claude Code keeps an
        // unset ${VAR} as it is).
        for token in [Some(wrong.as_str()), None, Some("${PLAYER_MCP_TOKEN}")] {
            let child = bridge(&player.endpoint, token);
            let diagnosis = diagnose(child, token.is_none()).await;
            diagnosis.check(-32001, "PLAYER_MCP_TOKEN");
            let remedy = diagnosis.error["tell_the_user"].as_str().unwrap();
            if token != Some(wrong.as_str()) {
                // Frameworks that do not pass the shell's environment, or write variables
                // another way: the config's env is where to look.
                assert!(remedy.contains("\"env\""), "{remedy}");
            }
            let all = format!(
                "{}{}{}",
                diagnosis.description, diagnosis.error, diagnosis.stderr
            );
            assert!(!all.contains(&wrong));
        }
    })
    .await;
}

/// The player goes away mid-session: the bridge exits non-zero and says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bridge_says_when_the_player_goes_away() {
    let player = Player::start("gone");
    within(async {
        let mut child = bridge(&player.endpoint, Some(&player.token));
        let stdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take().unwrap();
        let client = ().serve((stdout, stdin)).await.unwrap();
        call(&client, "player_status", json!({})).await;
        // A new token: the player closes this agent's connection.
        ui(
            &player.core,
            Command::RenewAgentToken {
                agent: "claude".into(),
            },
        );
        let status = tokio::time::timeout(TIMEOUT, child.wait())
            .await
            .expect("the bridge stayed")
            .unwrap();
        assert_eq!(status.code(), Some(1), "{status:?}");
        let stderr = stderr_of(&mut child).await;
        assert!(stderr.contains("closed the connection"), "{stderr}");
        drop(client);
    })
    .await;
}

/// The framework closes stdin right after a request that takes a while: its answer still
/// gets through before the bridge exits. Windows cannot half close, so there the
/// bridge waits for every answer, then for a quiet moment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_last_answer_gets_through_after_stdin_closes() {
    let player = Player::start("drain");
    within(async {
        let mut child = bridge(&player.endpoint, Some(&player.token));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        send(&mut stdin, initialize(1)).await;
        stdout.read_line(&mut line).await.unwrap();
        let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        send(&mut stdin, initialized).await;
        // A request the user must approve: a token to wait for.
        let folder = player.dir.path().join("extra");
        std::fs::create_dir_all(&folder).unwrap();
        let add = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "library_folders",
            "arguments": {"action": "add", "path": folder.display().to_string()}}});
        send(&mut stdin, add).await;
        line.clear();
        stdout.read_line(&mut line).await.unwrap();
        let asked: Value = serde_json::from_str(&line).unwrap();
        let token = asked["result"]["structuredContent"]["token"]
            .as_str()
            .unwrap_or_else(|| panic!("{asked}"))
            .to_owned();
        // A wait of a second, and stdin closes at once.
        let wait = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
            "name": "wait_for_confirmation",
            "arguments": {"token": token, "timeout_s": 1}}});
        send(&mut stdin, wait).await;
        drop(stdin);
        let started = Instant::now();
        let mut rest = String::new();
        stdout.read_to_string(&mut rest).await.unwrap();
        let status = tokio::time::timeout(TIMEOUT, child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success(), "{status:?}");
        let answer: Value = serde_json::from_str(rest.lines().next().unwrap_or_default())
            .unwrap_or_else(|e| panic!("{e}: {rest}"));
        assert_eq!(answer["id"], 3);
        assert_eq!(answer["result"]["structuredContent"]["status"], "pending");
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(800), "{took:?}");
        // Not the 60 s limit on waiting for answers: on Windows the player does not learn
        // that stdin closed, the bridge notices the last answer itself.
        assert!(took < Duration::from_secs(10), "{took:?}");
    })
    .await;
}

/// A player that does what a named pipe makes the real one do (it cannot be half closed):
/// it never learns that the framework is done and keeps the connection open. It takes any
/// token, reads the requests until the end of input, waits `delay`, answers those not
/// cancelled (`notifications/cancelled`), and then holds the connection.
#[cfg(unix)]
async fn player_that_never_hangs_up(listener: tokio::net::UnixListener, delay: Duration) {
    let (stream, _) = listener.accept().await.unwrap();
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let mut line = String::new();
    read.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("PLAYER-MCP/1 "), "{line}");
    write.write_all(b"OK claude\n").await.unwrap();
    let mut asked = Vec::new();
    loop {
        line.clear();
        if read.read_line(&mut line).await.unwrap() == 0 {
            break;
        }
        let message: Value = serde_json::from_str(&line).unwrap();
        if message["method"] == "notifications/cancelled" {
            let id = &message["params"]["requestId"];
            asked.retain(|asked| asked != id);
        } else if let Some(id) = message.get("id") {
            asked.push(id.clone());
        }
    }
    tokio::time::sleep(delay).await;
    for id in asked {
        let answer = json!({"jsonrpc": "2.0", "id": id, "result": {}});
        write
            .write_all(format!("{answer}\n").as_bytes())
            .await
            .unwrap();
    }
    write.flush().await.unwrap();
    std::future::pending::<()>().await;
}

/// After stdin closes, the bridge waits for the answers to what was asked even when the
/// player keeps the connection open (Windows): it exits about 300 ms after the last
/// answer, not when its 60 s limit runs out, and does not wait for a cancelled request,
/// which rmcp never answers.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bridge_exits_soon_after_the_last_answer_when_the_player_stays() {
    const DELAY: Duration = Duration::from_millis(700);
    const QUIET: Duration = Duration::from_millis(300);
    let request = |id: u64| json!({"jsonrpc": "2.0", "id": id, "method": "tools/list"});
    let cancel = json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": {"requestId": 2, "reason": "the user pressed Esc"}});
    // A request still unanswered when stdin closes; one the framework cancelled first.
    for (lines, answered) in [(vec![request(1)], true), (vec![request(2), cancel], false)] {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let path = dir.path().join("mcp.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let player = tokio::spawn(player_that_never_hangs_up(listener, DELAY));
        within(async {
            let mut child = bridge(&Endpoint::new(&path).unwrap(), Some("pmpa_0123"));
            let mut stdin = child.stdin.take().unwrap();
            for line in &lines {
                send(&mut stdin, line.clone()).await;
            }
            drop(stdin);
            let started = Instant::now();
            let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
                .await
                .expect("the bridge waited for an answer that had come, or never comes")
                .unwrap();
            let took = started.elapsed();
            assert!(status.success(), "{status:?}");
            let mut out = String::new();
            let mut stdout = child.stdout.take().unwrap();
            stdout.read_to_string(&mut out).await.unwrap();
            if answered {
                let answer: Value =
                    serde_json::from_str(out.trim_end()).unwrap_or_else(|e| panic!("{e}: {out}"));
                assert_eq!(answer["id"], 1);
                assert!(
                    took >= DELAY + QUIET - Duration::from_millis(50),
                    "{took:?}"
                );
                assert!(took < DELAY + Duration::from_secs(3), "{took:?}");
            } else {
                assert_eq!(out, "");
                assert!(took < Duration::from_secs(3), "{took:?}");
            }
        })
        .await;
        assert!(!player.is_finished(), "the player kept the connection");
        player.abort();
    }
}

/// The system refuses the connection (another user's socket): its own error, and the token
/// is not sent.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_endpoint_the_system_refuses_is_explained() {
    use std::os::unix::fs::PermissionsExt;
    let dir = mpmp_test_support::TempDir::new().unwrap();
    within(async {
        let path = dir.path().join("mcp.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            // Running as root: nothing is refused.
            return;
        }
        let child = bridge(&Endpoint::new(&path).unwrap(), Some("pmpa_0123"));
        diagnose(child, false).await.check(-32006, "another user");
    })
    .await;
}

/// Explicit data isolation overrides runtime discovery in both the player and bridge.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bridge_finds_the_player_in_an_overridden_data_folder() {
    let player = Player::start_at(|dir| Endpoint::new(dir.join("isolated/run/mcp.sock")).unwrap());
    within(async {
        let mut process = bridge_process(Some(&player.token));
        process.env("PLAYER_DATA_DIR", player.dir.path().join("isolated"));
        let mut child = process.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take().unwrap();
        let client = ().serve((stdout, stdin)).await.unwrap();
        let status = call(&client, "player_status", json!({})).await;
        assert_eq!(status["state"], "stopped");
        client.cancel().await.unwrap();
        assert!(
            tokio::time::timeout(TIMEOUT, child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    })
    .await;
}

/// Without `PLAYER_MCP_ENDPOINT` the bridge looks where the app may listen: the runtime
/// folder first, then the data folder, in `$XDG_DATA_HOME` and in `~/.local/share` (the
/// app may have seen only one of them).
#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bridge_finds_the_player_in_the_data_folder() {
    // Where the app listens (in its folder): it had `XDG_DATA_HOME`, or not.
    for at in [
        "data/mpmp/run/mcp.sock",
        "home/.local/share/mpmp/run/mcp.sock",
        "data/personal-music-player/run/mcp.sock",
        "home/.local/share/personal-music-player/run/mcp.sock",
    ] {
        let player = Player::start_at(|dir| Endpoint::new(dir.join(at)).unwrap());
        within(async {
            let dir = player.dir.path();
            let runtime = dir.join("runtime");
            std::fs::create_dir_all(&runtime).unwrap();
            let mut process = bridge_process(Some(&player.token));
            process
                .env("XDG_RUNTIME_DIR", &runtime)
                .env("XDG_DATA_HOME", dir.join("data"))
                .env("HOME", dir.join("home"));
            let mut child = process.spawn().unwrap();
            let stdout = child.stdout.take().unwrap();
            let stdin = child.stdin.take().unwrap();
            let client = ().serve((stdout, stdin)).await.unwrap();
            let status = call(&client, "player_status", json!({})).await;
            assert_eq!(status["state"], "stopped");
            client.cancel().await.unwrap();
            let status = tokio::time::timeout(TIMEOUT, child.wait())
                .await
                .unwrap()
                .unwrap();
            assert!(status.success(), "{status:?}");
        })
        .await;
    }
}

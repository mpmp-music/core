//! Agent permissions over the real library (docs/design/agent-permissions.md): agent registration and tokens, grants and how
//! they decide what an agent would otherwise have to ask for, their persistence, audit
//! entries, and that only the UI manages any of it.

use mpmp_test_support::client_parity;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mpmp_core::contract::*;
use mpmp_core::{Catalog, CoreConfig, CoreHandle, SubscribeOptions, Subscription};
use mpmp_media_io::FileResolver;

use mpmp_test_support::core as support;

/// A core over a library in `dir` (`library.db`), which a later core can open again.
fn start(dir: &Path) -> (CoreHandle, Subscription) {
    start_with_config(dir, CoreConfig::default())
}

fn start_with_config(dir: &Path, config: CoreConfig) -> (CoreHandle, Subscription) {
    let library = mpmp_library::Library::open(&dir.join("library.db")).unwrap();
    let output = support::null_output();
    let core = CoreHandle::start(
        Box::new(output),
        Arc::new(FileResolver),
        Arc::new(library) as Arc<dyn Catalog>,
        CoreConfig {
            automatic_loudness_scan: Some(false),
            ..config
        },
    )
    .unwrap();
    support::wait_for_output_initialization(&core);
    let sub = core
        .subscribe(Origin::Ui, SubscribeOptions::default())
        .unwrap();
    (core, sub)
}

/// Stops a core and waits until its library is closed.
fn stop(core: CoreHandle, sub: Subscription) {
    drop(sub);
    core.shutdown();
    drop(core);
}

use support::run;

use support::ok;

use support::err;

fn ui(core: &CoreHandle, command: Command) -> Receipt {
    ok(run(core, command, Origin::Ui))
}

fn session(name: &str, session: &str) -> Origin {
    Origin::Agent {
        name: name.into(),
        session: Some(session.into()),
    }
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn folder(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::create_dir_all(&path).unwrap();
    // Canonical, in the form locations are shown (no `\\?\` prefix on Windows).
    PathBuf::from(mpmp_fs_desktop::DesktopLocations.display(&text(&path)))
}

fn text(path: &Path) -> String {
    path.display().to_string()
}

fn add_root(path: &Path) -> Command {
    Command::AddRoot { path: text(path) }
}

fn grant(
    agent: &str,
    session: Option<&str>,
    operation: Operation,
    scope: Scope,
    until: Expiry,
    effect: Effect,
) -> Command {
    Command::GrantPermission {
        agent: agent.into(),
        session: session.map(Into::into),
        operation,
        scope,
        until,
        effect,
    }
}

fn class(class: OperationClass) -> Operation {
    Operation::Class { class }
}

fn command(name: &str) -> Operation {
    Operation::Command {
        command: name.into(),
    }
}

fn location(path: &Path) -> Scope {
    Scope::Location {
        location: text(path),
    }
}

/// That a refusal names none of `places`, in its message or its details: the places a user
/// forbids an agent are not the agent's to learn (`forbidden_by` says only how much of the
/// command a grant covers).
fn says_nowhere(e: &ContractError, places: &[&str]) {
    let json = serde_json::to_string(e).unwrap();
    for place in places {
        let quoted = serde_json::to_string(place).unwrap();
        assert!(
            !json.contains(quoted.trim_matches('"')) && !e.message.contains(place),
            "{place} in {json}"
        );
    }
}

fn grants(core: &CoreHandle, agent: Option<&str>) -> Vec<Grant> {
    match core.query(Query::Permissions {
        agent: agent.map(Into::into),
        offset: 0,
        limit: 100,
    }) {
        QueryResult::Permissions { grants, total, .. } => {
            assert_eq!(total as usize, grants.len());
            grants
        }
        other => panic!("{other:?}"),
    }
}

fn agents(core: &CoreHandle) -> Vec<AgentInfo> {
    match core.query(Query::Agents) {
        QueryResult::Agents { agents } => agents,
        other => panic!("{other:?}"),
    }
}

fn actions(core: &CoreHandle) -> Vec<ActionSummary> {
    match core.query(Query::Actions {
        origin: None,
        offset: 0,
        limit: 100,
    }) {
        QueryResult::Actions { actions, .. } => actions,
        other => panic!("{other:?}"),
    }
}

fn action(core: &CoreHandle, id: ActionId) -> ActionSummary {
    actions(core).into_iter().find(|a| a.id == id).unwrap()
}

/// Command events up to now (guarantee 3), excluding unrelated asynchronous
/// position and output discovery notifications.
fn drain(sub: &Subscription) -> Vec<EventEnvelope> {
    sub.events
        .try_iter()
        .filter(|e| {
            !matches!(
                e.event,
                Event::Position { .. }
                    | Event::OutputDevicesChanged { .. }
                    | Event::OutputChanged { .. }
            )
        })
        .collect()
}

use support::wait_event;

fn is_needs_confirmation(result: &CommandResult) -> bool {
    matches!(result, CommandResult::Err { error } if error.kind == ErrorKind::NeedsConfirmation)
}

fn confirmation_of(sub: &Subscription) -> PendingConfirmation {
    let asked = wait_event(sub, |e| {
        matches!(e.event, Event::ConfirmationRequested { .. })
    });
    let Event::ConfirmationRequested { confirmation } = asked.event else {
        unreachable!()
    };
    *confirmation
}

#[cfg(unix)]
fn path_of(command: &Command) -> &str {
    match command {
        Command::AddRoot { path }
        | Command::ImportM3u { path, .. }
        | Command::ExportM3u { path, .. } => path,
        other => panic!("{other:?}"),
    }
}

fn new_playlist(core: &CoreHandle, name: &str) -> PlaylistId {
    ui(
        core,
        Command::CreatePlaylist {
            name: name.into(),
            kind: PlaylistKind::Manual,
        },
    )
    .playlist
    .unwrap()
    .playlist
}

/// A catalog for the failure tests: no tracks; its permissions, or its library database,
/// fail as told.
struct Failing {
    permissions: bool,
    damaged: bool,
}

fn damaged() -> ContractError {
    ContractError::failed(
        ErrorCode::LibraryDamaged,
        "database error: database disk image is malformed",
    )
}

impl mpmp_core::catalog::Browse for Failing {
    fn tracks(&self, _ids: &[TrackId]) -> Result<Vec<TrackInfo>, ContractError> {
        Ok(Vec::new())
    }

    fn uri_of(&self, _id: TrackId) -> Result<Option<mpmp_media_io::MediaUri>, ContractError> {
        Ok(None)
    }

    fn context_tracks(&self, _context: &PlayContext) -> Result<Vec<TrackId>, ContractError> {
        Ok(Vec::new())
    }

    fn query(&self, _query: &Query) -> QueryResult {
        match self.damaged {
            true => QueryResult::Err { error: damaged() },
            false => QueryResult::Roots { roots: Vec::new() },
        }
    }
}

impl mpmp_core::catalog::Playlists for Failing {}

impl mpmp_core::catalog::LibraryRoots for Failing {}

impl mpmp_core::catalog::Actions for Failing {
    fn set_loved(
        &self,
        _tracks: &[TrackId],
        _loved: bool,
        _origin: &Origin,
    ) -> Result<mpmp_core::Recorded<Vec<TrackId>>, ContractError> {
        Err(damaged())
    }
}

impl mpmp_core::catalog::CatalogChecks for Failing {}

impl mpmp_core::catalog::AgentPermissions for Failing {
    fn load_permissions(&self) -> Result<mpmp_core::StoredPermissions, ContractError> {
        if self.permissions {
            Err(ContractError::failed(ErrorCode::Db, "disk I/O error").retryable())
        } else {
            Ok(mpmp_core::StoredPermissions::default())
        }
    }
}

impl mpmp_core::catalog::MediaResources for Failing {}

impl mpmp_core::catalog::Loudness for Failing {}

impl mpmp_core::catalog::TagsAndCovers for Failing {}

impl mpmp_core::catalog::PlaybackHistory for Failing {
    fn record_play(&self, _record: mpmp_core::PlayRecord) {}
}

fn start_failing(catalog: Failing) -> (CoreHandle, Subscription) {
    let output = support::null_output();
    let core = CoreHandle::start(
        Box::new(output),
        Arc::new(FileResolver),
        Arc::new(catalog) as Arc<dyn Catalog>,
        CoreConfig {
            automatic_loudness_scan: Some(false),
            ..Default::default()
        },
    )
    .expect("data problems never stop core from starting");
    support::wait_for_output_initialization(&core);
    let sub = core
        .subscribe(Origin::Ui, SubscribeOptions::default())
        .unwrap();
    (core, sub)
}

fn problems(core: &CoreHandle) -> Vec<ErrorCode> {
    match core.query(Query::Snapshot) {
        QueryResult::Snapshot { snapshot } => snapshot.problems.iter().map(|p| p.code).collect(),
        other => panic!("{other:?}"),
    }
}

/// Approves the confirmation `token` and returns the held command's own result, as the UI
/// and `ConfirmationResolved` see it.
fn approve(core: &CoreHandle, sub: &Subscription, token: String) -> CommandResult {
    drain(sub);
    let outcome = ui(core, Command::Confirm { token }).confirmed.unwrap();
    assert!(outcome.approved);
    let announced = drain(sub)
        .into_iter()
        .find_map(|e| match e.event {
            Event::ConfirmationResolved { outcome } => Some(outcome),
            _ => None,
        })
        .expect("the answer is announced");
    assert_eq!(announced, outcome);
    *outcome.result.unwrap()
}

/// Runs `f` on a thread of its own and fails the test if it takes more than a few seconds:
/// a FIFO opened to read waits for a writer, and the test must not wait with it. (The core
/// handle `f` holds keeps a stuck core from being joined when the test fails.)
fn promptly<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|_| panic!("{what} did not return: core waits"))
}

/// Makes a FIFO at `path`; false when something is there already (a name that differs
/// only in letter case, where case is ignored).
#[cfg(unix)]
fn mkfifo(path: &Path) -> bool {
    std::process::Command::new("mkfifo")
        .arg(path)
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// Drops an action from the journal as pruning does (its targets go with it).
fn forget_action(dir: &Path, action: ActionId) {
    let db = rusqlite::Connection::open(dir.join("library.db")).unwrap();
    db.pragma_update(None, "foreign_keys", true).unwrap();
    assert_eq!(
        db.execute("DELETE FROM actions WHERE id = ?1", [action.0])
            .unwrap(),
        1
    );
}

/// The desktop platform layer, slowed down as an offline network location is: any call
/// about a location in a `pmp-slow` folder waits while the gate is shut (a minute at most,
/// so that no thread of the test is left for good).
struct Slow {
    shut: std::sync::Mutex<bool>,
    changed: std::sync::Condvar,
    waited: std::sync::atomic::AtomicUsize,
}

impl Slow {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            shut: std::sync::Mutex::new(false),
            changed: std::sync::Condvar::new(),
            waited: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn shut(&self, shut: bool) {
        *self.shut.lock().unwrap() = shut;
        self.changed.notify_all();
    }

    /// Calls that have waited at the gate so far.
    fn waited(&self) -> usize {
        self.waited.load(std::sync::atomic::Ordering::Acquire)
    }

    fn wait(&self, locations: &[&str]) {
        if !locations.iter().any(|l| l.contains("pmp-slow")) {
            return;
        }
        let mut shut = self.shut.lock().unwrap();
        if !*shut {
            return;
        }
        self.waited
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let deadline = Instant::now() + Duration::from_secs(60);
        while *shut {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            shut = self.changed.wait_timeout(shut, left).unwrap().0;
        }
    }
}

impl mpmp_core::Locations for Slow {
    fn normalize(&self, location: &str) -> Option<String> {
        self.wait(&[location]);
        mpmp_fs_desktop::DesktopLocations.normalize(location)
    }
    fn contains(&self, scope: &str, location: &str, doubt: mpmp_core::Doubt) -> bool {
        self.wait(&[scope, location]);
        mpmp_fs_desktop::DesktopLocations.contains(
            scope,
            location,
            doubt == mpmp_core::Doubt::Include,
        )
    }
    fn child(&self, location: &str, relative: &str) -> Option<String> {
        self.wait(&[location]);
        mpmp_fs_desktop::DesktopLocations.child(location, relative)
    }
    fn ignores_case(&self, location: &str) -> Option<bool> {
        self.wait(&[location]);
        mpmp_fs_desktop::DesktopLocations.ignores_case(location)
    }
    fn display(&self, location: &str) -> String {
        self.wait(&[location]);
        mpmp_fs_desktop::DesktopLocations.display(location)
    }
}

/// A core whose platform layer is `slow`, with `limit` on an agent command's file system
/// calls.
fn start_slow(dir: &Path, slow: &Arc<Slow>, limit: Duration) -> (CoreHandle, Subscription) {
    let library = mpmp_library::Library::open(&dir.join("library.db")).unwrap();
    let output = support::null_output();
    let core = CoreHandle::start(
        Box::new(output),
        Arc::new(FileResolver),
        Arc::new(library) as Arc<dyn Catalog>,
        CoreConfig {
            locations: Some(slow.clone()),
            agent_file_system_timeout: Some(limit),
            ..Default::default()
        },
    )
    .unwrap();
    let sub = core
        .subscribe(Origin::Ui, SubscribeOptions::default())
        .unwrap();
    (core, sub)
}

/// Polls `done` until it holds, for a few seconds at most.
fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "{what} never happened");
        std::thread::sleep(Duration::from_millis(10));
    }
}

mod trusted_ui;

mod grants;

mod scopes;

mod tokens;

mod waiting;

mod damage;

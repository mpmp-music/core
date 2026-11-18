//! Where the app's MCP service listens and how the bridge reaches it: a Unix socket in a
//! private folder, or a named pipe on Windows (docs/design/agent-permissions.md). Both ends
//! make sure the other is the same user before trusting it: the adapter before it reads a
//! token, the bridge before it sends one (another user could otherwise take the name first
//! and collect tokens, or connect and try them).

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Environment variable that overrides the default endpoint (tests, several instances).
pub const ENDPOINT_ENV: &str = "PLAYER_MCP_ENDPOINT";

/// Environment variable that gives the bridge the agent's token.
pub const TOKEN_ENV: &str = "PLAYER_MCP_TOKEN";

/// A Unix socket path, or a Windows named pipe name (`\\.\pipe\…`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    path: PathBuf,
}

impl Endpoint {
    /// An endpoint at `path`: an absolute socket path on Unix, a pipe name
    /// (`\\.\pipe\<name>`) on Windows.
    pub fn new(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        platform::check(&path)?;
        Ok(Self { path })
    }

    /// Where the current user's player listens: on Linux and other Unix systems
    /// `$XDG_RUNTIME_DIR/personal-music-player/mcp.sock`, or without `XDG_RUNTIME_DIR`
    /// `mpmp/run/mcp.sock` in `$XDG_DATA_HOME` (else `~/.local/share`); on
    /// macOS always `~/Library/Application Support/mpmp/run/mcp.sock` (an
    /// app started from Finder or the Dock does not see the XDG variables a shell may set);
    /// on Windows `\\.\pipe\personal-music-player-mcp-<the user's SID>`. Without `HOME`
    /// the home folder comes from the user database (`getpwuid_r`).
    pub fn for_current_user() -> io::Result<Self> {
        Ok(Self {
            path: platform::default_path()?,
        })
    }

    /// Where the bridge looks for the current user's player, in order: the app and the
    /// framework that starts the bridge are started apart and may not see the same
    /// environment. On Linux and other Unix systems the runtime folder's socket (when
    /// `XDG_RUNTIME_DIR` is set), then the data folder's in `$XDG_DATA_HOME` and in
    /// `~/.local/share` (either may be the app's), then `/run/user/<uid>`'s (the usual
    /// runtime folder, for a framework started without `XDG_RUNTIME_DIR`). Legacy
    /// personal-music-player data-folder sockets are appended on Linux and macOS;
    /// Windows keeps its per-user pipe. The first is always where the app listens with the
    /// same environment. Each is checked on its own before the token goes there
    /// ([`connect_any`]).
    pub fn candidates_for_current_user() -> io::Result<Vec<Self>> {
        Ok(platform::candidate_paths()?
            .into_iter()
            .map(|path| Self { path })
            .collect())
    }

    /// The app's endpoint: [`ENDPOINT_ENV`] when set (and not empty), else
    /// [`Endpoint::for_current_user`].
    pub fn from_env() -> io::Result<Self> {
        selected_endpoints(std::env::var_os(ENDPOINT_ENV), || {
            Ok(vec![Self::for_current_user()?])
        })?
        .pop()
        .ok_or_else(|| io::Error::other("no endpoint"))
    }

    /// The bridge's endpoints: only ENDPOINT_ENV when set (and not empty), else
    /// new candidates followed by legacy data-folder candidates.
    pub fn candidates_from_env() -> io::Result<Vec<Self>> {
        selected_endpoints(
            std::env::var_os(ENDPOINT_ENV),
            Self::candidates_for_current_user,
        )
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn selected_endpoints(
    configured: Option<std::ffi::OsString>,
    fallback: impl FnOnce() -> io::Result<Vec<Endpoint>>,
) -> io::Result<Vec<Endpoint>> {
    match configured.filter(|p| !p.is_empty()) {
        Some(path) => Ok(vec![Endpoint::new(path)?]),
        None => fallback(),
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.path.display())
    }
}

/// Why the bridge could not reach the player.
#[derive(Debug)]
pub enum ConnectError {
    /// Nothing listens at the endpoint: the player is not running (or not serving agents).
    NotRunning(io::Error),
    /// What listens there runs as another user: it must not get the token.
    NotSameUser,
    /// The system refused the connection (Unix `EACCES`, Windows `ERROR_ACCESS_DENIED`): the
    /// endpoint, or its folder, belongs to another user.
    AccessDenied(io::Error),
    Io(io::Error),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectError::NotRunning(err) => write!(f, "nothing listens at the endpoint: {err}"),
            ConnectError::NotSameUser => write!(f, "the endpoint belongs to another user"),
            ConnectError::AccessDenied(err) => {
                write!(f, "access to the endpoint was denied: {err}")
            }
            ConnectError::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ConnectError {}

/// The bridge's side of a connection.
#[cfg(unix)]
pub type ClientStream = tokio::net::UnixStream;

#[cfg(windows)]
pub type ClientStream = tokio::net::windows::named_pipe::NamedPipeClient;

/// Connects to the player at `endpoint` and checks that it runs as the current user, before
/// anything is sent.
#[cfg(any(unix, windows))]
pub async fn connect(endpoint: &Endpoint) -> Result<ClientStream, ConnectError> {
    platform::connect(&endpoint.path).await
}

/// Connects to the first of `endpoints` where something listens ([`connect`] each in turn):
/// only "nothing listens there" moves on to the next; any other failure (another user's
/// endpoint, access denied …) ends the search. The stream, and the endpoint it reached.
#[cfg(any(unix, windows))]
pub async fn connect_any(
    endpoints: &[Endpoint],
) -> Result<(ClientStream, &Endpoint), ConnectError> {
    connect_candidates(
        endpoints,
        |endpoint| async move { connect(&endpoint).await },
    )
    .await
}

async fn connect_candidates<T, F, Fut>(
    endpoints: &[Endpoint],
    mut connector: F,
) -> Result<(T, &Endpoint), ConnectError>
where
    F: FnMut(Endpoint) -> Fut,
    Fut: std::future::Future<Output = Result<T, ConnectError>>,
{
    let mut last = ConnectError::NotRunning(io::Error::new(
        io::ErrorKind::NotFound,
        "no endpoint to try",
    ));
    for endpoint in endpoints {
        match connector(endpoint.clone()).await {
            Ok(stream) => return Ok((stream, endpoint)),
            Err(err @ ConnectError::NotRunning(_)) => last = err,
            Err(err) => return Err(err),
        }
    }
    Err(last)
}

/// A failed connect: nothing listens there, the system refused it, or something else.
#[cfg(any(unix, windows))]
fn connect_error(err: io::Error) -> ConnectError {
    match err.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => ConnectError::NotRunning(err),
        io::ErrorKind::PermissionDenied => ConnectError::AccessDenied(err),
        _ => ConnectError::Io(err),
    }
}

#[cfg(unix)]
mod platform {
    use std::io;
    use std::path::{Path, PathBuf};

    use super::ConnectError;

    pub(super) const APP_DIR: &str = "personal-music-player";
    pub(super) const SOCKET: &str = "mcp.sock";
    /// The socket's folder inside the app's data folder, without `XDG_RUNTIME_DIR`.
    pub(super) const RUN_DIR: &str = "run";

    pub(super) fn check(path: &Path) -> io::Result<()> {
        if path.is_absolute() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not an absolute socket path: {}", path.display()),
            ))
        }
    }

    /// The host never listens at legacy bridge fallback addresses.
    pub(super) fn default_path() -> io::Result<PathBuf> {
        host_path(cfg!(target_os = "macos"), or_passwd_home(absolute_env))
    }

    pub(super) fn host_path(
        macos: bool,
        env: impl Fn(&str) -> Option<PathBuf>,
    ) -> io::Result<PathBuf> {
        if !macos
            && env("PLAYER_DATA_DIR")
                .or_else(|| env("PLAYER_MANUAL_DATA_DIR"))
                .is_none()
            && let Some(runtime) = env("XDG_RUNTIME_DIR")
        {
            return Ok(runtime.join(APP_DIR).join(SOCKET));
        }
        mpmp_fs_desktop::data::DataPaths::resolve(
            if macos {
                mpmp_fs_desktop::data::Platform::Macos
            } else {
                mpmp_fs_desktop::data::Platform::Linux
            },
            |key| env(key).map(PathBuf::into_os_string),
            Path::new("/"),
        )
        .map(|paths| paths.root.join(RUN_DIR).join(SOCKET))
    }

    pub(super) fn candidate_paths() -> io::Result<Vec<PathBuf>> {
        candidates(
            cfg!(target_os = "macos"),
            or_passwd_home(absolute_env),
            Some(current_uid()),
        )
    }

    /// `env`, with the user's home folder from the user database when `HOME` is not set
    /// (or not absolute): some frameworks start MCP servers with almost no environment
    /// (GitHub Copilot CLI passes only `PATH`). Without the XDG variables the defaults and
    /// `/run/user/<uid>` still apply.
    pub(super) fn or_passwd_home(
        env: impl Fn(&str) -> Option<PathBuf>,
    ) -> impl Fn(&str) -> Option<PathBuf> {
        move |name| env(name).or_else(|| if name == "HOME" { passwd_home() } else { None })
    }

    /// The current user's home folder in the user database (`getpwuid_r`), if absolute.
    pub(super) fn passwd_home() -> Option<PathBuf> {
        mpmp_fs_desktop::data::home_from_user_database()
    }

    /// Where the socket may be, the app's choice first. Not on macOS:
    /// `$XDG_RUNTIME_DIR/personal-music-player`, then `run/` in the app's data folder
    /// (a folder of its own: the data folder's permissions are left as they are) in
    /// `$XDG_DATA_HOME`, then in `~/.local/share`; with `uid` (the bridge), last
    /// `/run/user/<uid>/personal-music-player`. On macOS only the data folder. No path
    /// twice. `env` reads a variable (absolute paths only).
    pub(super) fn candidates(
        macos: bool,
        env: impl Fn(&str) -> Option<PathBuf>,
        uid: Option<libc::uid_t>,
    ) -> io::Result<Vec<PathBuf>> {
        let runtime = |dir: PathBuf| dir.join(APP_DIR).join(SOCKET);
        // The same resolver as the desktop, including explicit isolation overrides.
        let platform = if macos {
            mpmp_fs_desktop::data::Platform::Macos
        } else {
            mpmp_fs_desktop::data::Platform::Linux
        };
        let data = |env: &dyn Fn(&str) -> Option<PathBuf>| {
            mpmp_fs_desktop::data::DataPaths::resolve(
                platform,
                |key| env(key).map(PathBuf::into_os_string),
                Path::new("/"),
            )
            .ok()
            .map(|paths| paths.root.join(RUN_DIR).join(SOCKET))
        };
        let overridden = env("PLAYER_DATA_DIR")
            .or_else(|| env("PLAYER_MANUAL_DATA_DIR"))
            .is_some();
        let mut found = if overridden || macos {
            vec![data(&env)]
        } else {
            let home_only = |key: &str| {
                if key == "XDG_DATA_HOME" {
                    None
                } else {
                    env(key)
                }
            };
            vec![
                env("XDG_RUNTIME_DIR").map(runtime),
                data(&env),
                data(&home_only),
                uid.map(|uid| runtime(PathBuf::from(format!("/run/user/{uid}")))),
            ]
        };
        if uid.is_some() && !overridden {
            // Only bridge discovery gets these, after EVERY new-version candidate.
            let legacy = |base: PathBuf| base.join(APP_DIR).join(RUN_DIR).join(SOCKET);
            if macos {
                found
                    .push(env("HOME").map(|home| legacy(home.join("Library/Application Support"))));
            } else {
                found.push(env("XDG_DATA_HOME").map(legacy));
                found.push(env("HOME").map(|home| legacy(home.join(".local/share"))));
            }
        }
        let mut paths: Vec<PathBuf> = Vec::new();
        for path in found.into_iter().flatten() {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        if paths.is_empty() {
            return Err(no_folder());
        }
        Ok(paths)
    }

    fn no_folder() -> io::Error {
        io::Error::new(
            io::ErrorKind::NotFound,
            concat!(
                "no folder for the endpoint: XDG_RUNTIME_DIR, XDG_DATA_HOME and HOME are ",
                "not set, and the user database has no home folder",
            ),
        )
    }

    fn absolute_env(name: &str) -> Option<PathBuf> {
        let value = PathBuf::from(std::env::var_os(name)?);
        if matches!(name, "PLAYER_DATA_DIR" | "PLAYER_MANUAL_DATA_DIR") {
            return Some(if value.is_absolute() {
                value
            } else {
                std::env::current_dir().ok()?.join(value)
            });
        }
        value.is_absolute().then_some(value)
    }

    /// The effective user id of this process.
    pub(crate) fn current_uid() -> libc::uid_t {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }

    /// Whether the process at the other end of `stream` runs as the current user (its
    /// effective uid when it connected: `SO_PEERCRED`, `getpeereid`).
    pub(crate) fn peer_is_same_user(stream: &tokio::net::UnixStream) -> io::Result<bool> {
        Ok(stream.peer_cred()?.uid() == current_uid())
    }

    pub(super) async fn connect(path: &Path) -> Result<tokio::net::UnixStream, ConnectError> {
        let stream = tokio::net::UnixStream::connect(path)
            .await
            .map_err(super::connect_error)?;
        match peer_is_same_user(&stream) {
            Ok(true) => Ok(stream),
            Ok(false) => Err(ConnectError::NotSameUser),
            Err(err) => Err(ConnectError::Io(err)),
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::io;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
    use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;

    use super::ConnectError;
    use crate::windows;

    const PIPE_PREFIX: &str = r"\\.\pipe\";

    pub(super) fn check(path: &Path) -> io::Result<()> {
        let name = path.to_string_lossy();
        // Bytes: the name need not be ASCII, so the prefix's end need not be a char boundary.
        let ok = name.len() > PIPE_PREFIX.len()
            && name.as_bytes()[..PIPE_PREFIX.len()].eq_ignore_ascii_case(PIPE_PREFIX.as_bytes());
        if ok {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(r"not a pipe name (\\.\pipe\<name>): {name}"),
            ))
        }
    }

    /// Named after the user's SID (`S-1-5-21-…`): unique (a logon name is not: a local and
    /// a domain account may share one) and ASCII.
    pub(super) fn default_path() -> io::Result<PathBuf> {
        let sid = windows::current_user()?.to_string_sid()?;
        Ok(PathBuf::from(format!(
            "{PIPE_PREFIX}personal-music-player-mcp-{sid}"
        )))
    }

    pub(super) fn candidate_paths() -> io::Result<Vec<PathBuf>> {
        Ok(vec![default_path()?])
    }

    pub(super) async fn connect(path: &Path) -> Result<NamedPipeClient, ConnectError> {
        // All instances busy: the server makes the next one at once; wait for it a little.
        let deadline = Instant::now() + crate::handshake::TIMEOUT;
        let client = loop {
            match ClientOptions::new().open(path) {
                Ok(client) => break client,
                Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                    if Instant::now() >= deadline {
                        return Err(ConnectError::Io(err));
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(err) => return Err(super::connect_error(err)),
            }
        };
        match windows::server_is_same_user(&client) {
            Ok(true) => Ok(client),
            Ok(false) => Err(ConnectError::NotSameUser),
            Err(err) => Err(ConnectError::Io(err)),
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use std::io;
    use std::path::{Path, PathBuf};

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "no local agent endpoint on this platform",
        )
    }

    pub(super) fn check(_path: &Path) -> io::Result<()> {
        Err(unsupported())
    }

    pub(super) fn default_path() -> io::Result<PathBuf> {
        Err(unsupported())
    }

    pub(super) fn candidate_paths() -> io::Result<Vec<PathBuf>> {
        Err(unsupported())
    }
}

#[cfg(all(unix, feature = "server"))]
pub(crate) use platform::{current_uid, peer_is_same_user};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_checked() {
        #[cfg(unix)]
        {
            assert!(Endpoint::new("relative/mcp.sock").is_err());
            let e = Endpoint::new("/run/user/1000/x/mcp.sock").unwrap();
            assert_eq!(e.to_string(), "/run/user/1000/x/mcp.sock");
        }
        #[cfg(windows)]
        {
            assert!(Endpoint::new(r"C:\x").is_err());
            Endpoint::new(r"\\.\pipe\x").unwrap();
            // Not ASCII where the prefix ends: refused, no panic.
            assert!(Endpoint::new("ab中文pipe-name").is_err());
            Endpoint::new(r"\\.\pipe\中文").unwrap();
        }
        let default = Endpoint::for_current_user().unwrap();
        #[cfg(unix)]
        {
            let runtime = Path::new(platform::APP_DIR).join(platform::SOCKET);
            let data = Path::new(mpmp_fs_desktop::data::APP_DIR)
                .join(platform::RUN_DIR)
                .join(platform::SOCKET);
            let path = default.path();
            assert!(
                path.ends_with(&runtime) || path.ends_with(&data),
                "{default}"
            );
        }
        #[cfg(windows)]
        {
            let name = default.to_string();
            assert!(
                name.starts_with(r"\\.\pipe\personal-music-player-mcp-S-1-"),
                "{name}"
            );
            assert!(name.is_ascii(), "{name}");
        }
        // The app's endpoint is the bridge's first choice.
        assert_eq!(Endpoint::candidates_for_current_user().unwrap()[0], default);
    }

    /// The bridge looks where the app may listen: the runtime folder, then the data folder;
    /// on macOS only the data folder, whatever the XDG variables say.
    #[cfg(unix)]
    #[test]
    fn unix_candidates() {
        let all = [
            ("XDG_RUNTIME_DIR", "/run/user/7"),
            ("XDG_DATA_HOME", "/data"),
            ("HOME", "/home/me"),
        ];
        let with_uid = |macos: bool, vars: &[(&str, &str)], uid: Option<u32>| {
            let env = |name: &str| {
                vars.iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| PathBuf::from(v))
            };
            platform::candidates(macos, env, uid.map(|uid| uid as libc::uid_t)).map(|paths| {
                paths
                    .into_iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
            })
        };
        // The app's: no fallback.
        let candidates = |macos: bool, vars: &[(&str, &str)]| with_uid(macos, vars, None);
        assert_eq!(
            candidates(false, &all).unwrap(),
            [
                "/run/user/7/personal-music-player/mcp.sock",
                "/data/mpmp/run/mcp.sock",
                "/home/me/.local/share/mpmp/run/mcp.sock"
            ]
        );
        // The bridge also tries the usual runtime folder; a folder named
        // twice is tried once.
        assert_eq!(
            with_uid(false, &all, Some(7)).unwrap(),
            [
                "/run/user/7/personal-music-player/mcp.sock",
                "/data/mpmp/run/mcp.sock",
                "/home/me/.local/share/mpmp/run/mcp.sock",
                "/data/personal-music-player/run/mcp.sock",
                "/home/me/.local/share/personal-music-player/run/mcp.sock"
            ]
        );
        assert_eq!(
            with_uid(
                false,
                &[("XDG_DATA_HOME", "/home/me/.local/share"), all[2]],
                Some(8)
            )
            .unwrap(),
            [
                "/home/me/.local/share/mpmp/run/mcp.sock",
                "/run/user/8/personal-music-player/mcp.sock",
                "/home/me/.local/share/personal-music-player/run/mcp.sock"
            ]
        );
        assert_eq!(
            with_uid(false, &[], Some(8)).unwrap(),
            ["/run/user/8/personal-music-player/mcp.sock"]
        );
        assert_eq!(
            with_uid(true, &all, Some(8)).unwrap(),
            [
                "/home/me/Library/Application Support/mpmp/run/mcp.sock",
                "/home/me/Library/Application Support/personal-music-player/run/mcp.sock"
            ]
        );
        assert_eq!(
            candidates(false, &all[2..]).unwrap(),
            ["/home/me/.local/share/mpmp/run/mcp.sock"]
        );
        // XDG_DATA_HOME is enough without HOME.
        assert_eq!(
            candidates(false, &all[1..2]).unwrap(),
            ["/data/mpmp/run/mcp.sock"]
        );
        assert_eq!(
            candidates(true, &all).unwrap(),
            ["/home/me/Library/Application Support/mpmp/run/mcp.sock"]
        );
        assert!(candidates(false, &[]).is_err());
        assert!(candidates(true, &all[..2]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn overridden_data_folder_matches_the_desktop_resolver() {
        for macos in [false, true] {
            let env = |key: &str| match key {
                "PLAYER_DATA_DIR" => Some(PathBuf::from("/isolated/mpmp")),
                "PLAYER_MANUAL_DATA_DIR" => Some(PathBuf::from("/unused")),
                "XDG_RUNTIME_DIR" => Some(PathBuf::from("/runtime")),
                "HOME" => Some(PathBuf::from("/home/me")),
                _ => None,
            };
            let root = mpmp_fs_desktop::data::DataPaths::resolve(
                if macos {
                    mpmp_fs_desktop::data::Platform::Macos
                } else {
                    mpmp_fs_desktop::data::Platform::Linux
                },
                |key| env(key).map(PathBuf::into_os_string),
                Path::new("/"),
            )
            .unwrap()
            .root;
            assert_eq!(
                platform::candidates(macos, env, Some(7)).unwrap(),
                [root.join("run/mcp.sock")]
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn host_uses_only_new_address_and_explicit_endpoint_excludes_all_fallbacks() {
        for macos in [false, true] {
            let env = |key: &str| (key == "HOME").then(|| PathBuf::from("/isolated/home"));
            let host = platform::host_path(macos, env).unwrap();
            assert!(host.ends_with("mpmp/run/mcp.sock"));
            let found = platform::candidates(macos, env, Some(7)).unwrap();
            assert_eq!(found[0], host);
            assert!(
                found
                    .last()
                    .unwrap()
                    .ends_with("personal-music-player/run/mcp.sock")
            );
        }
        let explicit = Endpoint::new("/explicit/old/mcp.sock").unwrap();
        let selected = selected_endpoints(Some(explicit.path().as_os_str().to_owned()), || {
            panic!("explicit endpoint must never discover defaults")
        })
        .unwrap();
        assert_eq!(selected, [explicit]);
        assert!(
            selected_endpoints(Some("relative/socket".into()), || panic!(
                "invalid override must fail closed"
            ))
            .is_err()
        );
        assert_eq!(
            selected_endpoints(Some("".into()), || Ok(vec![
                Endpoint::new("/new/mcp.sock").unwrap()
            ]))
            .unwrap()[0]
                .path(),
            Path::new("/new/mcp.sock")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn legacy_fallbacks_each_connect_and_peer_failure_stops_discovery() {
        let paths = [
            "/new/run/mcp.sock",
            "/old-a/run/mcp.sock",
            "/old-b/run/mcp.sock",
        ];
        let endpoints: Vec<_> = paths.iter().map(|p| Endpoint::new(p).unwrap()).collect();
        let mut checked = Vec::new();
        let result = connect_candidates(&endpoints, |endpoint| {
            checked.push(endpoint.path().to_path_buf());
            let result: Result<(), ConnectError> = if checked.len() == 1 {
                Err(ConnectError::NotRunning(io::Error::from(
                    io::ErrorKind::NotFound,
                )))
            } else {
                Err(ConnectError::NotSameUser)
            };
            std::future::ready(result)
        })
        .await;
        assert!(matches!(result, Err(ConnectError::NotSameUser)));
        assert_eq!(
            checked,
            endpoints[..2]
                .iter()
                .map(|e| e.path().to_path_buf())
                .collect::<Vec<_>>()
        );
        let mut checked = Vec::new();
        let (_, found) = connect_candidates(&endpoints, |endpoint| {
            checked.push(endpoint.path().to_path_buf());
            std::future::ready(if checked.len() < 3 {
                Err(ConnectError::NotRunning(io::Error::from(
                    io::ErrorKind::NotFound,
                )))
            } else {
                Ok(())
            })
        })
        .await
        .unwrap();
        assert_eq!(found, &endpoints[2]);
        assert_eq!(checked.len(), 3);
    }

    /// With none of the variables (frameworks may start the bridge with almost no
    /// environment), the home folder comes from the user database: the default places are
    /// still found.
    #[cfg(unix)]
    #[test]
    fn unix_candidates_without_the_environment() {
        let home = platform::passwd_home().expect("the user database has a home folder");
        assert!(home.is_absolute(), "{}", home.display());
        let uid = platform::current_uid();
        let empty = platform::or_passwd_home(|_| None);
        assert_eq!(
            platform::candidates(false, &empty, Some(uid)).unwrap(),
            [
                home.join(".local/share/mpmp/run/mcp.sock"),
                PathBuf::from(format!("/run/user/{uid}/personal-music-player/mcp.sock")),
                home.join(".local/share/personal-music-player/run/mcp.sock"),
            ]
        );
        // The app's own choice too.
        assert_eq!(
            platform::candidates(false, &empty, None).unwrap(),
            [home.join(".local/share/mpmp/run/mcp.sock")]
        );
        assert_eq!(
            platform::candidates(true, &empty, Some(uid)).unwrap(),
            [
                home.join("Library/Application Support/mpmp/run/mcp.sock"),
                home.join("Library/Application Support/personal-music-player/run/mcp.sock")
            ]
        );
        // A HOME that is set wins over the user database.
        let set = platform::or_passwd_home(|name: &str| {
            (name == "HOME").then(|| PathBuf::from("/elsewhere"))
        });
        assert_eq!(
            platform::candidates(false, &set, None).unwrap(),
            [PathBuf::from("/elsewhere/.local/share/mpmp/run/mcp.sock")]
        );
    }
}

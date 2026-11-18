//! The adapter's side of the endpoint: listen, and accept only the current user.
//!
//! Unix: the socket's folder is the user's own (not a symlink, mode 0700), the socket is
//! 0600, and a socket left by an earlier run is removed only when it is ours and nobody
//! answers on it; the peer's user is checked as it is accepted. Windows: the pipe is created
//! first (so another user cannot hold the name), rejects remote clients, only the current
//! user may open it, and the client's user is checked once it has written its first line
//! ([`Late`]: Windows can identify a pipe client only then).

use std::io;

use crate::endpoint::Endpoint;

/// The adapter's side of a connection.
#[cfg(unix)]
pub(crate) type ServerStream = tokio::net::UnixStream;

#[cfg(windows)]
pub(crate) type ServerStream = tokio::net::windows::named_pipe::NamedPipeServer;

#[cfg(unix)]
pub(crate) use unix::Listener;

#[cfg(windows)]
pub(crate) use win::Listener;

/// What a listener accepted: a stream, and whether its peer runs as the current user when
/// that is known at once (Unix); `None` when it is known only once the peer has written
/// something (Windows: [`Late`]).
pub(crate) struct Accepted {
    pub stream: ServerStream,
    pub same_user: Option<io::Result<bool>>,
}

/// Checks a connection's peer once it has written to it. Windows only (a pipe client can be
/// identified only once something was read from the pipe); on Unix the peer was checked when
/// it was accepted ([`Accepted::same_user`]), so this has nothing more to check. Taken
/// before the stream is split; valid while the stream lives.
pub(crate) struct Late {
    #[cfg(windows)]
    pipe: usize,
}

impl Late {
    pub(crate) fn of(stream: &ServerStream) -> Self {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            Self {
                pipe: stream.as_raw_handle() as usize,
            }
        }
        #[cfg(not(windows))]
        {
            let _ = stream;
            Self {}
        }
    }

    /// Whether the peer runs as the current user (call once the first line was read, with
    /// the stream still alive).
    pub(crate) fn same_user(&self) -> io::Result<bool> {
        #[cfg(windows)]
        {
            crate::windows::client_is_same_user(self.pipe as windows_sys::Win32::Foundation::HANDLE)
        }
        #[cfg(not(windows))]
        {
            Ok(true)
        }
    }
}

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    use super::Accepted;
    use crate::endpoint::{Endpoint, current_uid, peer_is_same_user};

    pub(crate) struct Listener {
        inner: tokio::net::UnixListener,
        path: PathBuf,
        /// The socket file we made (device, inode): only that one is removed at the end.
        made: (u64, u64),
    }

    impl Listener {
        /// Binds the socket. Must be called within the tokio runtime.
        pub(crate) fn bind(endpoint: &Endpoint) -> io::Result<Self> {
            let path = endpoint.path().to_path_buf();
            let dir = path.parent().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "socket path has no folder")
            })?;
            private_dir(dir)?;
            clear_stale(&path)?;
            let listener = std::os::unix::net::UnixListener::bind(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            let meta = fs::symlink_metadata(&path)?;
            listener.set_nonblocking(true)?;
            Ok(Self {
                inner: tokio::net::UnixListener::from_std(listener)?,
                made: (meta.dev(), meta.ino()),
                path,
            })
        }

        pub(crate) async fn accept(&mut self) -> io::Result<Accepted> {
            let (stream, _) = self.inner.accept().await?;
            let same_user = Some(peer_is_same_user(&stream));
            Ok(Accepted { stream, same_user })
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            // Only the socket we made: another instance may have taken the path since.
            if let Ok(meta) = fs::symlink_metadata(&self.path)
                && (meta.dev(), meta.ino()) == self.made
                && let Err(err) = fs::remove_file(&self.path)
            {
                tracing::warn!("cannot remove {}: {err}", self.path.display());
            }
        }
    }

    /// Makes sure `dir` is a folder of the current user's that only they can enter
    /// (creating it, 0700, if needed); not a symlink. Folders above it are created as usual
    /// if missing, and never changed.
    fn private_dir(dir: &Path) -> io::Result<()> {
        let meta = match fs::symlink_metadata(dir) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                if let Some(parent) = dir.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::DirBuilder::new().mode(0o700).create(dir)?;
                fs::symlink_metadata(dir)?
            }
            other => other?,
        };
        let refuse = |why: &str| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} {why}", dir.display()),
            ))
        };
        if meta.file_type().is_symlink() {
            return refuse("is a symbolic link");
        }
        if !meta.is_dir() {
            return refuse("is not a folder");
        }
        if meta.uid() != current_uid() {
            return refuse("belongs to another user");
        }
        if meta.mode() & 0o077 != 0 {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// Removes a socket left at `path` by an earlier run: only a socket of ours that nobody
    /// answers on (a running player keeps its endpoint).
    fn clear_stale(path: &Path) -> io::Result<()> {
        let meta = match fs::symlink_metadata(path) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            other => other?,
        };
        if !meta.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
        if meta.uid() != current_uid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} belongs to another user", path.display()),
            ));
        }
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("another player serves agents at {}", path.display()),
            )),
            Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => fs::remove_file(path),
            Err(err) => Err(err),
        }
    }
}

#[cfg(windows)]
mod win {
    use std::io;
    use std::path::PathBuf;

    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    use super::Accepted;
    use crate::endpoint::Endpoint;
    use crate::windows::OnlyCurrentUser;

    pub(crate) struct Listener {
        name: PathBuf,
        security: OnlyCurrentUser,
        /// The instance the next client connects to.
        next: NamedPipeServer,
    }

    impl Listener {
        /// Creates the first pipe instance (failing if anyone holds the name). Must be called
        /// within the tokio runtime.
        pub(crate) fn bind(endpoint: &Endpoint) -> io::Result<Self> {
            let name = endpoint.path().to_path_buf();
            let mut security = OnlyCurrentUser::new()?;
            let next = create(&name, &mut security, true)?;
            Ok(Self {
                name,
                security,
                next,
            })
        }

        pub(crate) async fn accept(&mut self) -> io::Result<Accepted> {
            let connected = self.next.connect().await;
            // A fresh instance waits for the next client, also when this one failed (a client
            // that came and went): a failed instance cannot be connected again.
            let new = create(&self.name, &mut self.security, false)?;
            let stream = std::mem::replace(&mut self.next, new);
            connected?;
            // Known once the client has written its first line (`Late`).
            Ok(Accepted {
                stream,
                same_user: None,
            })
        }
    }

    fn create(
        name: &PathBuf,
        security: &mut OnlyCurrentUser,
        first: bool,
    ) -> io::Result<NamedPipeServer> {
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first)
            .reject_remote_clients(true);
        // SAFETY: the attributes point at a security descriptor that lives as long as the
        // listener (and the call only reads them).
        unsafe { options.create_with_security_attributes_raw(name, security.as_ptr()) }
    }
}

#[cfg(not(any(unix, windows)))]
compile_error!("the agent MCP service needs Unix sockets or Windows named pipes");

/// Binds the endpoint (see [`Listener`]).
pub(crate) fn bind(endpoint: &Endpoint) -> io::Result<Listener> {
    Listener::bind(endpoint)
}

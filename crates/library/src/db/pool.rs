//! Pool of read-only connections. In WAL mode readers see the last committed state
//! and never wait for the writer.

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use rusqlite::{Connection, OpenFlags};

use crate::{Damage, LibraryError, Result};

/// Idle connections kept around; more are opened on demand and closed when returned.
const MAX_IDLE: usize = 4;

pub(crate) struct ReadPool {
    path: PathBuf,
    connections: Mutex<Connections>,
    returned: Condvar,
    closing: Arc<AtomicBool>,
    damage: Damage,
}

#[derive(Default)]
struct Connections {
    idle: Vec<Connection>,
    borrowed: usize,
}

impl ReadPool {
    pub fn new(path: &Path, damage: Damage, closing: Arc<AtomicBool>) -> Self {
        Self {
            path: path.to_owned(),
            connections: Mutex::default(),
            returned: Condvar::new(),
            closing,
            damage,
        }
    }

    /// Once admission has stopped, wait until even discarded connections have closed.
    pub fn close(&self) -> Result<()> {
        let connections = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        let mut connections = self
            .returned
            .wait_while(connections, |state| state.borrowed != 0)
            .unwrap_or_else(|e| e.into_inner());
        let idle = std::mem::take(&mut connections.idle);
        drop(connections);
        let mut result = Ok(());
        for conn in idle {
            if let Err((_, err)) = conn.close() {
                result = self.damage.watch(Err(err.into()));
            }
        }
        result
    }

    /// Runs `f` on a pooled connection. Every read goes through here (as every write goes
    /// through the writer thread), so a damaged database is always noted ([`Damage`]).
    pub fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let result = self.get().and_then(|conn| f(&conn));
        self.damage.watch(result)
    }

    #[cfg(all(test, feature = "desktop-fs"))]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A pooled connection for a test to read with directly (bypassing [`ReadPool::read`]).
    #[cfg(all(test, feature = "desktop-fs"))]
    pub fn connection(&self) -> Result<PooledConnection<'_>> {
        self.get()
    }

    fn get(&self) -> Result<PooledConnection<'_>> {
        if self.closing.load(Ordering::Acquire) {
            return Err(LibraryError::Closed);
        }
        let mut borrowed = {
            let mut connections = self.connections.lock().unwrap_or_else(|e| e.into_inner());
            if self.closing.load(Ordering::Acquire) {
                return Err(LibraryError::Closed);
            }
            connections.borrowed += 1;
            PooledConnection {
                pool: self,
                conn: connections.idle.pop(),
            }
        };
        if borrowed.conn.is_none() {
            borrowed.conn = Some(open_reader(&self.path)?);
        }
        Ok(borrowed)
    }
}

fn open_reader(path: &Path) -> Result<Connection> {
    let conn = super::with_busy_timeout(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ))?;
    tune(&conn)?;
    conn.set_prepared_statement_cache_capacity(32);
    Ok(conn)
}

/// Page cache and memory mapping for a 100k-track database (~150 MB with all tags): the
/// 2 MB default cache makes facets and tag queries several times slower. The mapping
/// shares the OS page cache, so it costs address space, not private memory; only on 64-bit
/// targets, since every pooled connection maps its own 256 MB (too much of a 32-bit
/// process's address space, e.g. armv7 Android).
pub(crate) fn tune(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "cache_size", -16_384)?;
    #[cfg(target_pointer_width = "64")]
    conn.pragma_update(None, "mmap_size", 256 * 1024 * 1024)?;
    Ok(())
}

pub(crate) struct PooledConnection<'a> {
    pool: &'a ReadPool,
    conn: Option<Connection>,
}

impl Deref for PooledConnection<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("connection present until drop")
    }
}

impl Drop for PooledConnection<'_> {
    fn drop(&mut self) {
        let mut connections = self
            .pool
            .connections
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(conn) = self.conn.take() {
            // Discard open read transactions and excess connections before notifying close.
            if conn.is_autocommit() && connections.idle.len() < MAX_IDLE {
                connections.idle.push(conn);
            }
        }
        connections.borrowed -= 1;
        if connections.borrowed == 0 && self.pool.closing.load(Ordering::Acquire) {
            self.pool.returned.notify_all();
        }
    }
}

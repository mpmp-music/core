//! SQLite schema, serialized writes, pooled reads, and fixed catalog queries.

use std::time::Duration;

use rusqlite::{Connection, ErrorCode};

use crate::LibraryError;

pub(crate) mod pool;
pub(crate) mod reads;
pub(crate) mod schema;
pub(crate) mod writer;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

fn with_busy_timeout(conn: rusqlite::Result<Connection>) -> rusqlite::Result<Connection> {
    let conn = conn?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}

pub(crate) fn open_error(error: LibraryError) -> LibraryError {
    match error {
        LibraryError::Db(error) if error.sqlite_error_code() == Some(ErrorCode::DatabaseBusy) => {
            LibraryError::DatabaseBusy(error)
        }
        error => error,
    }
}

//! Independently observable stages of a library shutdown.

/// A stage is pending until its result has been observed, including on timeout.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CloseStage {
    #[default]
    Pending,
    Succeeded,
    Failed(String),
}

impl CloseStage {
    pub(crate) fn from_result<E: std::fmt::Display>(result: &Result<(), E>) -> Self {
        match result {
            Ok(()) => Self::Succeeded,
            Err(error) => Self::Failed(error.to_string()),
        }
    }
}

/// A snapshot of the file-job drain, writer drain, and SQLite shutdown.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CloseProgress {
    pub file_jobs: CloseStage,
    pub writes: CloseStage,
    pub checkpoint: CloseStage,
    pub read_connections: CloseStage,
    pub write_connection: CloseStage,
}

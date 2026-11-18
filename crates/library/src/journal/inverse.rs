use super::*;

// ---- inverses ---

/// How to undo an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Inverse {
    /// Set each track's rating (`None`: unrated).
    Ratings(Vec<(i64, Option<u8>)>),
    /// Set each track's loved flag.
    Loved(Vec<(i64, bool)>),
    Playlist(PlaylistId, PlaylistOp),
    Root(RootId, RootOp),
    /// Write these values back into these files (a tag edit's undo, or redo): done by a
    /// tag job (`crate::tags::job`), never inside an undo's transaction.
    Tags(Vec<FileTags>),
    Covers(Vec<crate::cover::job::FileCover>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootOp {
    /// Soft-remove the root.
    Remove,
    /// Make a removed root active again.
    Restore,
}

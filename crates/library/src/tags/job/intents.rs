//! Prepared tag inverses are committed before an atomic same-directory publication.
//! The action, track rows and intent deletion share one transaction after publication.
use super::*;
use mpmp_storage::{
    LibraryStorage, LocalPath, ReplacementPaths, ReplacementTransaction, TagPublish,
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use tag_edit::snapshot::hex;

const FAMILY: &str = "tag_edit";
const VERSION: i64 = 4;

fn stable_by_default() -> bool {
    true
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Group {
    id: String,
    pub(super) undo_of: Option<ActionId>,
    origin: String,
    grant: Option<i64>,
    checked: bool,
    pub(super) at: i64,
    keys: Vec<(String, bool)>,
    #[serde(default)]
    pub(super) legacy: bool,
    #[serde(skip)]
    pub(super) expected: Option<crate::UndoDescription>,
}

impl Group {
    pub(super) fn new(
        meta: &OwnedMeta,
        undo_of: Option<ActionId>,
        changes: &[TagChange],
    ) -> Result<Self> {
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).map_err(|e| std::io::Error::other(e.to_string()))?;
        let meta = meta.meta();
        let keys = changes
            .iter()
            .map(|c| (c.key().to_owned(), matches!(c, TagChange::Set { .. })))
            .collect();
        Ok(Self {
            id: format!("{:032x}", u128::from_le_bytes(nonce)),
            undo_of,
            origin: meta.origin.to_owned(),
            grant: meta.grant,
            checked: meta.checked,
            at: now_ms(),
            keys,
            legacy: false,
            expected: None,
        })
    }

    pub(super) fn checked(&self) -> bool {
        self.checked
    }

    fn record(
        &self,
        tx: &Connection,
        files: Vec<FileTags>,
        changed: &HashSet<String>,
    ) -> Result<ActionId> {
        let meta = ActionMeta::new(&self.origin)
            .with_grant(self.grant)
            .with_checked(self.checked);
        if let Some(id) = self.undo_of {
            return journal::record_tag_undo(tx, id, meta, files, self.at, self.legacy);
        }
        let changes: Vec<_> = self
            .keys
            .iter()
            .map(|(key, set)| {
                if *set {
                    TagChange::Set {
                        key: key.clone(),
                        values: Vec::new(),
                    }
                } else {
                    TagChange::Remove { key: key.clone() }
                }
            })
            .collect();
        let details = changes
            .iter()
            .filter(|change| changed.contains(change.key()))
            .map(|change| {
                let operation = if matches!(change, TagChange::Set { .. }) {
                    "set"
                } else {
                    "remove"
                };
                serde_json::json!({"type":operation,"key":change.key()})
            })
            .collect();
        journal::record_tag_edit(
            tx,
            meta,
            summary(files.len(), &changes),
            files,
            serde_json::Value::Array(details),
            self.at,
        )
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Target {
    pub(super) path: PathBuf,
    pub(super) staged: PathBuf,
    pub(super) staged_identity: [u64; 2],
    pub(super) original_identity: [u64; 2],
    pub(super) track: TrackId,
    pub(super) root: RootId,
    pub(super) stamp: (u64, i64),
    pub(super) undo_of: Option<ActionId>,
    pub(super) changed: Vec<String>,
    #[serde(default)]
    pub(super) displaced: Option<PathBuf>,
    #[serde(default)]
    pub(super) exclusive: bool,
    #[serde(default)]
    pub(super) link_fallback: bool,
    #[serde(default)]
    pub(super) parent_identity: Option<[u64; 2]>,
    #[serde(default = "stable_by_default")]
    pub(super) stable_ids: bool,
    #[serde(default)]
    pub(super) charge: usize,
}

impl Target {
    fn validate(&self, auth: &Group) -> Result<()> {
        if !self.path.is_absolute()
            || self.staged.parent() != self.path.parent()
            || !self
                .staged
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with(".player-tags-") && name.ends_with(".tags.tmp")
                })
            || self.displaced.as_ref().is_some_and(|path| {
                path != &self.staged && path != &self.staged.with_extension("backup")
            })
            || self.staged == self.path
            || self.undo_of != auth.undo_of
        {
            return Err(LibraryError::Corrupt(
                "invalid tag write intent target".into(),
            ));
        }
        Ok(())
    }

    fn paths(&self) -> ReplacementPaths {
        replacement_paths(&self.path, &self.staged)
    }

    pub(super) fn protocol(&self) -> TagPublish {
        if self.link_fallback {
            TagPublish::Link
        } else if self.exclusive {
            TagPublish::Exclusive {
                stable_ids: self.stable_ids,
            }
        } else if self.displaced.as_ref() == Some(&self.staged) {
            TagPublish::Exchange
        } else {
            TagPublish::Replace {
                stable_ids: self.stable_ids,
            }
        }
    }

    pub(super) fn moved_id(&self, actual: [u64; 2], expected: [u64; 2]) -> bool {
        !self.stable_ids || actual == expected
    }

    pub(super) fn check_parent(&self, storage: &dyn LibraryStorage) -> std::io::Result<()> {
        if let Some(expected) = self.parent_identity
            && storage
                .directory_identity(self.path.parent().unwrap())?
                .parts()
                != expected
        {
            return Err(std::io::Error::other(
                "tag recovery directory identity changed",
            ));
        }
        Ok(())
    }
}

pub(super) fn replacement_paths(path: &Path, staged: &Path) -> ReplacementPaths {
    ReplacementPaths {
        destination: LocalPath(path.to_owned()),
        staged: Some(LocalPath(staged.to_owned())),
        backup: Some(LocalPath(staged.with_extension("backup"))),
        saved_after: None,
    }
}

fn json<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|e| LibraryError::Corrupt(e.to_string()))
}

fn parse<T: serde::de::DeserializeOwned>(value: &str) -> Result<T> {
    serde_json::from_str(value).map_err(|e| LibraryError::Corrupt(e.to_string()))
}

pub(super) fn register(inner: &Inner, group: &Group, target: &Target) -> Result<i64> {
    const INSERT: &str = r#"
INSERT INTO file_write_intents(family,format_version,authorization_json,target_json,
before_digest,before_image,after_digest,stage,created_at,updated_at)
VALUES(?1,?2,?3,?4,'',zeroblob(0),'','building',?5,?5)
"#;
    target.validate(group)?;
    let auth = json(group)?;
    let group_id = group.id.clone();
    let target = json(target)?;
    let expected = group.expected.clone();
    inner.writer.run(move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(expected) = expected {
            const PINNED: &str = "SELECT EXISTS(SELECT 1 FROM file_write_intents
              WHERE family=?1 AND stage<>'retained'
                AND json_extract(iif(json_valid(authorization_json), authorization_json, NULL),'$.id')=?2)";
            let pinned = tx.query_row(PINNED, params![FAMILY, group_id], |row| {
                row.get::<_, bool>(0)
            })?;
            if !pinned {
                journal::check_undo_description(&tx, &expected, now_ms())?;
            }
        }
        tx.execute(INSERT, params![FAMILY, VERSION, auth, target, now_ms()])?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    })
}

pub(super) fn budget(conn: &Connection, undo: bool) -> Result<usize> {
    const HISTORY: &str = "
SELECT coalesce(sum(length(inverse)),0) FROM actions
WHERE kind='tag_edit' OR EXISTS (
SELECT 1 FROM action_targets t WHERE t.action_id=actions.id AND t.kind='track_tags')";
    const PENDING: &str = "
SELECT coalesce(sum(coalesce(nullif(json_extract(iif(json_valid(target_json), target_json, NULL),'$.charge'),0),
                             length(before_image))),0)
FROM file_write_intents WHERE family=?1 AND length(before_image)>0";
    let pending = conn.query_row(PENDING, [FAMILY], |r| r.get::<_, i64>(0))? as usize;
    let available = tag_edit::MAX_TAG_INTENT_BYTES.saturating_sub(pending);
    if undo {
        // The old bidirectional image is released when this intent is journaled.
        // Development single-direction records are upgraded out of this reserve.
        return Ok(available);
    }
    let retained = conn.query_row(HISTORY, [], |r| r.get::<_, i64>(0))? as usize;
    Ok(available.min(tag_edit::MAX_TAG_HISTORY_BYTES.saturating_sub(retained + pending)))
}

pub(super) fn prepare(inner: &Inner, id: i64, target: &Target, reverse: &FileTags) -> Result<bool> {
    const UPDATE: &str = "
UPDATE file_write_intents SET before_digest=?2,before_image=?3,
after_digest=?4,target_json=?5,stage='prepared',updated_at=?6 WHERE id=?1";
    let exact = reverse.exact.as_ref().unwrap();
    let (before, after) = (hex(exact.before), hex(exact.after));
    let image = journal::Inverse::Tags(vec![reverse.clone()]).encode();
    let charge = target.charge.max(image.len());
    let undo = target.undo_of.is_some();
    let target = json(target)?;
    inner.writer.run(move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if budget(&tx, undo)? < charge {
            return Ok(false);
        }
        tx.execute(UPDATE, params![id, before, image, after, target, now_ms()])?;
        tx.commit()?;
        Ok(true)
    })
}

pub(super) fn identity(storage: &dyn LibraryStorage, path: &Path) -> std::io::Result<[u64; 2]> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_file() {
        return Err(std::io::Error::other("intent path is not a regular file"));
    }
    Ok(storage.entry_identity(path, &meta)?.parts())
}

pub(super) fn staged_present(storage: &dyn LibraryStorage, target: &Target) -> bool {
    identity(storage, &target.staged).ok() == Some(target.staged_identity)
}

pub(super) fn check_source(
    storage: &dyn LibraryStorage,
    original: &std::fs::File,
    target: &Target,
    candidate_stamp: (u64, i64),
    checked: bool,
) -> std::result::Result<(), Rejected> {
    if checked && storage_access::ensure_canonical(storage, &target.path).is_err() {
        return Err(Rejected::Code("location_changed"));
    }
    target.check_parent(storage)?;
    let meta = original.metadata()?;
    if stamp(&meta) != target.stamp
        || storage.opened_identity(original)?.parts() != target.original_identity
        || identity(storage, &target.path).ok() != Some(target.original_identity)
        || !staged_present(storage, target)
        || stamp(&std::fs::metadata(&target.staged)?) != candidate_stamp
    {
        return Err(Rejected::Changed);
    }
    if meta.permissions().readonly() {
        return Err(Rejected::Code("write_failed"));
    }
    Ok(())
}

fn delete(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM file_write_intents WHERE id=?1", [id])?;
    Ok(())
}

pub(super) fn mark(inner: &Inner, id: i64, stage: &'static str) -> Result<()> {
    inner.writer.run(move |conn| {
        conn.execute(
            "UPDATE file_write_intents SET stage=?2,updated_at=?3,format_version=?4 WHERE id=?1",
            params![id, stage, now_ms(), VERSION],
        )?;
        Ok(())
    })
}

pub(super) fn retain(inner: &Inner, id: i64, target: &Target) -> Result<()> {
    tracing::warn!(
        track = target.track.0, intent = id, staged = ?target.staged, path = ?target.displaced,
        "retaining tag recovery copies"
    );
    inner.writer.run(move |conn| {
        conn.execute(
            "UPDATE file_write_intents SET stage='retained',before_image=zeroblob(0),
             updated_at=?2,format_version=?3 WHERE id=?1",
            params![id, now_ms(), VERSION],
        )?;
        Ok(())
    })
}

pub(super) fn inspect_file(storage: &dyn LibraryStorage, path: &Path) -> Result<Observed> {
    let mut file = storage_access::open_regular(storage, path, true)?;
    inspect_open(storage, path, &mut file)
}

pub(super) fn inspect_open(
    storage: &dyn LibraryStorage,
    path: &Path,
    file: &mut std::fs::File,
) -> Result<Observed> {
    let id = storage.opened_identity(file)?.parts();
    let before = file.metadata()?;
    let hash = tag_edit::snapshot::digest(file)?;
    let after = file.metadata()?;
    let stamp = stamp(&after);
    if identity(storage, path)? != id || !same_metadata(&before, &after) {
        return Err(std::io::Error::other("tag replacement changed during verification").into());
    }
    Ok(Observed {
        id,
        hash,
        stamp,
        meta: after,
    })
}

pub(super) fn same_metadata(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (before.ctime(), before.ctime_nsec()) != (after.ctime(), after.ctime_nsec()) {
            return false;
        }
    }
    before.len() == after.len()
        && before
            .modified()
            .ok()
            .is_some_and(|time| after.modified().ok() == Some(time))
}

pub(super) struct Observed {
    pub id: [u64; 2],
    pub hash: [u8; 32],
    pub stamp: (u64, i64),
    meta: std::fs::Metadata,
}

pub(super) fn unchanged(storage: &dyn LibraryStorage, path: &Path, observed: &Observed) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| {
        meta.is_file()
            && same_metadata(&observed.meta, &meta)
            && storage
                .entry_identity(path, &meta)
                .ok()
                .map(|id| id.parts())
                == Some(observed.id)
    })
}

pub(super) struct Published {
    pub intent: i64,
    pub file: FileTags,
    pub record: Option<Box<TrackRecord>>,
    pub retain: bool,
}

pub(super) fn finish(
    storage: &dyn LibraryStorage,
    conn: &mut Connection,
    group: &Group,
    published: Vec<Published>,
    changed: &HashSet<String>,
) -> Result<ActionId> {
    let mut records = Vec::new();
    let mut pending = Vec::new();
    let mut files = Vec::new();
    let mut ids = Vec::new();
    for Published {
        intent,
        file,
        record,
        retain,
    } in published
    {
        ids.push((intent, retain));
        if let Some(record) = record {
            records.push(*record);
        } else {
            pending.push(TrackId(file.track));
        }
        files.push(file);
    }
    commit(storage, conn, &records, &pending, |tx, _| {
        // The original undo is pinned by these rows until this transaction. Delete
        // the rows after recording it so journal pruning sees that pin.
        let action = group.record(tx, files, changed)?;
        for (id, retain) in ids {
            if retain {
                tx.execute(
                    "UPDATE file_write_intents SET stage='retained',before_image=zeroblob(0),
                     format_version=?2,action_id=?3 WHERE id=?1",
                    params![id, VERSION, action.0],
                )?;
            } else {
                delete(tx, id)?;
            }
        }
        Ok(action)
    })
}

pub(super) enum Point {
    Building,
    Prepared,
    Publishing,
    Displacing,
    DisplaceGap,
    Installing,
    RollbackGap,
    Sync,
    AfterSyscall,
    AfterRename,
    Replaced,
    BeforeAction,
    AfterAction,
}

#[cfg(all(test, feature = "desktop-fs"))]
impl Point {
    fn name(&self) -> &'static str {
        match self {
            Self::Building => "building",
            Self::Sync => "sync_failed",
            Self::Prepared => "prepared",
            Self::Publishing => "publishing",
            Self::Displacing => "displacing",
            Self::DisplaceGap => "displace_gap",
            Self::Installing => "installing",
            Self::RollbackGap => "rollback_gap",
            Self::AfterSyscall => "after_syscall",
            Self::Replaced => "replaced",
            Self::AfterRename => "after_rename",
            Self::BeforeAction => "before_action",
            Self::AfterAction => "after_action",
        }
    }
}

pub(super) fn fault(inner: &Inner, point: Point) -> Result<()> {
    #[cfg(all(test, feature = "desktop-fs"))]
    if *inner.hooks.tag_fault.lock().unwrap() == Some(point.name()) {
        return Err(LibraryError::Corrupt(
            "injected tag write interruption".into(),
        ));
    }
    let _ = (inner, point);
    Ok(())
}

struct Intent {
    id: i64,
    group: Group,
    target: Target,
    stage: String,
    before: String,
    after: String,
    action: Option<i64>,
}

#[derive(Default)]
struct Loaded {
    rows: Vec<Intent>,
    unreadable: Vec<Unreadable>,
}

struct Unreadable {
    id: i64,
    error: String,
    retained: bool,
}

impl Loaded {
    fn blocks_metadata_writes(&self) -> bool {
        self.unreadable.iter().any(|row| !row.retained)
            || self.rows.iter().any(|row| row.stage == "unreadable")
    }
}

fn load(conn: &Connection, include_retained: bool) -> Result<Loaded> {
    let mut statement = conn.prepare(
        "SELECT id,authorization_json,target_json,stage,before_digest,after_digest,format_version,
                action_id
         FROM file_write_intents WHERE family=?1 AND (?2 OR stage<>'retained') ORDER BY id",
    )?;
    let mut rows = statement.query(params![FAMILY, include_retained])?;
    let mut loaded = Loaded::default();
    while let Some(r) = rows.next()? {
        let id: i64 = r.get(0)?;
        let result = (|| -> Result<Intent> {
            let version: i64 = r.get(6)?;
            let stage: String = r.get(3)?;
            if !(1..=VERSION).contains(&version)
                || (version == 1 && !matches!(stage.as_str(), "prepared" | "replaced" | "retained"))
                || !matches!(
                    stage.as_str(),
                    "building"
                        | "publishing"
                        | "prepared"
                        | "replaced"
                        | "retained"
                        | "rolling_back"
                        | "verified_external"
                        | "displacing"
                        | "displaced"
                        | "installing"
                        | "rollback_installing"
                        | "unreadable"
                )
            {
                return Err(LibraryError::Corrupt(
                    "unknown tag intent version or stage".into(),
                ));
            }
            let group = parse(&r.get::<_, String>(1)?)?;
            let target: Target = parse(&r.get::<_, String>(2)?)?;
            target.validate(&group)?;
            Ok(Intent {
                id,
                group,
                target,
                stage,
                before: r.get(4)?,
                after: r.get(5)?,
                action: r.get(7)?,
            })
        })();
        match result {
            Ok(row) => loaded.rows.push(row),
            Err(error) => loaded.unreadable.push(Unreadable {
                id,
                error: error.to_string(),
                retained: r.get_ref(3)?.as_str().ok() == Some("retained"),
            }),
        }
    }
    Ok(loaded)
}

pub(crate) fn validate(conn: &Connection) -> Result<()> {
    let loaded = load(conn, true)?;
    if let Some(row) = loaded.unreadable.first() {
        return Err(LibraryError::Corrupt(format!(
            "tag intent {}: {}",
            row.id, row.error
        )));
    }
    for intent in loaded.rows {
        if intent.stage != "building" && intent.stage != "retained" {
            intent.inverse(conn)?;
        }
    }
    Ok(())
}

impl Intent {
    fn inverse(&self, conn: &Connection) -> Result<FileTags> {
        let data: Option<Vec<u8>> = conn.query_row(
            "SELECT CASE WHEN typeof(before_image)='blob' AND length(before_image)<=?2
                    THEN before_image END FROM file_write_intents WHERE id=?1",
            params![self.id, tag_edit::MAX_TAG_INTENT_BYTES as i64],
            |r| r.get(0),
        )?;
        if let Some(journal::Inverse::Tags(mut files)) =
            data.as_deref().and_then(journal::Inverse::decode)
            && files.len() == 1
        {
            let file = files.pop().unwrap();
            if file.track == self.target.track.0
                && file.exact.as_ref().is_some_and(|exact| {
                    hex(exact.before) == self.before && hex(exact.after) == self.after
                })
            {
                return Ok(file);
            }
        }
        Err(LibraryError::Corrupt(
            "invalid tag write intent inverse".into(),
        ))
    }
}

/// Read and hash independently of the SQLite writer so one slow root cannot
/// block unrelated database commands. Filesystem errors leave that intent pending.
pub(crate) fn recover_inner(inner: &Inner) -> Result<()> {
    if !inner.storage.files.write_capabilities().legacy_tag_write
        || !inner.storage.replacements.provides_tag_publish()
    {
        return Ok(());
    }
    let Loaded {
        mut rows,
        unreadable,
    } = inner.writer.run(|conn| load(conn, true))?;
    for Unreadable { id, error, .. } in unreadable {
        tracing::warn!(intent = id, %error, "unreadable tag intent left untouched");
    }
    rows.sort_by_key(|row| (row.group.at, row.group.id.clone(), row.id));
    for rows in rows.chunk_by(|a, b| a.group.id == b.group.id) {
        let mut published = Vec::new();
        let mut changed = HashSet::new();
        let mut leases = Vec::new();
        for row in rows {
            let Some(lease) = inner.file_access.acquire(row.target.root) else {
                continue;
            };
            leases.push(lease);
            let result = reconcile(inner, row);
            match result {
                Ok(Some(ready)) => {
                    changed.extend(row.target.changed.iter().cloned());
                    published.push(ready);
                }
                Ok(None) => {}
                Err(error @ LibraryError::Corrupt(_)) => {
                    tracing::warn!(%error, intent = row.id, "unreadable tag inverse retained");
                    mark(inner, row.id, "unreadable")?;
                }
                Err(error) => tracing::warn!(
                    %error, track = row.target.track.0, intent = row.id,
                    "tag recovery still pending"
                ),
            }
        }
        if !published.is_empty() {
            let storage = inner.storage.files.clone();
            // Move only the authorization facts into the database closure.
            let group = parse::<Group>(&json(&rows[0].group)?)?;
            if let Err(error) = inner
                .writer
                .run_data(move |conn| finish(storage.as_ref(), conn, &group, published, &changed))
            {
                tracing::warn!(%error, "cannot finalize recovered tags");
            }
        }
    }
    Ok(())
}

fn reconcile(inner: &Inner, row: &Intent) -> Result<Option<Published>> {
    let target = &row.target;
    if row.stage == "unreadable" {
        return Ok(None);
    }
    target.check_parent(inner.storage.files.as_ref())?;
    if row.stage == "retained" {
        if target.parent_identity.is_some()
            && absent(&target.staged)
            && target.displaced.as_deref().is_none_or(absent)
        {
            let id = row.id;
            inner.writer.run(move |conn| delete(conn, id))?;
        }
        return Ok(None);
    }
    if row.stage == "building" {
        clean_unpublished(inner, row.id, target, None)?;
        return Ok(None);
    }
    let tx = inner.storage.replacements.resume(target.paths())?;
    restore_public_name(inner, target, tx.as_ref(), &row.stage, |hash| {
        hex(hash) == row.after
    })?;
    let mut file = inner.pool.read(|conn| row.inverse(conn))?;
    let exact = file.exact.as_ref().unwrap();
    if row.stage == "prepared" && target.displaced.is_some() {
        clean_unpublished(inner, row.id, target, Some(exact.after))?;
        return Ok(None);
    }
    // Version 1 used prepared for both sides of the replacement. Positive
    // evidence of a rename is still enough to recover these older rows.
    let settled = settle(
        inner,
        row.id,
        target,
        tx.as_ref(),
        exact,
        row.stage.as_str(),
        None,
    )?;
    let Resolution::Complete { current, retain } = settled else {
        return Ok(None);
    };
    let record = if let Some(stamp) = current {
        (file.size, file.mtime_ms) = stamp;
        let refs = inner.pool.read(|conn| refs_from(conn, &[target.track]))?;
        refs.first()
            .and_then(|file| read_record(file, &target.path, stamp))
    } else {
        None
    };
    Ok(Some(Published {
        intent: row.id,
        file,
        record,
        retain,
    }))
}

fn observe(storage: &dyn LibraryStorage, path: &Path) -> Result<Option<Observed>> {
    match inspect_file(storage, path) {
        Ok(value) => Ok(Some(value)),
        Err(LibraryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            // Do not interpret an unmounted/inaccessible directory as a missing file.
            std::fs::metadata(path.parent().unwrap())?;
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn observe_copy(
    storage: &dyn LibraryStorage,
    path: &Path,
) -> Result<Option<(Observed, std::fs::File)>> {
    // Check for a missing name before requesting a canonical, guarded handle.
    // The retained handle denies writes on Windows through verification and unlink.
    let mut file = match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(_) => storage_access::open_tag_source(storage, path, true)?,
    };
    let observed = inspect_open(storage, path, &mut file)?;
    Ok(Some((observed, file)))
}

fn absent(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

fn remove_verified(
    storage: &dyn LibraryStorage,
    target: &Target,
    path: &Path,
    observed: &Observed,
) -> bool {
    if target.check_parent(storage).is_err() || !unchanged(storage, path, observed) {
        return false;
    }
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "tag copy cleanup deferred");
            false
        }
    }
}

pub(super) fn clean_unpublished(
    inner: &Inner,
    id: i64,
    target: &Target,
    after: Option<[u8; 32]>,
) -> Result<()> {
    let storage = inner.storage.files.as_ref();
    if remove_unpublished(storage, target, after)? {
        inner.writer.run(move |conn| delete(conn, id))
    } else {
        retain(inner, id, target)
    }
}

pub(super) fn remove_unpublished(
    storage: &dyn LibraryStorage,
    target: &Target,
    after: Option<[u8; 32]>,
) -> Result<bool> {
    target.check_parent(storage)?;
    if let Some((candidate, _guard)) = observe_copy(storage, &target.staged)? {
        let ours = candidate.id == target.staged_identity
            && (after == Some(candidate.hash) || candidate.stamp.0 == 0);
        return Ok(ours && remove_verified(storage, target, &target.staged, &candidate));
    }
    Ok(true)
}

pub(super) enum Resolution {
    Complete {
        current: Option<(u64, i64)>,
        retain: bool,
    },
    Changed,
    Unverified,
}

fn restore_public_name(
    inner: &Inner,
    target: &Target,
    tx: &dyn ReplacementTransaction,
    stage: &str,
    matches_after: impl FnOnce([u8; 32]) -> bool,
) -> Result<()> {
    if !absent(&target.path) {
        return Ok(());
    }
    match stage {
        // Installation could not have started. Do not depend on the candidate or inverse.
        "displacing" | "displaced" => recover_missing(inner, target, tx, false),
        "installing" if target.exclusive => {
            // A missing candidate could mean installation succeeded and an external writer
            // subsequently deleted the public name. Restore only with the candidate in place.
            let storage = inner.storage.files.as_ref();
            if let Some((candidate, _guard)) = observe_copy(storage, &target.staged)? {
                #[cfg(unix)]
                if target.link_fallback {
                    use std::os::unix::fs::MetadataExt;
                    if candidate.meta.nlink() != 1 {
                        return Ok(());
                    }
                }
                if candidate.id == target.staged_identity
                    && matches_after(candidate.hash)
                    && unchanged(storage, &target.staged, &candidate)
                {
                    recover_missing(inner, target, tx, false)?;
                }
            }
            Ok(())
        }
        "rolling_back"
            if target.exclusive
                && std::fs::symlink_metadata(&target.staged).is_ok_and(|m| m.is_file()) =>
        {
            recover_missing(inner, target, tx, false)
        }
        _ => Ok(()),
    }
}

pub(super) fn recover_missing(
    inner: &Inner,
    target: &Target,
    tx: &dyn ReplacementTransaction,
    rolling_back: bool,
) -> Result<()> {
    let storage = inner.storage.files.as_ref();
    target.check_parent(storage)?;
    if std::fs::symlink_metadata(&target.path)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        && let Some(displaced) = target.displaced.as_ref()
    {
        let alternate = (rolling_back && displaced != &target.staged).then_some(&target.staged);
        for path in std::iter::once(displaced).chain(alternate) {
            if std::fs::symlink_metadata(path).is_ok_and(|meta| {
                // Never publish the empty private name reserved by move_to_private.
                meta.is_file()
                    && (!target.link_fallback
                        || meta.len() != 0
                        || storage
                            .entry_identity(path, &meta)
                            .is_ok_and(|id| id.parts() == target.original_identity))
            }) && (target.protocol() != TagPublish::Exchange
                || identity(storage, path)? != target.staged_identity)
            {
                // Exclusive restoration refuses a new competing destination. A system
                // replacement can fail after displacing a third party's file.
                let _namespace = inner.changing_file_names();
                tx.restore_tag(path, &target.path, target.protocol())?;
                tx.sync(&target.path)?;
                break;
            }
        }
    }
    Ok(())
}

fn rollback(
    inner: &Inner,
    id: i64,
    target: &Target,
    tx: &dyn ReplacementTransaction,
    current: &Observed,
    previous: &Observed,
) -> Result<Resolution> {
    let storage = inner.storage.files.as_ref();
    let displaced = target.displaced.as_ref().unwrap();
    mark(inner, id, "rolling_back")?;
    if !unchanged(storage, &target.path, current) || !unchanged(storage, displaced, previous) {
        retain(inner, id, target)?;
        return Ok(Resolution::Changed);
    }
    #[cfg(all(test, feature = "desktop-fs"))]
    if let Some(hook) = inner.hooks.tag_before_rollback.lock().unwrap().as_ref() {
        hook(&target.path, displaced);
    }
    // Recheck both names immediately after the last point where work can block.
    if !unchanged(storage, &target.path, current) || !unchanged(storage, displaced, previous) {
        retain(inner, id, target)?;
        return Ok(Resolution::Changed);
    }
    target.check_parent(storage)?;
    let _namespace = inner.changing_file_names();
    let restored = if target.exclusive {
        tx.displace_tag(&target.path, &target.staged, target.protocol())?;
        fault(inner, Point::RollbackGap)?;
        mark(inner, id, "rollback_installing")?;
        tx.restore_tag(displaced, &target.path, target.protocol())
    } else {
        tx.rollback_tag(target.protocol())
    };
    if let Err(error) = restored {
        if !target.exclusive {
            recover_missing(inner, target, tx, true)?;
        }
        return Err(error.into());
    }
    let restored = observe(storage, &target.path)?;
    let rejected = observe_copy(storage, &target.staged)?;
    if let (Some(restored), Some((rejected, _guard))) = (restored, rejected)
        && restored.hash == previous.hash
        && target.moved_id(restored.id, previous.id)
        && rejected.hash == current.hash
        && target.moved_id(rejected.id, current.id)
        && unchanged(storage, &target.path, &restored)
    {
        tx.sync(&target.path)?;
        if unchanged(storage, &target.path, &restored)
            && remove_verified(storage, target, &target.staged, &rejected)
            && (displaced == &target.staged || absent(displaced))
        {
            // Persist the removal before forgetting the intent.
            tx.sync(&target.path)?;
            inner.writer.run(move |conn| delete(conn, id))?;
            return Ok(Resolution::Changed);
        }
    }
    retain(inner, id, target)?;
    Ok(Resolution::Changed)
}

pub(super) fn settle(
    inner: &Inner,
    id: i64,
    target: &Target,
    tx: &dyn ReplacementTransaction,
    exact: &tag_edit::snapshot::ByteUndo,
    stage: &str,
    published_source: Option<std::fs::File>,
) -> Result<Resolution> {
    let live = published_source.is_some();
    let storage = inner.storage.files.as_ref();
    target.check_parent(storage)?;
    restore_public_name(inner, target, tx, stage, |hash| hash == exact.after)?;
    if matches!(stage, "rolling_back" | "rollback_installing") {
        if target
            .displaced
            .as_ref()
            .is_some_and(|path| path == &target.staged || absent(path))
            && let Some(restored) = observe(storage, &target.path)?
            && restored.hash == exact.before
            && target.moved_id(restored.id, target.original_identity)
            && let Some((rejected, _guard)) = observe_copy(storage, &target.staged)?
            && rejected.hash == exact.after
            && target.moved_id(rejected.id, target.staged_identity)
            && unchanged(storage, &target.path, &restored)
            && remove_verified(storage, target, &target.staged, &rejected)
        {
            tx.sync(&target.path)?;
            inner.writer.run(move |conn| delete(conn, id))?;
            return Ok(Resolution::Changed);
        }
        retain(inner, id, target)?;
        return Ok(Resolution::Changed);
    }
    let current = observe(storage, &target.path)?;
    let previous = target
        .displaced
        .as_ref()
        .map(|path| observe_copy(storage, path))
        .transpose()?
        .flatten();
    let (previous, previous_guard) = match previous {
        Some((file, guard)) => (Some(file), Some(guard)),
        None => (None, None),
    };
    let exchange = target.displaced.as_ref() == Some(&target.staged);
    let staged = if exchange {
        None
    } else {
        observe_copy(storage, &target.staged)?
    };
    let staged_identity = if exchange {
        previous.as_ref().map(|file| (file.id, file.hash))
    } else {
        staged.as_ref().map(|(file, _)| (file.id, file.hash))
    };
    let correct_current = current.as_ref().filter(|file| {
        file.hash == exact.after && target.moved_id(file.id, target.staged_identity)
    });
    let correct_previous = previous.as_ref().filter(|file| {
        file.hash == exact.before && target.moved_id(file.id, target.original_identity)
    });
    let previously_verified = stage == "replaced";
    let published_exact = correct_current.is_some()
        && (correct_previous.is_some() || target.displaced.is_none())
        && (exchange
            || staged_identity.is_none()
            || (target.link_fallback
                && staged_identity == correct_current.map(|file| (file.id, file.hash))));
    if !previously_verified && !published_exact {
        if let (Some(current), Some(previous)) = (correct_current, previous.as_ref())
            && correct_previous.is_none()
            && !(previous.id == target.staged_identity && target.stable_ids)
        {
            // ReplaceFileW needs write sharing when the backup becomes a candidate again.
            drop((published_source, previous_guard, staged));
            return rollback(inner, id, target, tx, current, previous);
        }
        let unchanged_source = current.as_ref().is_some_and(|file| {
            file.hash == exact.before && target.moved_id(file.id, target.original_identity)
        });
        let fallback_unpublished =
            target.exclusive && matches!(stage, "displacing" | "displaced" | "installing");
        let removed_reservation = if stage == "displacing"
            && target.link_fallback
            && let (Some(current), Some(previous), Some(path)) =
                (&current, &previous, &target.displaced)
        {
            unchanged_source
                && previous.stamp.0 == 0
                && previous.id != current.id
                && unchanged(storage, &target.path, current)
                && remove_verified(storage, target, path, previous)
        } else {
            false
        };
        let no_displacement = previous.is_none()
            || exchange && staged_identity == Some((target.staged_identity, exact.after))
            || removed_reservation;
        if no_displacement && (unchanged_source || staged_identity.is_none()) {
            if fallback_unpublished && unchanged_source {
                let removed = remove_unpublished(storage, target, Some(exact.after))?;
                tx.sync(&target.path)?;
                if removed {
                    inner.writer.run(move |conn| delete(conn, id))?;
                } else {
                    retain(inner, id, target)?;
                }
            } else {
                clean_unpublished(inner, id, target, Some(exact.after))?;
            }
        } else {
            retain(inner, id, target)?;
            return Ok(Resolution::Unverified);
        }
        return Ok(Resolution::Changed);
    }

    if let Some(current) = correct_current {
        if !unchanged(storage, &target.path, current) {
            return Err(std::io::Error::other("tag result changed during verification").into());
        }
        if live {
            fault(inner, Point::Sync)?;
        }
        // The Windows backup has a deny-write handle. The installed file was
        // already staged durably and can be opened for a writable flush.
        #[cfg(not(windows))]
        if let (Some(path), Some(_)) = (&target.displaced, &previous) {
            tx.sync(path)?;
        }
        tx.sync(&target.path)?;
    }
    if !previously_verified {
        mark(inner, id, "replaced")?;
    }
    if live {
        fault(inner, Point::AfterRename)?;
        fault(inner, Point::Replaced)?;
    }
    #[cfg(all(test, feature = "desktop-fs"))]
    if live
        && let Some(path) = &target.displaced
        && let Some(hook) = inner.hooks.tag_before_cleanup.lock().unwrap().as_ref()
    {
        hook(path);
    }
    let current_unchanged =
        correct_current.is_some_and(|file| unchanged(storage, &target.path, file));
    let mut removed = false;
    let mut retain = false;
    if let Some(path) = &target.displaced {
        if previous.is_some() {
            let cleaned = current_unchanged
                && correct_previous
                    .is_some_and(|file| remove_verified(storage, target, path, file));
            removed |= cleaned;
            retain |= !cleaned;
        } else {
            retain |= !absent(path);
        }
    }
    if !exchange && !absent(&target.staged) {
        let cleaned = current_unchanged
            && target.link_fallback
            && staged.as_ref().is_some_and(|(file, _)| {
                file.hash == exact.after
                    && correct_current.is_some_and(|current| current.id == file.id)
                    && remove_verified(storage, target, &target.staged, file)
            });
        removed |= cleaned;
        retain |= !cleaned;
    }
    drop((published_source, previous_guard, staged));
    if removed {
        tx.sync(&target.path)?;
    }
    Ok(Resolution::Complete {
        current: current
            .filter(|file| file.hash == exact.after)
            .map(|file| file.stamp),
        retain,
    })
}

pub(crate) struct PendingTag {
    pub track: TrackId,
    pub path: PathBuf,
    pub identity_before: Option<[u64; 2]>,
    pub identity_after: [u64; 2],
}

const UNREADABLE_GUIDANCE: &str = concat!(
    "Tag and cover writes are blocked, and scans cannot mark files missing. ",
    "Keep the database and music directories intact for recovery.",
);

/// Paths are trusted-host diagnostics and are never sent to agents or theme queries.
pub(crate) fn diagnostics(conn: &Connection) -> Result<Vec<serde_json::Value>> {
    let loaded = load(conn, true)?;
    let mut items: Vec<_> = loaded.rows.into_iter().map(|row| serde_json::json!({
        "intent": row.id,
        "track": row.target.track.0,
        "stage": row.stage,
        "action": row.action,
        "target": row.target.path,
        "staged": row.target.staged,
        "displaced": row.target.displaced,
        "blocks_metadata_writes": row.stage == "unreadable",
        "guidance": if row.stage == "retained" {
            "Close other editors first. Copy any retained file to a safe location, then compare or recover its contents. Delete a retained file only once you have verified it is no longer needed."
        } else if row.stage == "unreadable" {
            UNREADABLE_GUIDANCE
        } else {
            "Keep all listed files. If the original directory was moved, locate the copies there before changing any files."
        },
    })).collect();
    items.extend(loaded.unreadable.into_iter().map(|row| {
        serde_json::json!({
            "intent": row.id, "stage": "unreadable", "error": row.error,
            "blocks_metadata_writes": !row.retained,
            "guidance": if row.retained {
                "Keep the database and music directories for recovery."
            } else {
                UNREADABLE_GUIDANCE
            },
        })
    }));
    for id in crate::cover::job::unreadable_recovery_rows(conn)? {
        items.push(serde_json::json!({
            "intent": id, "family": "cover_edit", "stage": "unreadable",
            "guidance": "Keep the database and music directories for recovery.",
        }));
    }
    let mut other = conn.prepare(
        "SELECT id FROM file_write_intents WHERE family NOT IN ('tag_edit','cover_edit')",
    )?;
    for id in other.query_map([], |row| row.get::<_, i64>(0))? {
        items.push(serde_json::json!({
            "intent": id?, "stage": "unreadable",
            "guidance": "Unknown file-write intent family. Keep the database unchanged.",
        }));
    }
    Ok(items)
}

/// Probe files only occur in indexed directories. Clean stale, empty sentinels
/// away from startup and without touching tag candidates.
pub(crate) fn clean_interrupted_probes(inner: &Arc<Inner>) {
    if !inner.storage.replacements.provides_tag_publish() {
        return;
    }
    let dirs = inner.writer.run(|conn| {
        let mut statement = conn.prepare(
            "SELECT roots.id,roots.path,tracks.rel_path FROM roots
             LEFT JOIN tracks ON tracks.root_id=roots.id WHERE roots.removed=0",
        )?;
        let mut directories = HashSet::new();
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let root = RootId(row.get(0)?);
            let path = PathBuf::from(row.get::<_, String>(1)?);
            let relative: Option<String> = row.get(2)?;
            let parent = relative
                .as_deref()
                .and_then(|relative| Path::new(relative).parent());
            directories.insert((root, parent.map_or_else(|| path.clone(), |p| path.join(p))));
        }
        Ok(directories)
    });
    let Ok(dirs) = dirs else { return };
    if dirs.is_empty() {
        return;
    }
    let weak = Arc::downgrade(inner);
    let replacements = inner.storage.replacements.clone();
    let access = inner.file_access.clone();
    let started = std::thread::Builder::new()
        .name("tag-probe-clean".into())
        .spawn(move || {
            for (root, path) in dirs {
                if weak.strong_count() == 0 {
                    break;
                }
                if let Some(_lease) = access.acquire(root)
                    && let Err(error) = replacements.clean_tag_probes(&path)
                {
                    tracing::debug!(%error, path = %path.display(), "tag probe cleanup deferred");
                }
            }
        });
    if let Err(error) = started {
        tracing::warn!(%error, "cannot start tag probe cleanup");
    }
}

pub(crate) fn pending_targets(conn: &Connection) -> Result<(bool, Vec<PendingTag>)> {
    let loaded = load(conn, false)?;
    let blocks_missing = loaded.blocks_metadata_writes();
    let targets = loaded
        .rows
        .into_iter()
        .map(|row| {
            let target = row.target;
            PendingTag {
                track: target.track,
                path: target.path,
                identity_before: (row.stage != "replaced").then_some(target.original_identity),
                identity_after: target.staged_identity,
            }
        })
        .collect();
    Ok((blocks_missing, targets))
}

/// Check all tag headers once per metadata operation. A row without a trustworthy
/// target cannot be scoped to a particular track.
pub(crate) fn require_readable(conn: &Connection) -> Result<()> {
    if blocks_metadata_writes(conn)? {
        Err(LibraryError::CoverRecoveryRequired)
    } else {
        Ok(())
    }
}

pub(crate) fn blocks_metadata_writes(conn: &Connection) -> Result<bool> {
    Ok(load(conn, false)?.blocks_metadata_writes())
}

pub(crate) fn pending(inner: &Inner, track: TrackId, path: &Path) -> Result<bool> {
    const MATCH: &str = "
SELECT EXISTS(SELECT 1 FROM file_write_intents WHERE family=?1 AND stage<>'retained'
AND (stage='unreadable' OR NOT json_valid(target_json)
OR json_extract(iif(json_valid(target_json), target_json, NULL),'$.track')=?2 OR json_extract(iif(json_valid(target_json), target_json, NULL),'$.path')=?3
OR json_extract(iif(json_valid(target_json), target_json, NULL),'$.staged_identity')=?4
OR (stage<>'replaced' AND json_extract(iif(json_valid(target_json), target_json, NULL),'$.original_identity')=?4)))";
    let storage = inner.storage.files.as_ref();
    inner.pool.read(|conn| {
        let present = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM file_write_intents WHERE family=?1 AND stage<>'retained')",
            [FAMILY],
            |row| row.get::<_, bool>(0),
        )?;
        if !present {
            return Ok(false);
        }
        let id = identity(storage, path)
            .ok()
            .map(|id| json(&id))
            .transpose()?;
        Ok(conn.query_row(
            MATCH,
            params![FAMILY, track.0, path.to_string_lossy(), id],
            |row| row.get(0),
        )?)
    })
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests;

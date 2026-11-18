-- Historical migration 1

CREATE TABLE roots (
    id              INTEGER PRIMARY KEY,
    path            TEXT    NOT NULL UNIQUE,
    forced_encoding TEXT,
    added_at        INTEGER NOT NULL,
    last_scan_at    INTEGER,
    -- Removal is soft (§6.1): the root and its tracks stay, so re-adding the folder brings
    -- back the same track ids and the play history stays linked.
    removed         INTEGER NOT NULL DEFAULT 0
);

-- AUTOINCREMENT: ids of deleted rows are never handed out again, so a TrackId stays unique
-- for the life of the database.
CREATE TABLE tracks (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    root_id           INTEGER NOT NULL REFERENCES roots(id) ON DELETE CASCADE,
    rel_path          TEXT    NOT NULL,
    size              INTEGER NOT NULL,
    mtime_ms          INTEGER NOT NULL,
    available         INTEGER NOT NULL DEFAULT 1,

    title             TEXT,
    artist            TEXT,
    album             TEXT,
    album_artist      TEXT,
    genre             TEXT,
    year              INTEGER,
    track_no          INTEGER,
    track_total       INTEGER,
    disc_no           INTEGER,
    disc_total        INTEGER,
    compilation       INTEGER NOT NULL DEFAULT 0,

    duration_ms       INTEGER,
    sample_rate       INTEGER,
    channels          INTEGER,
    bits_per_sample   INTEGER,
    bitrate_kbps      INTEGER,
    codec             TEXT,
    lossless          INTEGER,
    rg_track_gain     REAL,
    rg_track_peak     REAL,
    rg_album_gain     REAL,
    rg_album_peak     REAL,
    has_embedded_art  INTEGER NOT NULL DEFAULT 0,
    -- A cover image file (cover/folder/front.jpg|jpeg|png) sits next to the track.
    has_folder_art    INTEGER NOT NULL DEFAULT 0,
    tag_encoding      TEXT,

    -- Normalized copies for search (NFKC + lower case). title_norm falls back to the file
    -- name so untagged files can be found.
    title_norm        TEXT    NOT NULL DEFAULT '',
    artist_norm       TEXT    NOT NULL DEFAULT '',
    album_norm        TEXT    NOT NULL DEFAULT '',
    album_artist_norm TEXT    NOT NULL DEFAULT '',
    genre_norm        TEXT    NOT NULL DEFAULT '',
    -- Grouping: the AlbumKey string (tracks without an album tag form one "unknown album"
    -- per artist), the album artist shown for the album (album artist, "Various Artists"
    -- for untagged compilations, or the track artist) and its normalized form, which keys
    -- the artist view.
    album_key         TEXT    NOT NULL,
    album_group_artist TEXT,
    group_artist_norm TEXT    NOT NULL DEFAULT '',

    added_at          INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    UNIQUE (root_id, rel_path)
);

CREATE INDEX tracks_album ON tracks (album_key, disc_no, track_no);
CREATE INDEX tracks_group_artist ON tracks (group_artist_norm) WHERE group_artist_norm <> '';
-- Covering index for short (LIKE-style) searches: scanning it is several times cheaper than
-- scanning the table, and it yields title order so a LIMIT can stop early.
CREATE INDEX tracks_search ON tracks
    (title_norm, artist_norm, album_norm, album_artist_norm, genre_norm) WHERE available = 1;

-- Albums and artists of available tracks, kept in step with `tracks` by the writer (see
-- `refresh_aggregates`): listing and searching them must not group 100k rows per query.
CREATE TABLE albums (
    album_key   TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    artist      TEXT,
    year        INTEGER,
    track_count INTEGER NOT NULL,
    disc_count  INTEGER NOT NULL,
    duration_ms INTEGER NOT NULL,
    art_track   INTEGER,
    added_at    INTEGER NOT NULL,
    title_norm  TEXT NOT NULL,
    artist_norm TEXT NOT NULL
) WITHOUT ROWID;

-- Unknown albums (empty title) sort after named ones.
CREATE INDEX albums_title ON albums (title_norm = '', title_norm, album_key);
CREATE INDEX albums_artist ON albums (artist_norm, album_key);
CREATE INDEX albums_added ON albums (added_at, album_key);

-- Keyed by the normalized album artist (tracks.group_artist_norm).
CREATE TABLE artists (
    artist_norm TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    album_count INTEGER NOT NULL,
    track_count INTEGER NOT NULL
) WITHOUT ROWID;

-- Keys whose aggregate rows need recomputing, filled by triggers on `tracks`. Duplicates are
-- fine; no unique constraint, because an outer UPSERT's conflict handling would override a
-- trigger's INSERT OR IGNORE.
CREATE TABLE dirty_albums (album_key TEXT NOT NULL);
CREATE TABLE dirty_artists (artist_norm TEXT NOT NULL);

CREATE TRIGGER tracks_dirty_insert AFTER INSERT ON tracks BEGIN
    INSERT INTO dirty_albums VALUES (new.album_key);
    INSERT INTO dirty_artists SELECT new.group_artist_norm WHERE new.group_artist_norm <> '';
END;

CREATE TRIGGER tracks_dirty_update AFTER UPDATE ON tracks BEGIN
    INSERT INTO dirty_albums VALUES (old.album_key);
    INSERT INTO dirty_albums SELECT new.album_key WHERE new.album_key <> old.album_key;
    INSERT INTO dirty_artists SELECT old.group_artist_norm WHERE old.group_artist_norm <> '';
    INSERT INTO dirty_artists SELECT new.group_artist_norm
        WHERE new.group_artist_norm <> '' AND new.group_artist_norm <> old.group_artist_norm;
END;

CREATE TRIGGER tracks_dirty_delete AFTER DELETE ON tracks BEGIN
    INSERT INTO dirty_albums VALUES (old.album_key);
    INSERT INTO dirty_artists SELECT old.group_artist_norm WHERE old.group_artist_norm <> '';
END;

CREATE VIRTUAL TABLE tracks_fts USING fts5 (
    title_norm, artist_norm, album_norm, album_artist_norm, genre_norm,
    content = 'tracks', content_rowid = 'id', tokenize = 'trigram'
);

CREATE TRIGGER tracks_fts_insert AFTER INSERT ON tracks BEGIN
    INSERT INTO tracks_fts (rowid, title_norm, artist_norm, album_norm, album_artist_norm, genre_norm)
    VALUES (new.id, new.title_norm, new.artist_norm, new.album_norm, new.album_artist_norm, new.genre_norm);
END;

CREATE TRIGGER tracks_fts_delete AFTER DELETE ON tracks BEGIN
    INSERT INTO tracks_fts (tracks_fts, rowid, title_norm, artist_norm, album_norm, album_artist_norm, genre_norm)
    VALUES ('delete', old.id, old.title_norm, old.artist_norm, old.album_norm, old.album_artist_norm, old.genre_norm);
END;

CREATE TRIGGER tracks_fts_update
AFTER UPDATE OF title_norm, artist_norm, album_norm, album_artist_norm, genre_norm ON tracks BEGIN
    INSERT INTO tracks_fts (tracks_fts, rowid, title_norm, artist_norm, album_norm, album_artist_norm, genre_norm)
    VALUES ('delete', old.id, old.title_norm, old.artist_norm, old.album_norm, old.album_artist_norm, old.genre_norm);
    INSERT INTO tracks_fts (rowid, title_norm, artist_norm, album_norm, album_artist_norm, genre_norm)
    VALUES (new.id, new.title_norm, new.artist_norm, new.album_norm, new.album_artist_norm, new.genre_norm);
END;

-- Raw play history (§7.6). No foreign key: events outlive removed roots and tracks.
CREATE TABLE play_events (
    id           INTEGER PRIMARY KEY,
    track_id     INTEGER NOT NULL,
    context      TEXT,
    started_at   INTEGER NOT NULL,
    played_ms    INTEGER NOT NULL,
    ended_reason TEXT    NOT NULL,
    origin       TEXT    NOT NULL
);

CREATE INDEX play_events_started ON play_events (started_at);
CREATE INDEX play_events_track ON play_events (track_id, started_at);
-- Historical migration 2

ALTER TABLE tracks ADD COLUMN rel_path_norm TEXT NOT NULL DEFAULT '';

-- Every text tag, one row per value (docs/public-core/queries-playlists-undo.md). Keys are upper case: lofty's
-- unified (Vorbis comment) names where it maps the key, else the format's own key.
CREATE TABLE track_tags (
    track_id   INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    key        TEXT    NOT NULL,
    value      TEXT    NOT NULL,
    value_norm TEXT    NOT NULL
);
CREATE INDEX track_tags_track ON track_tags (track_id, key);
CREATE INDEX track_tags_key ON track_tags (key, value_norm, track_id);

-- Derived from play_events (stats.rs); no foreign keys, like play_events.
CREATE TABLE track_stats (
    track_id        INTEGER PRIMARY KEY,
    play_count      INTEGER NOT NULL DEFAULT 0,
    skip_count      INTEGER NOT NULL DEFAULT 0,
    first_played_at INTEGER,
    last_played_at  INTEGER
);

-- User data that lives only in the database: kept across rescans and soft root removal.
CREATE TABLE track_user (
    track_id INTEGER PRIMARY KEY,
    rating   INTEGER CHECK (rating BETWEEN 0 AND 5),
    loved    INTEGER NOT NULL DEFAULT 0
);

-- Facets and sorts on common columns.
CREATE INDEX tracks_genre ON tracks (genre_norm) WHERE available = 1;
CREATE INDEX tracks_year ON tracks (year) WHERE available = 1;
-- Historical migration 3

-- AUTOINCREMENT: ids of purged playlists and removed items are never handed out again.
CREATE TABLE playlists (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT    NOT NULL,
    -- normalize(name), for ordering.
    name_norm   TEXT    NOT NULL,
    kind        TEXT    NOT NULL CHECK (kind IN ('manual', 'smart')),
    -- Smart playlists: the TrackQuery as JSON, and the optional track limit.
    query_json  TEXT,
    item_limit  INTEGER,
    -- Bumped by every edit; checked against `expected_revision`.
    revision    INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    -- Set while the playlist is in the trash; retained until confirmed manual purge.
    deleted_at  INTEGER
);
CREATE INDEX playlists_name ON playlists (name_norm, id);
CREATE INDEX playlists_trash ON playlists (deleted_at) WHERE deleted_at IS NOT NULL;

-- Items of manual playlists. `id` is the stable item id (the same track twice is two items);
-- `position` is dense (0..n) and renumbered on edit. The track reference has no cascade:
-- tracks are never hard-deleted (missing files and removed roots only make them
-- unavailable), and a future hard delete of a listed track fails instead of silently
-- dropping playlist items.
CREATE TABLE playlist_items (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    position    INTEGER NOT NULL,
    track_id    INTEGER NOT NULL REFERENCES tracks(id)
);
-- Not unique: shifting a range of positions passes through duplicates.
CREATE INDEX playlist_items_position ON playlist_items (playlist_id, position);

-- The inverse of each playlist's last edit (one-step undo), valid while the playlist's
-- revision is still `revision`. `op` says how to read the rest:
--   'name'    text = the previous name
--   'query'   text = the previous query JSON, number = the previous limit
--   'remove'  data = ids of the items the edit added
--   'restore' data = (position, item id, track id) of the items the edit removed
--   'move'    data = (item id, previous position)
-- `data` is a compact integer encoding (playlist.rs, `encode_ints`).
CREATE TABLE playlist_undo (
    playlist_id INTEGER PRIMARY KEY REFERENCES playlists(id) ON DELETE CASCADE,
    revision    INTEGER NOT NULL,
    op          TEXT    NOT NULL,
    text        TEXT,
    number      INTEGER,
    data        BLOB
);

-- Small persistent flags. 'rescan_recommended' (a counter, present while set): files must
-- be re-read; cleared by the next completed scan.
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value INTEGER NOT NULL
) WITHOUT ROWID;
-- Historical migration 4
-- Historical migration 5

-- Each playlist edit's inverse, newest last (playlist.rs): an undo applies the newest step
-- and deletes it. At most 50 steps per playlist, each kept 30 days. Columns as in the
-- v3 `playlist_undo`, plus when the edit was made; `revision` is the revision the edit
-- produced (informational).
CREATE TABLE playlist_undo_steps (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    revision    INTEGER NOT NULL,
    created_at  INTEGER NOT NULL,
    op          TEXT    NOT NULL,
    text        TEXT,
    number      INTEGER,
    data        BLOB
);
CREATE INDEX playlist_undo_steps_playlist ON playlist_undo_steps (playlist_id, id);
CREATE INDEX playlist_undo_steps_created ON playlist_undo_steps (created_at);

-- The one-step record was only valid while the playlist was still at its revision.
INSERT INTO playlist_undo_steps (playlist_id, revision, created_at, op, text, number, data)
SELECT u.playlist_id, u.revision, p.updated_at, u.op, u.text, u.number, u.data
FROM playlist_undo u JOIN playlists p ON p.id = u.playlist_id
WHERE u.revision = p.revision;
DROP TABLE playlist_undo;

-- Ratings are 1-5 stars; a stored 0 becomes unrated.
CREATE TABLE track_user_v5 (
    track_id INTEGER PRIMARY KEY,
    rating   INTEGER CHECK (rating BETWEEN 1 AND 5),
    loved    INTEGER NOT NULL DEFAULT 0
);
INSERT INTO track_user_v5 (track_id, rating, loved)
SELECT track_id, NULLIF(rating, 0), loved FROM track_user
WHERE NULLIF(rating, 0) IS NOT NULL OR loved <> 0;
DROP TABLE track_user;
ALTER TABLE track_user_v5 RENAME TO track_user;
-- Historical migration 6

-- One row per recorded change of user data. `inverse` (journal.rs `Inverse`) undoes it;
-- NULL once it cannot be undone any more. An undo is an action too: `undo_of` names the
-- action it undid (which gets `undone_by`), `redo` is set when that was itself an undo.
-- Kept 30 days, at most 10000 rows.
CREATE TABLE actions (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    at_ms        INTEGER NOT NULL,
    origin       TEXT    NOT NULL,
    kind         TEXT    NOT NULL,
    summary      TEXT    NOT NULL,
    target_count INTEGER NOT NULL,
    undone_by    INTEGER,
    undo_of      INTEGER,
    redo         INTEGER NOT NULL DEFAULT 0,
    inverse      BLOB
);
CREATE INDEX actions_at ON actions (at_ms);
CREATE INDEX actions_origin ON actions (origin, id);

-- The objects each action changed, by the version it gave them: `kind` is 'track'
-- (track_user.user_version), 'playlist' (playlists.revision), 'playlist_trash'
-- (playlists.trash_version) or 'root' (roots.version). The states say which state of the
-- object the action found and left (journal.rs explains the conflict check).
CREATE TABLE action_targets (
    kind          TEXT    NOT NULL,
    object_id     INTEGER NOT NULL,
    version_after INTEGER NOT NULL,
    action_id     INTEGER NOT NULL REFERENCES actions(id) ON DELETE CASCADE,
    state_before  INTEGER NOT NULL,
    state_after   INTEGER NOT NULL,
    PRIMARY KEY (kind, object_id, version_after)
) WITHOUT ROWID;
CREATE INDEX action_targets_action ON action_targets (action_id);

-- Bumped by every change of the rating or loved flag; rows are never deleted any more.
ALTER TABLE track_user ADD COLUMN user_version INTEGER NOT NULL DEFAULT 0;
-- Bumped by every move into or out of the trash.
ALTER TABLE playlists ADD COLUMN trash_version INTEGER NOT NULL DEFAULT 0;
-- Bumped by every removal and re-activation.
ALTER TABLE roots ADD COLUMN version INTEGER NOT NULL DEFAULT 0;

-- AUTOINCREMENT: a play event id is never handed out again, even if events are deleted.
CREATE TABLE play_events_v6 (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id     INTEGER NOT NULL,
    context      TEXT,
    started_at   INTEGER NOT NULL,
    played_ms    INTEGER NOT NULL,
    ended_reason TEXT    NOT NULL,
    origin       TEXT    NOT NULL
);
INSERT INTO play_events_v6 (id, track_id, context, started_at, played_ms, ended_reason, origin)
SELECT id, track_id, context, started_at, played_ms, ended_reason, origin FROM play_events;
DROP TABLE play_events;
ALTER TABLE play_events_v6 RENAME TO play_events;
CREATE INDEX play_events_started ON play_events (started_at);
CREATE INDEX play_events_track ON play_events (track_id, started_at);

-- Empty-input Rust journal step: no undo rows to backfill.
DROP TABLE playlist_undo_steps;
-- Historical migration 7

-- Agents the user registered. Only the SHA-256 of an agent's token is kept, never the
-- token itself.
CREATE TABLE agents (
    name         TEXT    PRIMARY KEY,
    token_hash   BLOB    NOT NULL,
    created_at   INTEGER NOT NULL,
    last_seen_at INTEGER
) WITHOUT ROWID;

-- The user's permission grants that outlive core: until a time (`expires_at`, Unix ms) or
-- forever (NULL); grants for a session live in core's memory only. `operation`, `scope`
-- and `effect` are written by core (the contract's shapes) and opaque here. Ids come from
-- the 'grant_id' counter in `meta`, which session grants share, so none is reused.
CREATE TABLE grants (
    id         INTEGER PRIMARY KEY,
    agent      TEXT    NOT NULL,
    session    TEXT,
    operation  TEXT    NOT NULL,
    scope      TEXT    NOT NULL,
    expires_at INTEGER,
    effect     TEXT    NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX grants_agent ON grants (agent, id);

-- The grant that let an action run without asking the user (NULL: none).
ALTER TABLE actions ADD COLUMN grant_id INTEGER;
-- Historical migration 8

CREATE TABLE tracks_v8 (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    root_id           INTEGER NOT NULL REFERENCES roots(id) ON DELETE CASCADE,
    -- For a sheet track: its audio file.
    rel_path          TEXT    NOT NULL,
    size              INTEGER NOT NULL,
    mtime_ms          INTEGER NOT NULL,
    available         INTEGER NOT NULL DEFAULT 1,

    title             TEXT,
    artist            TEXT,
    album             TEXT,
    album_artist      TEXT,
    genre             TEXT,
    year              INTEGER,
    track_no          INTEGER,
    track_total       INTEGER,
    disc_no           INTEGER,
    disc_total        INTEGER,
    compilation       INTEGER NOT NULL DEFAULT 0,

    duration_ms       INTEGER,
    sample_rate       INTEGER,
    channels          INTEGER,
    bits_per_sample   INTEGER,
    bitrate_kbps      INTEGER,
    codec             TEXT,
    lossless          INTEGER,
    rg_track_gain     REAL,
    rg_track_peak     REAL,
    rg_album_gain     REAL,
    rg_album_peak     REAL,
    has_embedded_art  INTEGER NOT NULL DEFAULT 0,
    has_folder_art    INTEGER NOT NULL DEFAULT 0,
    tag_encoding      TEXT,

    title_norm        TEXT    NOT NULL DEFAULT '',
    artist_norm       TEXT    NOT NULL DEFAULT '',
    album_norm        TEXT    NOT NULL DEFAULT '',
    album_artist_norm TEXT    NOT NULL DEFAULT '',
    genre_norm        TEXT    NOT NULL DEFAULT '',
    album_key         TEXT    NOT NULL,
    album_group_artist TEXT,
    group_artist_norm TEXT    NOT NULL DEFAULT '',

    added_at          INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    rel_path_norm     TEXT    NOT NULL DEFAULT '',

    -- A track of a CUE sheet: the sheet (root-relative; the audio file itself for a sheet
    -- embedded in it), the TRACK number (0: the hidden track before track 1), where it
    -- starts and ends in its file in CD frames (1/75 s; end NULL: the end of the file), and
    -- the sheet file's size and modification time as scanned. All NULL for a file's row.
    cue_sheet         TEXT,
    cue_number        INTEGER,
    cue_start         INTEGER,
    cue_end           INTEGER,
    cue_size          INTEGER,
    cue_mtime_ms      INTEGER,
    -- A file's row whose file a sheet cuts into tracks: not a track of its own while so
    -- (`available` is 0 too, but the file is not missing).
    hidden            INTEGER NOT NULL DEFAULT 0
);

INSERT INTO tracks_v8 (
    id, root_id, rel_path, size, mtime_ms, available, title, artist, album, album_artist,
    genre, year, track_no, track_total, disc_no, disc_total, compilation, duration_ms,
    sample_rate, channels, bits_per_sample, bitrate_kbps, codec, lossless, rg_track_gain,
    rg_track_peak, rg_album_gain, rg_album_peak, has_embedded_art, has_folder_art,
    tag_encoding, title_norm, artist_norm, album_norm, album_artist_norm, genre_norm,
    album_key, album_group_artist, group_artist_norm, added_at, updated_at, rel_path_norm)
SELECT
    id, root_id, rel_path, size, mtime_ms, available, title, artist, album, album_artist,
    genre, year, track_no, track_total, disc_no, disc_total, compilation, duration_ms,
    sample_rate, channels, bits_per_sample, bitrate_kbps, codec, lossless, rg_track_gain,
    rg_track_peak, rg_album_gain, rg_album_peak, has_embedded_art, has_folder_art,
    tag_encoding, title_norm, artist_norm, album_norm, album_artist_norm, genre_norm,
    album_key, album_group_artist, group_artist_norm, added_at, updated_at, rel_path_norm
FROM tracks;

-- Ids of deleted rows are never handed out again: keep the counter.
INSERT INTO sqlite_sequence (name, seq)
SELECT 'tracks_v8', 0 WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'tracks_v8');
UPDATE sqlite_sequence SET seq = MAX(seq,
    COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'tracks'), 0))
WHERE name = 'tracks_v8';

DROP TABLE tracks;
ALTER TABLE tracks_v8 RENAME TO tracks;

CREATE UNIQUE INDEX tracks_file ON tracks (root_id, rel_path) WHERE cue_sheet IS NULL;
CREATE UNIQUE INDEX tracks_cue ON tracks (root_id, cue_sheet, cue_number)
    WHERE cue_sheet IS NOT NULL;
CREATE INDEX tracks_album ON tracks (album_key, disc_no, track_no);
CREATE INDEX tracks_group_artist ON tracks (group_artist_norm) WHERE group_artist_norm <> '';
CREATE INDEX tracks_search ON tracks
    (title_norm, artist_norm, album_norm, album_artist_norm, genre_norm) WHERE available = 1;
CREATE INDEX tracks_genre ON tracks (genre_norm) WHERE available = 1;
CREATE INDEX tracks_year ON tracks (year) WHERE available = 1;

CREATE TRIGGER tracks_dirty_insert AFTER INSERT ON tracks BEGIN
    INSERT INTO dirty_albums VALUES (new.album_key);
    INSERT INTO dirty_artists SELECT new.group_artist_norm WHERE new.group_artist_norm <> '';
END;

CREATE TRIGGER tracks_dirty_update AFTER UPDATE ON tracks BEGIN
    INSERT INTO dirty_albums VALUES (old.album_key);
    INSERT INTO dirty_albums SELECT new.album_key WHERE new.album_key <> old.album_key;
    INSERT INTO dirty_artists SELECT old.group_artist_norm WHERE old.group_artist_norm <> '';
    INSERT INTO dirty_artists SELECT new.group_artist_norm
        WHERE new.group_artist_norm <> '' AND new.group_artist_norm <> old.group_artist_norm;
END;

CREATE TRIGGER tracks_dirty_delete AFTER DELETE ON tracks BEGIN
    INSERT INTO dirty_albums VALUES (old.album_key);
    INSERT INTO dirty_artists SELECT old.group_artist_norm WHERE old.group_artist_norm <> '';
END;

CREATE TRIGGER tracks_fts_insert AFTER INSERT ON tracks BEGIN
    INSERT INTO tracks_fts (rowid, title_norm, artist_norm, album_norm, album_artist_norm, genre_norm)
    VALUES (new.id, new.title_norm, new.artist_norm, new.album_norm, new.album_artist_norm, new.genre_norm);
END;

CREATE TRIGGER tracks_fts_delete AFTER DELETE ON tracks BEGIN
    INSERT INTO tracks_fts (tracks_fts, rowid, title_norm, artist_norm, album_norm, album_artist_norm, genre_norm)
    VALUES ('delete', old.id, old.title_norm, old.artist_norm, old.album_norm, old.album_artist_norm, old.genre_norm);
END;

CREATE TRIGGER tracks_fts_update
AFTER UPDATE OF title_norm, artist_norm, album_norm, album_artist_norm, genre_norm ON tracks BEGIN
    INSERT INTO tracks_fts (tracks_fts, rowid, title_norm, artist_norm, album_norm, album_artist_norm, genre_norm)
    VALUES ('delete', old.id, old.title_norm, old.artist_norm, old.album_norm, old.album_artist_norm, old.genre_norm);
    INSERT INTO tracks_fts (rowid, title_norm, artist_norm, album_norm, album_artist_norm, genre_norm)
    VALUES (new.id, new.title_norm, new.artist_norm, new.album_norm, new.album_artist_norm, new.genre_norm);
END;
-- Historical migration 9

CREATE TABLE track_loudness (
    track_id INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
    version TEXT NOT NULL,
    integrated_lufs REAL,
    sample_peak REAL NOT NULL CHECK(sample_peak >= 0),
    measured_at INTEGER NOT NULL
);
CREATE TABLE album_loudness (
    album_key TEXT PRIMARY KEY,
    integrated_lufs REAL NOT NULL,
    sample_peak REAL NOT NULL CHECK(sample_peak >= 0),
    measured_at INTEGER NOT NULL
);
CREATE TRIGGER loudness_track_version AFTER UPDATE OF size, mtime_ms, cue_sheet,
    cue_number, cue_start, cue_end, cue_size, cue_mtime_ms, rel_path, root_id, sample_rate, channels ON tracks
WHEN OLD.size IS NOT NEW.size OR OLD.mtime_ms IS NOT NEW.mtime_ms
 OR OLD.cue_sheet IS NOT NEW.cue_sheet OR OLD.cue_number IS NOT NEW.cue_number
 OR OLD.cue_start IS NOT NEW.cue_start OR OLD.cue_end IS NOT NEW.cue_end
 OR OLD.cue_size IS NOT NEW.cue_size OR OLD.cue_mtime_ms IS NOT NEW.cue_mtime_ms
 OR OLD.rel_path IS NOT NEW.rel_path OR OLD.root_id IS NOT NEW.root_id
 OR OLD.sample_rate IS NOT NEW.sample_rate OR OLD.channels IS NOT NEW.channels
BEGIN
 DELETE FROM track_loudness WHERE track_id=OLD.id;
 DELETE FROM album_loudness WHERE album_key IN (OLD.album_key, NEW.album_key);
END;
CREATE TRIGGER loudness_album_insert AFTER INSERT ON tracks BEGIN
 DELETE FROM album_loudness WHERE album_key=NEW.album_key;
END;
CREATE TRIGGER loudness_album_delete AFTER DELETE ON tracks BEGIN
 DELETE FROM album_loudness WHERE album_key=OLD.album_key;
END;
CREATE TRIGGER loudness_album_members AFTER UPDATE OF available, hidden, album_key ON tracks
WHEN OLD.available IS NOT NEW.available OR OLD.hidden IS NOT NEW.hidden OR OLD.album_key IS NOT NEW.album_key
BEGIN
 DELETE FROM album_loudness WHERE album_key IN (OLD.album_key, NEW.album_key);
END;
-- Historical migration 10

ALTER TABLE track_loudness ADD COLUMN statistics BLOB;
-- Historical migration 11

DROP TRIGGER IF EXISTS loudness_track_version;
CREATE TRIGGER loudness_track_version AFTER UPDATE OF size, mtime_ms, cue_sheet,
    cue_number, cue_start, cue_end, cue_size, cue_mtime_ms, rel_path, root_id, sample_rate, channels ON tracks
WHEN NEW.mtime_ms IS NOT -1 AND (OLD.size IS NOT NEW.size OR OLD.mtime_ms IS NOT NEW.mtime_ms
 OR OLD.cue_sheet IS NOT NEW.cue_sheet OR OLD.cue_number IS NOT NEW.cue_number
 OR OLD.cue_start IS NOT NEW.cue_start OR OLD.cue_end IS NOT NEW.cue_end
 OR OLD.cue_size IS NOT NEW.cue_size OR OLD.cue_mtime_ms IS NOT NEW.cue_mtime_ms
 OR OLD.rel_path IS NOT NEW.rel_path OR OLD.root_id IS NOT NEW.root_id
 OR OLD.sample_rate IS NOT NEW.sample_rate OR OLD.channels IS NOT NEW.channels)
BEGIN
 DELETE FROM album_loudness WHERE album_key IN (OLD.album_key, NEW.album_key)
  AND NOT EXISTS (SELECT 1 FROM track_loudness WHERE track_id=NEW.id AND version=json_array(NEW.size,NEW.mtime_ms,NEW.cue_sheet,NEW.cue_number,NEW.cue_start,NEW.cue_end,NEW.cue_size,NEW.cue_mtime_ms,NEW.rel_path,NEW.root_id,NEW.sample_rate,NEW.channels));
 DELETE FROM track_loudness WHERE track_id=OLD.id
  AND version IS NOT json_array(NEW.size,NEW.mtime_ms,NEW.cue_sheet,NEW.cue_number,NEW.cue_start,NEW.cue_end,NEW.cue_size,NEW.cue_mtime_ms,NEW.rel_path,NEW.root_id,NEW.sample_rate,NEW.channels);
END;
-- Historical migration 12
ALTER TABLE actions ADD COLUMN details_json TEXT;
-- Historical migration 13

DROP INDEX tracks_file;
CREATE UNIQUE INDEX tracks_file ON tracks(root_id,rel_path) WHERE cue_sheet IS NULL AND (hidden=0 OR available=1);
ALTER TABLE tracks ADD COLUMN artists_source TEXT;
ALTER TABLE tracks ADD COLUMN artists_parser_version INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tracks ADD COLUMN cover_revision TEXT;
ALTER TABLE tracks ADD COLUMN metadata_revision INTEGER NOT NULL DEFAULT 0;
CREATE TABLE person_artists (
    key TEXT PRIMARY KEY CHECK(length(key)>0),
    name_norm TEXT NOT NULL UNIQUE CHECK(length(name_norm)>0)
) WITHOUT ROWID;
CREATE TABLE track_artists (
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    artist_key TEXT NOT NULL REFERENCES person_artists(key),
    ordinal INTEGER NOT NULL CHECK(ordinal>=0),
    name TEXT NOT NULL,
    PRIMARY KEY(track_id, artist_key),
    UNIQUE(track_id, ordinal)
) WITHOUT ROWID;
CREATE INDEX track_artists_person ON track_artists(artist_key, track_id);
CREATE INDEX tracks_artists_pending ON tracks(artists_parser_version, id) WHERE hidden=0;
CREATE TABLE file_write_intents (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    family TEXT NOT NULL,
    format_version INTEGER NOT NULL,
    authorization_json TEXT NOT NULL,
    target_json TEXT NOT NULL,
    before_digest TEXT NOT NULL,
    before_image BLOB NOT NULL,
    after_digest TEXT NOT NULL,
    stage TEXT NOT NULL,
    action_id INTEGER REFERENCES actions(id) ON DELETE SET NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX file_write_intents_stage ON file_write_intents(stage, id);

PRAGMA user_version=13;
PRAGMA application_id=0;

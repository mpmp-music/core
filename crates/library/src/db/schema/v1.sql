CREATE TABLE action_targets (
    kind          TEXT    NOT NULL,
    object_id     INTEGER NOT NULL,
    version_after INTEGER NOT NULL,
    action_id     INTEGER NOT NULL REFERENCES actions(id) ON DELETE CASCADE,
    state_before  INTEGER NOT NULL,
    state_after   INTEGER NOT NULL,
    PRIMARY KEY (kind, object_id, version_after)
) WITHOUT ROWID;

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
, grant_id INTEGER, details_json TEXT);

CREATE TABLE agents (
    name         TEXT    PRIMARY KEY,
    token_hash   BLOB    NOT NULL,
    created_at   INTEGER NOT NULL,
    last_seen_at INTEGER
) WITHOUT ROWID;

CREATE TABLE album_loudness (
    album_key TEXT PRIMARY KEY,
    integrated_lufs REAL NOT NULL,
    sample_peak REAL NOT NULL CHECK(sample_peak >= 0),
    measured_at INTEGER NOT NULL
);

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

CREATE TABLE artists (
    artist_norm TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    album_count INTEGER NOT NULL,
    track_count INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE dirty_albums (album_key TEXT NOT NULL);

CREATE TABLE dirty_artists (artist_norm TEXT NOT NULL);

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

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE person_artists (
    key TEXT PRIMARY KEY CHECK(length(key)>0),
    name_norm TEXT NOT NULL UNIQUE CHECK(length(name_norm)>0)
) WITHOUT ROWID;

CREATE TABLE "play_events" (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id     INTEGER NOT NULL,
    context      TEXT,
    started_at   INTEGER NOT NULL,
    played_ms    INTEGER NOT NULL,
    ended_reason TEXT    NOT NULL,
    origin       TEXT    NOT NULL
);

CREATE TABLE playlist_items (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    position    INTEGER NOT NULL,
    track_id    INTEGER NOT NULL REFERENCES tracks(id)
);

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
, trash_version INTEGER NOT NULL DEFAULT 0);

CREATE TABLE roots (
    id              INTEGER PRIMARY KEY,
    path            TEXT    NOT NULL UNIQUE,
    forced_encoding TEXT,
    added_at        INTEGER NOT NULL,
    last_scan_at    INTEGER,
    -- Removal is soft (§6.1): the root and its tracks stay, so re-adding the folder brings
    -- back the same track ids and the play history stays linked.
    removed         INTEGER NOT NULL DEFAULT 0
, version INTEGER NOT NULL DEFAULT 0);

CREATE TABLE track_artists (
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    artist_key TEXT NOT NULL REFERENCES person_artists(key),
    ordinal INTEGER NOT NULL CHECK(ordinal>=0),
    name TEXT NOT NULL,
    PRIMARY KEY(track_id, artist_key),
    UNIQUE(track_id, ordinal)
) WITHOUT ROWID;

CREATE TABLE track_loudness (
    track_id INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
    version TEXT NOT NULL,
    integrated_lufs REAL,
    sample_peak REAL NOT NULL CHECK(sample_peak >= 0),
    measured_at INTEGER NOT NULL
, statistics BLOB);

CREATE TABLE track_stats (
    track_id        INTEGER PRIMARY KEY,
    play_count      INTEGER NOT NULL DEFAULT 0,
    skip_count      INTEGER NOT NULL DEFAULT 0,
    first_played_at INTEGER,
    last_played_at  INTEGER
);

CREATE TABLE track_tags (
    track_id   INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    key        TEXT    NOT NULL,
    value      TEXT    NOT NULL,
    value_norm TEXT    NOT NULL
);

CREATE TABLE "track_user" (
    track_id INTEGER PRIMARY KEY,
    rating   INTEGER CHECK (rating BETWEEN 1 AND 5),
    loved    INTEGER NOT NULL DEFAULT 0
, user_version INTEGER NOT NULL DEFAULT 0);

CREATE TABLE "tracks" (
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
, artists_source TEXT, artists_parser_version INTEGER NOT NULL DEFAULT 0, cover_revision TEXT, metadata_revision INTEGER NOT NULL DEFAULT 0);

CREATE VIRTUAL TABLE tracks_fts USING fts5 (
    title_norm, artist_norm, album_norm, album_artist_norm, genre_norm,
    content = 'tracks', content_rowid = 'id', tokenize = 'trigram'
);

CREATE INDEX action_targets_action ON action_targets (action_id);

CREATE INDEX actions_at ON actions (at_ms);

CREATE INDEX actions_origin ON actions (origin, id);

CREATE INDEX albums_added ON albums (added_at, album_key);

CREATE INDEX albums_artist ON albums (artist_norm, album_key);

CREATE INDEX albums_title ON albums (title_norm = '', title_norm, album_key);

CREATE INDEX file_write_intents_stage ON file_write_intents(stage, id);

CREATE INDEX grants_agent ON grants (agent, id);

CREATE TRIGGER loudness_album_delete AFTER DELETE ON tracks BEGIN
 DELETE FROM album_loudness WHERE album_key=OLD.album_key;
END;

CREATE TRIGGER loudness_album_insert AFTER INSERT ON tracks BEGIN
 DELETE FROM album_loudness WHERE album_key=NEW.album_key;
END;

CREATE TRIGGER loudness_album_members AFTER UPDATE OF available, hidden, album_key ON tracks
WHEN OLD.available IS NOT NEW.available OR OLD.hidden IS NOT NEW.hidden OR OLD.album_key IS NOT NEW.album_key
BEGIN
 DELETE FROM album_loudness WHERE album_key IN (OLD.album_key, NEW.album_key);
END;

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

CREATE INDEX play_events_started ON play_events (started_at);

CREATE INDEX play_events_track ON play_events (track_id, started_at);

CREATE INDEX playlist_items_position ON playlist_items (playlist_id, position);

CREATE INDEX playlists_name ON playlists (name_norm, id);

CREATE INDEX playlists_trash ON playlists (deleted_at) WHERE deleted_at IS NOT NULL;

CREATE INDEX track_artists_person ON track_artists(artist_key, track_id);

CREATE INDEX track_tags_key ON track_tags (key, value_norm, track_id);

CREATE INDEX track_tags_track ON track_tags (track_id, key);

CREATE INDEX tracks_album ON tracks (album_key, disc_no, track_no);

CREATE INDEX tracks_artists_pending ON tracks(artists_parser_version, id) WHERE hidden=0;

CREATE UNIQUE INDEX tracks_cue ON tracks (root_id, cue_sheet, cue_number)
    WHERE cue_sheet IS NOT NULL;

CREATE TRIGGER tracks_dirty_delete AFTER DELETE ON tracks BEGIN
    INSERT INTO dirty_albums VALUES (old.album_key);
    INSERT INTO dirty_artists SELECT old.group_artist_norm WHERE old.group_artist_norm <> '';
END;

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

CREATE UNIQUE INDEX tracks_file ON tracks(root_id,rel_path) WHERE cue_sheet IS NULL AND (hidden=0 OR available=1);

CREATE TRIGGER tracks_fts_delete AFTER DELETE ON tracks BEGIN
    INSERT INTO tracks_fts (tracks_fts, rowid, title_norm, artist_norm, album_norm, album_artist_norm, genre_norm)
    VALUES ('delete', old.id, old.title_norm, old.artist_norm, old.album_norm, old.album_artist_norm, old.genre_norm);
END;

CREATE TRIGGER tracks_fts_insert AFTER INSERT ON tracks BEGIN
    INSERT INTO tracks_fts (rowid, title_norm, artist_norm, album_norm, album_artist_norm, genre_norm)
    VALUES (new.id, new.title_norm, new.artist_norm, new.album_norm, new.album_artist_norm, new.genre_norm);
END;

CREATE TRIGGER tracks_fts_update
AFTER UPDATE OF title_norm, artist_norm, album_norm, album_artist_norm, genre_norm ON tracks BEGIN
    INSERT INTO tracks_fts (tracks_fts, rowid, title_norm, artist_norm, album_norm, album_artist_norm, genre_norm)
    VALUES ('delete', old.id, old.title_norm, old.artist_norm, old.album_norm, old.album_artist_norm, old.genre_norm);
    INSERT INTO tracks_fts (rowid, title_norm, artist_norm, album_norm, album_artist_norm, genre_norm)
    VALUES (new.id, new.title_norm, new.artist_norm, new.album_norm, new.album_artist_norm, new.genre_norm);
END;

CREATE INDEX tracks_genre ON tracks (genre_norm) WHERE available = 1;

CREATE INDEX tracks_group_artist ON tracks (group_artist_norm) WHERE group_artist_norm <> '';

CREATE INDEX tracks_search ON tracks
    (title_norm, artist_norm, album_norm, album_artist_norm, genre_norm) WHERE available = 1;

CREATE INDEX tracks_year ON tracks (year) WHERE available = 1;

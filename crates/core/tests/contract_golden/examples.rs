//! The golden examples as Rust values: every command, query, query result, event and
//! response shape of contract v1, written to `tests/golden/current/` by
//! `UPDATE_GOLDEN=1 cargo test -p mpmp-core --test contract_golden`.
//!
//! The values are plausible (ids from the clock, real-looking tracks) but are not the
//! output of one session; what matters is that every shape is shown. The coverage test
//! (`every_variant_and_optional_field_has_a_golden`) fails when a new variant or optional
//! field has no example here.

use mpmp_test_support::client_parity;

use std::collections::BTreeMap;

use mpmp_core::contract::*;

/// Which message a golden file holds: its directory under `golden/<set>/` and its root
/// in docs/contract.schema.json.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Request,
    Query,
    Response,
    QueryResult,
    Event,
}

impl Kind {
    pub const ALL: [Kind; 5] = [
        Kind::Request,
        Kind::Query,
        Kind::Response,
        Kind::QueryResult,
        Kind::Event,
    ];

    pub fn dir(self) -> &'static str {
        match self {
            Kind::Request => "request",
            Kind::Query => "query",
            Kind::Response => "response",
            Kind::QueryResult => "query_result",
            Kind::Event => "event",
        }
    }

    /// The root of docs/contract.schema.json that describes it.
    pub fn schema_root(self) -> &'static str {
        match self {
            Kind::Request => "Request",
            Kind::Query => "Query",
            Kind::Response => "Response",
            Kind::QueryResult => "QueryResult",
            Kind::Event => "EventEnvelope",
        }
    }

    /// Client → core (strict) rather than core → client (lenient).
    pub fn is_request_side(self) -> bool {
        matches!(self, Kind::Request | Kind::Query)
    }

    /// Decodes JSON as this kind's Rust type and encodes it again. Through text, as on
    /// the wire: `serde_json::to_value` would widen an `f32` such as 0.8 to
    /// 0.800000011920929, where the text encoding writes 0.8.
    pub fn reencode(self, json: &serde_json::Value) -> Result<serde_json::Value, String> {
        fn re<T: serde::de::DeserializeOwned + serde::Serialize>(
            json: &serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            let value: T = serde_json::from_str(&json.to_string()).map_err(|e| e.to_string())?;
            let text = serde_json::to_string(&value).map_err(|e| e.to_string())?;
            serde_json::from_str(&text).map_err(|e| e.to_string())
        }
        match self {
            Kind::Request => re::<Request>(json),
            Kind::Query => re::<Query>(json),
            Kind::Response => re::<Response>(json),
            Kind::QueryResult => re::<QueryResult>(json),
            Kind::Event => re::<EventEnvelope>(json),
        }
    }

    /// Decodes a golden file's text and pretty-prints the Rust value again (the canonical
    /// form, fields in declaration order).
    pub fn reencode_text(self, text: &str) -> Result<String, String> {
        fn re<T: serde::de::DeserializeOwned + serde::Serialize>(
            text: &str,
        ) -> Result<String, String> {
            let value: T = serde_json::from_str(text).map_err(|e| e.to_string())?;
            serde_json::to_string_pretty(&value).map_err(|e| e.to_string())
        }
        match self {
            Kind::Request => re::<Request>(text),
            Kind::Query => re::<Query>(text),
            Kind::Response => re::<Response>(text),
            Kind::QueryResult => re::<QueryResult>(text),
            Kind::Event => re::<EventEnvelope>(text),
        }
        .map(|s| s + "\n")
    }
}

pub enum Message {
    Request(Request),
    Query(Query),
    Response(Response),
    QueryResult(QueryResult),
    Event(EventEnvelope),
}

pub struct Golden {
    /// The file name without `.json`.
    pub name: String,
    pub message: Message,
}

impl Golden {
    pub fn kind(&self) -> Kind {
        match self.message {
            Message::Request(_) => Kind::Request,
            Message::Query(_) => Kind::Query,
            Message::Response(_) => Kind::Response,
            Message::QueryResult(_) => Kind::QueryResult,
            Message::Event(_) => Kind::Event,
        }
    }

    /// `request/play_album.json`
    pub fn path(&self) -> String {
        format!("{}/{}.json", self.kind().dir(), self.name)
    }

    /// The file's content: pretty JSON of the Rust value, with a final newline.
    pub fn text(&self) -> String {
        let text = match &self.message {
            Message::Request(v) => serde_json::to_string_pretty(v),
            Message::Query(v) => serde_json::to_string_pretty(v),
            Message::Response(v) => serde_json::to_string_pretty(v),
            Message::QueryResult(v) => serde_json::to_string_pretty(v),
            Message::Event(v) => serde_json::to_string_pretty(v),
        };
        text.expect("contract values always serialize") + "\n"
    }
}

// --- building blocks ---

/// Ids and revisions start from the clock (guarantee 6).
const CLOCK: u64 = 1_792_500_000_000;

const NOW_MS: i64 = 1_792_540_800_000;

const DAY_MS: i64 = 86_400_000;

fn entry(n: u64) -> EntryId {
    EntryId(CLOCK + n)
}

fn revision(n: u64) -> QueueRevision {
    QueueRevision(CLOCK + n)
}

fn agent() -> Origin {
    Origin::agent("claude")
}

fn remote() -> Origin {
    Origin::Remote {
        device: "living-room-tablet".into(),
    }
}

fn album_key(artist: &str, album: &str) -> String {
    format!("{artist}\u{1f}{album}")
}

fn track(id: i64, no: u32, title: &str, duration_ms: u64) -> TrackInfo {
    TrackInfo {
        id: TrackId(id),
        title: title.into(),
        title_from_file: false,
        artist: Some("Miren Dalen".into()),
        artists: vec![ArtistRef {
            key: "person:v1:9534d53acedcff35f87de4c0d30a5eb5427b91bd3a14145bb7492ec103f86876"
                .into(),
            name: "Miren Dalen".into(),
        }],
        artists_source: Some(ArtistListSource::ArtistValues),
        artists_complete: true,
        album: Some("Kites of Brass".into()),
        album_artist: Some("Miren Dalen".into()),
        group_artist: Some("Miren Dalen".into()),
        album_key: album_key("miren dalen", "kites of brass"),
        track_no: Some(no),
        track_total: Some(5),
        disc_no: Some(1),
        disc_total: Some(1),
        year: Some(1959),
        genre: Some("Jazz".into()),
        compilation: false,
        duration_ms: Some(duration_ms),
        codec: Some("flac".into()),
        sample_rate: Some(44_100),
        channels: Some(2),
        bits_per_sample: Some(16),
        bitrate_kbps: Some(912),
        lossless: true,
        root: RootId(1),
        path: format!("Miren Dalen/Kites of Brass/0{no} {title}.flac"),
        size_bytes: 61_234_567,
        modified_at_ms: 1_700_000_000_000,
        added_at_ms: 1_745_000_000_000,
        available: true,
        has_cover: true,
        cover_revision: Some("sha256:example".into()),
        rating: Some(5),
        loved: true,
        play_count: 42,
        skip_count: 1,
        last_played_at_ms: Some(NOW_MS - DAY_MS),
        first_played_at_ms: Some(1_745_100_000_000),
        replay_gain: Some(ReplayGainInfo {
            track_gain_db: Some(-7.9),
            track_peak: Some(0.98),
            album_gain_db: Some(-8.4),
            album_peak: Some(1.0),
            track_source: GainDataSource::Tags,
            album_source: GainDataSource::Tags,
        }),
        tags: BTreeMap::new(),
        cue: None,
    }
}

fn so_what() -> TrackInfo {
    track(1201, 1, "Soft Wheels", 562_000)
}

fn freddie() -> TrackInfo {
    track(1202, 2, "Fennel Ferryman", 589_000)
}

fn with_tags(mut track: TrackInfo) -> TrackInfo {
    track.tags = BTreeMap::from([
        (
            "COMPOSER".to_owned(),
            vec!["Miren Dalen".to_owned(), "Bren Elven".to_owned()],
        ),
        ("LABEL".to_owned(), vec!["Cloudbox".to_owned()]),
    ]);
    track
}

/// A file without tags whose folder was removed: every optional value missing.
fn unavailable_track() -> TrackInfo {
    TrackInfo {
        id: TrackId(877),
        title: "track03".into(),
        title_from_file: true,
        album_key: album_key("", ""),
        codec: Some("mp3".into()),
        root: RootId(2),
        path: "old rips/track03.mp3".into(),
        size_bytes: 4_812_345,
        modified_at_ms: 1_200_000_000_000,
        added_at_ms: 1_745_000_000_000,
        available: false,
        ..Default::default()
    }
}

fn hires_track() -> TrackInfo {
    TrackInfo {
        id: TrackId(2310),
        title: "Brass in Glass".into(),
        album: Some("Kites of Brass (2022 Remaster)".into()),
        album_key: album_key("miren dalen", "kites of brass 2022 remaster"),
        sample_rate: Some(96_000),
        bits_per_sample: Some(24),
        bitrate_kbps: Some(3_521),
        rating: None,
        loved: false,
        play_count: 0,
        skip_count: 0,
        last_played_at_ms: None,
        first_played_at_ms: None,
        replay_gain: None,
        ..track(2310, 3, "Brass in Glass", 337_000)
    }
}

fn mp3_track() -> TrackInfo {
    TrackInfo {
        id: TrackId(3050),
        title: "Dunafinado".into(),
        artist: Some("Soren Glen & João Galveno".into()),
        album: Some("Glen/Galveno".into()),
        album_artist: None,
        group_artist: Some("Soren Glen & João Galveno".into()),
        album_key: album_key("stan getz joao gilberto", "getz gilberto"),
        year: Some(1964),
        genre: Some("Bossa Nova".into()),
        codec: Some("mp3".into()),
        bits_per_sample: None,
        bitrate_kbps: Some(320),
        lossless: false,
        path: "Glen Galveno/02 Dunafinado.mp3".into(),
        size_bytes: 9_876_543,
        rating: Some(4),
        loved: false,
        replay_gain: Some(ReplayGainInfo {
            track_gain_db: Some(-4.2),
            track_peak: Some(0.89),
            album_gain_db: None,
            album_peak: None,
            track_source: GainDataSource::Tags,
            album_source: GainDataSource::Tags,
        }),
        ..track(3050, 2, "Dunafinado", 254_000)
    }
}

fn replay_gain_stage(detail: &str) -> StageInfo {
    StageInfo {
        name: "replay gain".into(),
        kind: SignalStageKind::ReplayGain,
        gain_db: Some(-8.4),
        gain_source: Some(GainDataSource::Tags),
        gain_scope: Some(GainScope::Album),
        detail: detail.into(),
        alters_samples: true,
        ..Default::default()
    }
}

fn volume_stage() -> StageInfo {
    StageInfo {
        name: "volume".into(),
        kind: SignalStageKind::Volume,
        gain_db: Some(-5.8),
        silent: Some(false),
        detail: "-5.8 dB".into(),
        alters_samples: true,
        ..Default::default()
    }
}

fn resample_stage() -> StageInfo {
    StageInfo {
        name: "resample".into(),
        kind: SignalStageKind::Resample,
        input_format: Some(SignalFormat {
            sample_rate: 44100,
            channels: 2,
            bits_per_sample: Some(16),
            valid_bits_per_sample: Some(16),
            sample_format: None,
        }),
        output_format: Some(SignalFormat {
            sample_rate: 48000,
            channels: 2,
            bits_per_sample: Some(32),
            valid_bits_per_sample: None,
            sample_format: Some(SampleFormat::F32),
        }),
        detail: "44100 Hz → 48000 Hz".into(),
        alters_samples: true,
        ..Default::default()
    }
}

fn signal_path(
    track: &TrackInfo,
    stages: Vec<StageInfo>,
    output_rate: u32,
    output_format: SampleFormat,
    bit_perfect: BitPerfectInfo,
) -> SignalPathInfo {
    SignalPathInfo {
        share_mode: Some(if matches!(bit_perfect, BitPerfectInfo::Yes) {
            OutputShareMode::Exclusive
        } else {
            OutputShareMode::Shared
        }),
        output_device_id: Some("cpal:example".into()),
        backend: Some(if matches!(bit_perfect, BitPerfectInfo::Yes) {
            OutputBackend::Wasapi
        } else {
            OutputBackend::Cpal
        }),
        sample_integrity: Some(if matches!(bit_perfect, BitPerfectInfo::Yes) {
            SampleIntegrity::Verified
        } else {
            SampleIntegrity::Unverified
        }),
        codec: track.codec.clone().unwrap_or_default(),
        lossless: track.lossless,
        source_rate: track.sample_rate.unwrap_or(44_100),
        source_channels: track.channels.unwrap_or(2),
        source_bits_per_sample: track.bits_per_sample,
        stages: stages
            .iter()
            .cloned()
            .map(|mut stage| {
                if stage.input_format.is_some() {
                    stage.input_format = Some(SignalFormat {
                        sample_rate: track.sample_rate.unwrap_or(44_100),
                        channels: track.channels.unwrap_or(2),
                        bits_per_sample: track.bits_per_sample,
                        valid_bits_per_sample: track.bits_per_sample,
                        sample_format: None,
                    });
                    stage.output_format = Some(SignalFormat {
                        sample_rate: output_rate,
                        channels: 2,
                        bits_per_sample: Some(match output_format {
                            SampleFormat::I16 => 16,
                            SampleFormat::I24 => 24,
                            _ => 32,
                        }),
                        valid_bits_per_sample: match output_format {
                            SampleFormat::I16 => Some(16),
                            SampleFormat::I24 => Some(24),
                            SampleFormat::I32 => Some(32),
                            _ => None,
                        },
                        sample_format: Some(output_format),
                    });
                }
                stage
            })
            .collect(),
        output_rate,
        output_channels: 2,
        output_format,
        reason_codes: match &bit_perfect {
            BitPerfectInfo::Uncertain { .. } => vec![SignalReasonCode::SharedMixer],
            BitPerfectInfo::No { .. } => {
                let mut codes = Vec::new();
                if !track.lossless {
                    codes.push(SignalReasonCode::LossySource);
                }
                codes.extend(
                    stages
                        .iter()
                        .filter(|s| s.alters_samples)
                        .map(|s| match s.kind {
                            SignalStageKind::Resample => SignalReasonCode::Resample,
                            SignalStageKind::ReplayGain => SignalReasonCode::ReplayGain,
                            SignalStageKind::Volume => SignalReasonCode::Volume,
                            _ => SignalReasonCode::Unknown,
                        }),
                );
                codes
            }
            _ => Vec::new(),
        },
        bit_perfect,
    }
}

fn audio(volume: f32, volume_db: Option<f32>, muted: bool, mode: ReplayGainMode) -> AudioSettings {
    AudioSettings {
        output: Some(OutputSettings::default()),
        volume,
        volume_db,
        muted,
        volume_mode: VolumeMode::Software,
        replay_gain: ReplayGainSettings {
            mode,
            preamp_db: 0.0,
            fallback_preamp_db: -6.0,
            prevent_clipping: true,
        },
    }
}

fn snapshot(seq: u64) -> Snapshot {
    Snapshot {
        seq,
        contract_version: CONTRACT_VERSION,
        contract_minor: CONTRACT_MINOR,
        features: vec![
            "multi_artist".into(),
            "cover_edit".into(),
            "playback_limits".into(),
            "output_devices".into(),
            "output_mode_contract".into(),
            "permissions".into(),
            "lyrics".into(),
            "waveform".into(),
            "cue".into(),
            "edit_tags".into(),
            "automatic_loudness".into(),
            "paused_navigation".into(),
            "track_rows".into(),
            "album_paging".into(),
            "search_paging".into(),
            "enqueue_expected_current".into(),
            "playlist_playback_guards".into(),
            "release_root".into(),
            "queue_candidates".into(),
            "structured_signal_path".into(),
            "job_queries".into(),
            "action_details".into(),
            "scan_error_codes".into(),
        ],
        now_ms: 3_605_250,
        state: PlayState::Stopped,
        current: None,
        position: None,
        queue: QueueSummary::default(),
        shuffle: false,
        repeat: RepeatMode::Off,
        audio: audio(0.8, Some(-5.81), false, ReplayGainMode::Smart),
        playback_limits: Some(PlaybackLimitsInfo::default()),
        output: Some(OutputStateInfo::default()),
        problems: Vec::new(),
    }
}

/// Playing, resampled for a shared-mode device, with ReplayGain and volume.
fn snapshot_playing() -> Snapshot {
    let track = so_what();
    Snapshot {
        state: PlayState::Playing,
        current: Some(NowPlaying {
            entry: entry(2),
            signal_path: Some(signal_path(
                &track,
                vec![
                    resample_stage(),
                    replay_gain_stage("album -8.4 dB"),
                    volume_stage(),
                ],
                48_000,
                SampleFormat::F32,
                BitPerfectInfo::No {
                    reasons: vec![
                        "resample: 44100 Hz → 48000 Hz".into(),
                        "replay gain: album -8.4 dB".into(),
                        "volume: -5.8 dB".into(),
                    ],
                },
            )),
            track,
        }),
        position: Some(PositionInfo {
            entry: entry(2),
            position_ms: 83_250,
            at_ms: 3_605_000,
            rate: 1.0,
        }),
        queue: QueueSummary {
            revision: revision(17),
            entry_count: 9,
            current_index: Some(1),
            next_entry: Some(entry(7)),
            natural_next_entry: Some(entry(7)),
        },
        shuffle: true,
        repeat: RepeatMode::All,
        ..snapshot(4182)
    }
}

/// Fresh start: nothing queued, muted at volume 0.
fn snapshot_idle() -> Snapshot {
    Snapshot {
        seq: 0,
        now_ms: 112,
        queue: QueueSummary {
            revision: revision(0),
            entry_count: 0,
            current_index: None,
            ..Default::default()
        },
        audio: audio(0.0, None, true, ReplayGainMode::Off),
        ..snapshot(0)
    }
}

/// Paused on a CD-quality track, bit-perfect as far as core can tell.
fn snapshot_paused() -> Snapshot {
    let track = freddie();
    Snapshot {
        state: PlayState::Paused,
        current: Some(NowPlaying {
            entry: entry(3),
            signal_path: Some(signal_path(
                &track,
                Vec::new(),
                44_100,
                SampleFormat::I16,
                BitPerfectInfo::Uncertain {
                    reason: "shared mode: the OS mixer may process the signal".into(),
                },
            )),
            track,
        }),
        position: Some(PositionInfo {
            entry: entry(3),
            position_ms: 201_400,
            at_ms: 3_640_020,
            rate: 0.0,
        }),
        queue: QueueSummary {
            revision: revision(17),
            entry_count: 9,
            current_index: Some(2),
            next_entry: Some(entry(4)),
            natural_next_entry: Some(entry(3)),
        },
        repeat: RepeatMode::One,
        now_ms: 3_640_020,
        audio: audio(1.0, Some(0.0), false, ReplayGainMode::Track),
        ..snapshot(4190)
    }
}

/// Playing a lossy file into a 32-bit device after a resync.
fn snapshot_lossy() -> Snapshot {
    let track = mp3_track();
    Snapshot {
        state: PlayState::Playing,
        current: Some(NowPlaying {
            entry: entry(9),
            signal_path: Some(signal_path(
                &track,
                vec![resample_stage()],
                48_000,
                SampleFormat::I32,
                BitPerfectInfo::No {
                    reasons: vec![
                        "lossy source (mp3)".into(),
                        "resample: 44100 Hz → 48000 Hz".into(),
                    ],
                },
            )),
            track,
        }),
        position: Some(PositionInfo {
            entry: entry(9),
            position_ms: 12_000,
            at_ms: 3_900_000,
            rate: 1.0,
        }),
        queue: QueueSummary {
            revision: revision(21),
            entry_count: 12,
            current_index: Some(8),
            next_entry: Some(entry(10)),
            natural_next_entry: Some(entry(10)),
        },
        now_ms: 3_900_000,
        audio: audio(0.8, Some(-5.81), false, ReplayGainMode::Album),
        ..snapshot(5003)
    }
}

/// Exercises every filter kind, field, value kind and operator.
fn every_filter() -> Filter {
    use Field as F;
    fn cmp(field: Field, op: CompareOp, value: Value) -> Filter {
        Filter::Compare { field, op, value }
    }
    fn text(field: Field, op: TextOp, text: &str) -> Filter {
        Filter::Text {
            field,
            op,
            text: text.into(),
        }
    }
    Filter::And {
        all: vec![
            Filter::Or {
                any: vec![
                    text(F::Title, TextOp::Contains, "blue"),
                    text(F::Artist, TextOp::Equals, "miren dalen"),
                    text(F::Album, TextOp::StartsWith, "kind of"),
                    text(F::AlbumArtist, TextOp::Contains, "davis"),
                    text(F::Genre, TextOp::Equals, "jazz"),
                    text(
                        F::Tag {
                            key: "COMPOSER".into(),
                        },
                        TextOp::Contains,
                        "elven",
                    ),
                    Filter::Search {
                        text: "modal".into(),
                    },
                ],
            },
            cmp(F::Year, CompareOp::Ge, Value::Int(1955)),
            cmp(F::Year, CompareOp::Lt, Value::Int(1970)),
            cmp(F::TrackNo, CompareOp::Le, Value::Int(12)),
            cmp(F::DiscNo, CompareOp::Eq, Value::Int(1)),
            cmp(F::Compilation, CompareOp::Eq, Value::Bool(false)),
            cmp(F::DurationMs, CompareOp::Gt, Value::Int(120_000)),
            cmp(F::Codec, CompareOp::Eq, Value::Text("flac".into())),
            cmp(F::SampleRate, CompareOp::Ge, Value::Int(44_100)),
            cmp(F::BitsPerSample, CompareOp::Ge, Value::Int(16)),
            cmp(F::Channels, CompareOp::Eq, Value::Int(2)),
            cmp(F::Lossless, CompareOp::Eq, Value::Bool(true)),
            cmp(F::BitrateKbps, CompareOp::Ge, Value::Int(320)),
            text(F::Path, TextOp::StartsWith, "miren dalen/"),
            cmp(F::Root, CompareOp::Ne, Value::Int(3)),
            cmp(F::SizeBytes, CompareOp::Lt, Value::Int(200_000_000)),
            cmp(
                F::ModifiedAt,
                CompareOp::Ge,
                Value::Timestamp(1_600_000_000_000),
            ),
            cmp(F::AddedAt, CompareOp::Ge, Value::DaysAgo(365)),
            cmp(F::Available, CompareOp::Eq, Value::Bool(true)),
            cmp(F::HasCover, CompareOp::Eq, Value::Bool(true)),
            cmp(F::PlayCount, CompareOp::Ge, Value::Int(3)),
            cmp(F::SkipCount, CompareOp::Le, Value::Int(1)),
            cmp(F::LastPlayedAt, CompareOp::Lt, Value::DaysAgo(30)),
            cmp(
                F::FirstPlayedAt,
                CompareOp::Ge,
                Value::Timestamp(1_700_000_000_000),
            ),
            cmp(F::Rating, CompareOp::Ge, Value::Int(4)),
            Filter::Not {
                filter: Box::new(Filter::Or {
                    any: vec![
                        cmp(F::Loved, CompareOp::Eq, Value::Bool(false)),
                        Filter::Missing { field: F::Genre },
                    ],
                }),
            },
            Filter::All,
        ],
    }
}

fn sort(field: Field, descending: bool) -> SortKey {
    SortKey { field, descending }
}

/// "Forgotten favourites": four stars or more, not heard for a month.
fn favourites_query() -> TrackQuery {
    TrackQuery {
        filter: Filter::And {
            all: vec![
                Filter::Compare {
                    field: Field::Rating,
                    op: CompareOp::Ge,
                    value: Value::Int(4),
                },
                Filter::Compare {
                    field: Field::LastPlayedAt,
                    op: CompareOp::Lt,
                    value: Value::DaysAgo(30),
                },
            ],
        },
        sort: vec![sort(Field::PlayCount, true)],
    }
}

fn album_summary() -> AlbumSummary {
    AlbumSummary {
        key: album_key("miren dalen", "kites of brass"),
        title: "Kites of Brass".into(),
        artist: Some("Miren Dalen".into()),
        year: Some(1959),
        track_count: 5,
        disc_count: 1,
        duration_ms: 2_756_000,
        art_track: Some(TrackId(1201)),
        added_at_ms: 1_745_000_000_000,
    }
}

fn unknown_album_summary() -> AlbumSummary {
    AlbumSummary {
        key: album_key("", ""),
        title: String::new(),
        artist: None,
        year: None,
        track_count: 31,
        disc_count: 0,
        duration_ms: 6_120_000,
        art_track: None,
        added_at_ms: 1_745_000_000_000,
    }
}

fn manual_playlist() -> PlaylistSummary {
    PlaylistSummary {
        id: PlaylistId(12),
        name: "Late night".into(),
        kind: PlaylistKind::Manual,
        track_count: 2,
        duration_ms: 1_151_000,
        revision: PlaylistRevision(8),
        created_at_ms: 1_750_000_000_000,
        updated_at_ms: NOW_MS - 2 * DAY_MS,
        deleted_at_ms: None,
        query_error: None,
    }
}

fn smart_playlist() -> PlaylistSummary {
    PlaylistSummary {
        id: PlaylistId(15),
        name: "Forgotten favourites".into(),
        kind: PlaylistKind::Smart {
            query: favourites_query(),
            max_tracks: Some(100),
        },
        track_count: 64,
        duration_ms: 17_300_000,
        revision: PlaylistRevision(2),
        created_at_ms: 1_755_000_000_000,
        updated_at_ms: 1_755_000_000_000,
        deleted_at_ms: None,
        query_error: None,
    }
}

fn broken_smart_playlist() -> PlaylistSummary {
    PlaylistSummary {
        id: PlaylistId(16),
        name: "By composer".into(),
        kind: PlaylistKind::Smart {
            query: TrackQuery {
                filter: Filter::Text {
                    field: Field::Tag {
                        key: "COMPOSER".into(),
                    },
                    op: TextOp::Contains,
                    text: "elven".into(),
                },
                sort: Vec::new(),
            },
            max_tracks: None,
        },
        track_count: 0,
        duration_ms: 0,
        revision: PlaylistRevision(1),
        created_at_ms: 1_756_000_000_000,
        updated_at_ms: 1_756_000_000_000,
        deleted_at_ms: None,
        query_error: Some("the stored query no longer type-checks: unknown field".into()),
    }
}

fn error(kind: ErrorKind, code: ErrorCode, message: &str) -> ContractError {
    ContractError::new(kind, code, message)
}

fn receipt(changed: bool, queue_revision: u64) -> Receipt {
    Receipt {
        changed,
        queue_revision: revision(queue_revision),
        ..Default::default()
    }
}

#[allow(clippy::too_many_arguments)]
fn action(
    id: i64,
    minutes_ago: i64,
    origin: Origin,
    kind: ActionKind,
    summary: &str,
    target_count: u32,
    undone_by: Option<i64>,
    undo_of: Option<i64>,
) -> ActionSummary {
    ActionSummary {
        id: ActionId(id),
        recorded_at_ms: NOW_MS - minutes_ago * 60_000,
        origin,
        kind,
        summary: summary.into(),
        target_count,
        undone_by: undone_by.map(ActionId),
        undo_of: undo_of.map(ActionId),
        undoable: undone_by.is_none(),
        grant: None,
        details: None,
    }
}

/// An agent connected through an adapter that tells sessions apart.
fn agent_session() -> Origin {
    Origin::Agent {
        name: "claude".into(),
        session: Some("conn-7f3a".into()),
    }
}

fn grant(
    id: i64,
    session: Option<&str>,
    operation: Operation,
    scope: Scope,
    until: Expiry,
    effect: Effect,
) -> Grant {
    Grant {
        id: GrantId(id),
        agent: "claude".into(),
        session: session.map(Into::into),
        operation,
        scope,
        until,
        effect,
        created_at_ms: NOW_MS - 3 * DAY_MS,
    }
}

// --- the goldens ---

struct Goldens {
    all: Vec<Golden>,
    request_id: u64,
}

impl Goldens {
    fn push(&mut self, name: &str, message: Message) {
        self.all.push(Golden {
            name: name.into(),
            message,
        });
    }

    fn request(&mut self, name: &str, key: Option<&str>, command: Command) {
        self.request_id += 1;
        let request = Request {
            request_id: self.request_id,
            idempotency_key: key.map(Into::into),
            command,
        };
        self.push(name, Message::Request(request));
    }

    fn query(&mut self, name: &str, query: Query) {
        self.push(name, Message::Query(query));
    }

    fn ok(&mut self, name: &str, request_id: u64, receipt: Receipt) {
        self.response(name, request_id, CommandResult::Ok(receipt));
    }

    fn err(&mut self, name: &str, request_id: u64, error: ContractError) {
        self.response(name, request_id, CommandResult::Err { error });
    }

    fn response(&mut self, name: &str, request_id: u64, result: CommandResult) {
        self.push(name, Message::Response(Response { request_id, result }));
    }

    fn result(&mut self, name: &str, result: QueryResult) {
        self.push(name, Message::QueryResult(result));
    }

    fn event(&mut self, name: &str, seq: u64, origin: Option<Origin>, event: Event) {
        self.push(name, Message::Event(EventEnvelope { seq, origin, event }));
    }
}

/// Every golden of the current contract.
pub fn all() -> Vec<Golden> {
    let mut g = Goldens {
        all: Vec::new(),
        request_id: 0,
    };
    minor_2(&mut g);
    requests(&mut g);
    queries(&mut g);
    permission_queries(&mut g);
    lyrics_queries(&mut g);
    waveform_queries(&mut g);
    cue_queries(&mut g);
    responses(&mut g);
    query_results(&mut g);
    permission_results(&mut g);
    lyrics_results(&mut g);
    waveform_results(&mut g);
    cue_results(&mut g);
    events(&mut g);
    waveform_events(&mut g);
    tag_edit_requests(&mut g);
    tag_edit_queries(&mut g);
    tag_edit_responses(&mut g);
    tag_edit_results(&mut g);
    tag_edit_events(&mut g);
    loudness_examples(&mut g);
    g.request(
        "play_playlist_item",
        None,
        Command::Play {
            context: PlayContext::Playlist {
                playlist: PlaylistId(12),
                start_item: Some(PlaylistItemId(18)),
                expected_revision: Some(PlaylistRevision(4)),
            },
            start: 0,
            start_track: None,
        },
    );
    let cases: serde_json::Value = serde_json::from_str(client_parity::SIGNAL).unwrap();
    for (index, case) in cases.as_array().unwrap().iter().enumerate() {
        let mut value = snapshot_playing();
        value.current.as_mut().unwrap().signal_path =
            Some(serde_json::from_value(case["expected"].clone()).unwrap());
        g.result(
            &format!("snapshot_structured_signal_{index}"),
            QueryResult::Snapshot {
                snapshot: Box::new(value),
            },
        );
    }
    playback_limits(&mut g);
    output_examples(&mut g);
    cover_examples(&mut g);
    g.all
}

fn requests(g: &mut Goldens) {
    // Playback.
    g.request(
        "play_album",
        Some("ui-play-1"),
        Command::Play {
            context: PlayContext::Album {
                key: album_key("miren dalen", "kites of brass"),
            },
            start: 0,
            start_track: None,
        },
    );
    g.request(
        "play_tracks",
        None,
        Command::Play {
            context: PlayContext::Tracks {
                tracks: vec![TrackId(1201), TrackId(1202), TrackId(3050)],
            },
            start: 2,
            start_track: None,
        },
    );
    g.request(
        "play_search",
        None,
        Command::Play {
            context: PlayContext::Search {
                text: "calven".into(),
            },
            start: 0,
            start_track: None,
        },
    );
    g.request(
        "play_query",
        None,
        Command::Play {
            context: PlayContext::Query {
                query: favourites_query(),
            },
            start: 0,
            start_track: None,
        },
    );
    g.request(
        "play_playlist",
        None,
        Command::Play {
            context: PlayContext::Playlist {
                playlist: PlaylistId(12),
                start_item: None,
                expected_revision: None,
            },
            start: 0,
            start_track: Some(TrackId(1202)),
        },
    );
    g.request("play_entry", None, Command::PlayEntry { entry: entry(5) });
    g.request("pause", None, Command::Pause);
    g.request("resume", None, Command::Resume);
    g.request("toggle_pause", None, Command::TogglePause);
    g.request("stop", None, Command::Stop);
    g.request("next", None, Command::Next);
    g.request("previous", None, Command::Previous);
    g.request(
        "seek",
        None,
        Command::Seek {
            position_ms: 120_000,
            entry: Some(entry(2)),
        },
    );
    // The queue.
    g.request(
        "enqueue",
        Some("agent-enqueue-7"),
        Command::Enqueue {
            tracks: vec![TrackId(2310), TrackId(3050)],
            at: InsertAt::Next,
            expected_revision: Some(revision(17)),
            expected_current: Some(entry(2)),
        },
    );
    g.request(
        "enqueue_at_end",
        None,
        Command::Enqueue {
            tracks: vec![TrackId(1201)],
            at: InsertAt::End,
            expected_revision: None,
            expected_current: None,
        },
    );
    g.request(
        "remove",
        None,
        Command::Remove {
            entries: vec![entry(4), entry(7)],
            expected_revision: Some(revision(18)),
        },
    );
    g.request(
        "move",
        None,
        Command::Move {
            entry: entry(6),
            to: 0,
            expected_revision: Some(revision(19)),
        },
    );
    g.request(
        "clear",
        None,
        Command::Clear {
            expected_revision: Some(revision(20)),
        },
    );
    g.request("set_shuffle", None, Command::SetShuffle { shuffle: true });
    g.request(
        "set_repeat",
        None,
        Command::SetRepeat {
            mode: RepeatMode::One,
        },
    );
    g.request(
        "undo",
        None,
        Command::Undo {
            revision: revision(21),
        },
    );
    // The library.
    g.request(
        "add_root",
        Some("ui-add-root-1"),
        Command::AddRoot {
            path: "/home/me/Music".into(),
        },
    );
    g.request("remove_root", None, Command::RemoveRoot { root: RootId(2) });
    g.request("rescan", None, Command::Rescan);
    g.request(
        "set_rating",
        Some("agent-rate-3"),
        Command::SetRating {
            tracks: vec![TrackId(1201), TrackId(1202)],
            rating: Some(5),
        },
    );
    g.request(
        "set_rating_clear",
        None,
        Command::SetRating {
            tracks: vec![TrackId(877)],
            rating: None,
        },
    );
    g.request(
        "set_loved",
        None,
        Command::SetLoved {
            tracks: vec![TrackId(3050)],
            loved: true,
        },
    );
    // Confirmations (UI only).
    g.request(
        "confirm",
        None,
        Command::Confirm {
            token: "c-5f0e2a9b".into(),
        },
    );
    g.request(
        "reject",
        None,
        Command::Reject {
            token: "c-77d1c3e0".into(),
        },
    );
    // Volume and ReplayGain.
    g.request("set_volume", None, Command::SetVolume { volume: 0.8 });
    g.request("set_muted", None, Command::SetMuted { muted: true });
    g.request(
        "set_volume_mode",
        None,
        Command::SetVolumeMode {
            mode: VolumeMode::System,
        },
    );
    g.request(
        "set_replay_gain",
        None,
        Command::SetReplayGain {
            mode: Some(ReplayGainMode::Album),
            preamp_db: Some(2.5),
            fallback_preamp_db: None,
            prevent_clipping: None,
        },
    );
    g.request(
        "set_replay_gain_untagged",
        None,
        Command::SetReplayGain {
            mode: None,
            preamp_db: None,
            fallback_preamp_db: Some(-6.0),
            prevent_clipping: Some(false),
        },
    );
    // Playlists.
    g.request(
        "create_playlist_manual",
        Some("ui-create-1"),
        Command::CreatePlaylist {
            name: "Late night".into(),
            kind: PlaylistKind::Manual,
        },
    );
    g.request(
        "create_playlist_smart",
        None,
        Command::CreatePlaylist {
            name: "Forgotten favourites".into(),
            kind: PlaylistKind::Smart {
                query: favourites_query(),
                max_tracks: Some(100),
            },
        },
    );
    g.request(
        "rename_playlist",
        None,
        Command::RenamePlaylist {
            playlist: PlaylistId(12),
            name: "Late night jazz".into(),
            expected_revision: Some(PlaylistRevision(8)),
        },
    );
    g.request(
        "delete_playlist",
        None,
        Command::DeletePlaylist {
            playlist: PlaylistId(12),
        },
    );
    g.request(
        "restore_playlist",
        None,
        Command::RestorePlaylist {
            playlist: PlaylistId(12),
        },
    );
    g.request(
        "purge_playlists",
        None,
        Command::PurgePlaylists {
            playlists: Some(vec![PlaylistId(9), PlaylistId(10)]),
        },
    );
    g.request(
        "purge_playlists_all",
        None,
        Command::PurgePlaylists { playlists: None },
    );
    g.request(
        "add_to_playlist",
        Some("remote-add-11"),
        Command::AddToPlaylist {
            playlist: PlaylistId(12),
            tracks: vec![TrackId(2310), TrackId(3050)],
            index: Some(1),
            expected_revision: Some(PlaylistRevision(8)),
        },
    );
    g.request(
        "remove_from_playlist",
        None,
        Command::RemoveFromPlaylist {
            playlist: PlaylistId(12),
            items: vec![PlaylistItemId(301)],
            expected_revision: Some(PlaylistRevision(9)),
        },
    );
    g.request(
        "move_playlist_item",
        None,
        Command::MovePlaylistItem {
            playlist: PlaylistId(12),
            item: PlaylistItemId(302),
            to: 0,
            expected_revision: Some(PlaylistRevision(10)),
        },
    );
    g.request(
        "set_smart_query",
        None,
        Command::SetSmartQuery {
            playlist: PlaylistId(15),
            query: favourites_query(),
            max_tracks: Some(50),
            expected_revision: Some(PlaylistRevision(2)),
        },
    );
    g.request(
        "undo_playlist",
        None,
        Command::UndoPlaylist {
            playlist: PlaylistId(12),
            revision: PlaylistRevision(11),
        },
    );
    g.request(
        "import_m3u",
        None,
        Command::ImportM3u {
            path: "/home/me/Playlists/road trip.m3u8".into(),
            name: Some("Road trip".into()),
        },
    );
    g.request(
        "export_m3u",
        None,
        Command::ExportM3u {
            playlist: PlaylistId(12),
            path: "/home/me/Playlists/late night.m3u8".into(),
        },
    );
    g.request(
        "undo_action",
        Some("agent-undo-2"),
        Command::UndoAction {
            action: ActionId(5121),
        },
    );

    // Agent permissions (minor 1, feature `permissions`).
    g.request(
        "register_agent",
        Some("ui-register-1"),
        Command::RegisterAgent {
            name: "claude".into(),
        },
    );
    g.request(
        "renew_agent_token",
        None,
        Command::RenewAgentToken {
            agent: "claude".into(),
        },
    );
    g.request(
        "remove_agent",
        None,
        Command::RemoveAgent {
            agent: "codex".into(),
        },
    );
    // "Allow for this session" from a confirmation dialog: the session's grant, then
    // `Confirm`.
    g.request(
        "grant_permission_session",
        None,
        Command::GrantPermission {
            agent: "claude".into(),
            session: Some("conn-7f3a".into()),
            operation: Operation::Class {
                class: OperationClass::LocalPath,
            },
            scope: Scope::Location {
                location: "/home/me/Music".into(),
            },
            until: Expiry::Session,
            effect: Effect::Allow,
        },
    );
    g.request(
        "grant_permission_until",
        None,
        Command::GrantPermission {
            agent: "claude".into(),
            session: None,
            operation: Operation::Command {
                command: "export_m3u".into(),
            },
            scope: Scope::Library {
                root: RootId(1),
                prefix: Some("Playlists".into()),
            },
            until: Expiry::At {
                at_ms: NOW_MS + 7 * DAY_MS,
            },
            effect: Effect::Allow,
        },
    );
    g.request(
        "grant_permission_forbid",
        None,
        Command::GrantPermission {
            agent: "codex".into(),
            session: None,
            operation: Operation::Class {
                class: OperationClass::FileWrite,
            },
            scope: Scope::All,
            until: Expiry::Forever,
            effect: Effect::Forbid,
        },
    );
    g.request(
        "revoke_permission",
        None,
        Command::RevokePermission { grant: GrantId(31) },
    );
}

fn queries(g: &mut Goldens) {
    for view in [ArtistView::AlbumGroup, ArtistView::Person] {
        let suffix = if view == ArtistView::Person {
            "person"
        } else {
            "album_group"
        };
        g.query(
            &format!("artists_{suffix}"),
            Query::Artists {
                view: Some(view),
                offset: 0,
                limit: 100,
            },
        );
        g.query(
            &format!("artist_albums_{suffix}"),
            Query::ArtistAlbums {
                view: Some(view),
                name: "Miren Dalen".into(),
                sort: AlbumSort::Artist,
                offset: 0,
                limit: 100,
            },
        );
        g.query(
            &format!("search_{suffix}"),
            Query::Search {
                artist_view: Some(view),
                text: "Miren".into(),
                kind: Some(SearchKind::Artists),
                offset: 0,
                limit: 100,
            },
        );
    }
    g.query(
        "tracks_artist_person",
        Query::Tracks {
            query: TrackQuery {
                filter: Filter::ArtistPerson {
                    name: "Miren Dalen".into(),
                },
                sort: vec![],
            },
            offset: 0,
            limit: 100,
            tags: vec![],
        },
    );
    g.result(
        "artists_person",
        QueryResult::Artists {
            artists: vec![ArtistSummary {
                key: Some(
                    "person:v1:9534d53acedcff35f87de4c0d30a5eb5427b91bd3a14145bb7492ec103f86876"
                        .into(),
                ),
                name: "Miren Dalen".into(),
                album_count: 1,
                track_count: 1,
            }],
            indexing: Some(true),
            offset: 0,
            total: 1,
            next_offset: None,
        },
    );
    for (suffix, source) in [
        ("artists_tag", ArtistListSource::ArtistsTag),
        ("cue_performer", ArtistListSource::CuePerformer),
        ("legacy_display", ArtistListSource::LegacyDisplay),
        ("none", ArtistListSource::None),
    ] {
        let mut t = so_what();
        t.artists_source = Some(source);
        if source == ArtistListSource::None {
            t.artists.clear();
        }
        g.result(
            &format!("track_artist_source_{suffix}"),
            QueryResult::TrackRows { tracks: vec![t] },
        );
    }

    g.query("snapshot", Query::Snapshot);
    g.query(
        "queue_page",
        Query::QueuePage {
            offset: 0,
            limit: 100,
        },
    );
    g.query(
        "search",
        Query::Search {
            artist_view: None,
            kind: None,
            offset: 0,
            text: "kites of brass".into(),
            limit: 20,
        },
    );
    for (name, sort) in [
        ("albums", AlbumSort::Artist),
        ("albums_by_title", AlbumSort::Title),
        ("albums_by_year", AlbumSort::Year),
        ("albums_by_title_desc", AlbumSort::TitleDesc),
        ("albums_by_artist_desc", AlbumSort::ArtistDesc),
        ("albums_by_year_desc", AlbumSort::YearDesc),
        ("albums_oldest_added", AlbumSort::OldestAdded),
    ] {
        g.query(
            name,
            Query::Albums {
                sort,
                offset: 0,
                limit: 200,
            },
        );
    }
    g.query(
        "album",
        Query::Album {
            offset: Some(0),
            limit: Some(1000),
            key: album_key("miren dalen", "kites of brass"),
        },
    );
    g.query(
        "playlists",
        Query::Playlists {
            deleted: false,
            offset: 0,
            limit: 100,
        },
    );
    g.query(
        "playlists_trash",
        Query::Playlists {
            deleted: true,
            offset: 0,
            limit: 100,
        },
    );
    g.query(
        "playlist_items",
        Query::PlaylistItems {
            playlist: PlaylistId(12),
            offset: 0,
            limit: 500,
            tags: vec!["COMPOSER".into(), "LABEL".into()],
        },
    );
    g.query(
        "artists",
        Query::Artists {
            view: None,
            offset: 0,
            limit: 500,
        },
    );
    g.query(
        "artist_albums",
        Query::ArtistAlbums {
            view: None,
            name: "Miren Dalen".into(),
            sort: AlbumSort::RecentlyAdded,
            offset: 0,
            limit: 50,
        },
    );
    g.query(
        "history",
        Query::History {
            offset: 0,
            limit: 50,
        },
    );
    g.query("roots", Query::Roots);
    g.query(
        "track_rows",
        Query::TrackRows {
            tracks: vec![TrackId(42), TrackId(999), TrackId(42)],
            tags: vec!["COMPOSER".into()],
        },
    );
    g.result(
        "track_rows",
        QueryResult::TrackRows {
            tracks: vec![with_tags(so_what()), with_tags(so_what())],
        },
    );
    g.query(
        "tracks",
        Query::Tracks {
            query: TrackQuery {
                filter: every_filter(),
                sort: vec![
                    sort(Field::Rating, true),
                    sort(Field::AlbumArtist, false),
                    sort(Field::GroupArtist, false),
                    sort(Field::Year, false),
                ],
            },
            offset: 0,
            limit: 1000,
            tags: vec!["COMPOSER".into()],
        },
    );
    g.query(
        "facet",
        Query::Facet {
            field: Field::Genre,
            filter: Filter::Compare {
                field: Field::Year,
                op: CompareOp::Lt,
                value: Value::Int(1970),
            },
            offset: 0,
            limit: 100,
        },
    );
    g.query(
        "actions",
        Query::Actions {
            origin: None,
            offset: 0,
            limit: 100,
        },
    );
    for (name, origin) in [
        ("actions_of_ui", OriginFilter::Ui),
        ("actions_of_media_controls", OriginFilter::MediaControls),
        ("actions_of_system", OriginFilter::System),
        ("actions_of_agents", OriginFilter::Agent { name: None }),
        (
            "actions_of_one_agent",
            OriginFilter::Agent {
                name: Some("claude".into()),
            },
        ),
        ("actions_of_remotes", OriginFilter::Remote { device: None }),
        (
            "actions_of_one_remote",
            OriginFilter::Remote {
                device: Some("living-room-tablet".into()),
            },
        ),
    ] {
        g.query(
            name,
            Query::Actions {
                origin: Some(origin),
                offset: 0,
                limit: 100,
            },
        );
    }
}

fn permission_queries(g: &mut Goldens) {
    g.query("agents", Query::Agents);
    g.query(
        "permissions",
        Query::Permissions {
            agent: None,
            offset: 0,
            limit: 100,
        },
    );
    g.query(
        "permissions_of_one_agent",
        Query::Permissions {
            agent: Some("claude".into()),
            offset: 0,
            limit: 100,
        },
    );
}

fn lyrics_queries(g: &mut Goldens) {
    g.query("lyrics", Query::Lyrics { track: TrackId(42) });
}

fn waveform_queries(g: &mut Goldens) {
    g.query("waveform", Query::Waveform { track: TrackId(42) });
}

fn responses(g: &mut Goldens) {
    g.ok("ok", 7, receipt(true, 17));
    g.ok(
        "ok_audit",
        70,
        Receipt {
            audit: Some(ActionId(5161)),
            ..receipt(true, 17)
        },
    );
    g.ok("ok_unchanged", 8, receipt(false, 17));
    g.ok(
        "ok_enqueue",
        14,
        Receipt {
            created_entries: vec![entry(10), entry(11)],
            ..receipt(true, 18)
        },
    );
    g.ok(
        "ok_remove",
        16,
        Receipt {
            removed_entries: vec![entry(4)],
            ..receipt(true, 19)
        },
    );
    g.ok(
        "ok_add_root",
        22,
        Receipt {
            root: Some(RootId(3)),
            job: Some(JobId(7)),
            action: Some(ActionId(5120)),
            ..receipt(true, 21)
        },
    );
    g.ok(
        "ok_create_playlist",
        36,
        Receipt {
            playlist: Some(PlaylistReceipt {
                playlist: PlaylistId(12),
                revision: PlaylistRevision(0),
                created_items: Vec::new(),
                removed_items: Vec::new(),
                import: None,
            }),
            action: Some(ActionId(5122)),
            ..receipt(true, 21)
        },
    );
    g.ok(
        "ok_add_to_playlist",
        43,
        Receipt {
            playlist: Some(PlaylistReceipt {
                playlist: PlaylistId(12),
                revision: PlaylistRevision(9),
                created_items: vec![PlaylistItemId(305), PlaylistItemId(306)],
                removed_items: Vec::new(),
                import: None,
            }),
            action: Some(ActionId(5123)),
            ..receipt(true, 21)
        },
    );
    g.ok(
        "ok_remove_from_playlist",
        44,
        Receipt {
            playlist: Some(PlaylistReceipt {
                playlist: PlaylistId(12),
                revision: PlaylistRevision(10),
                created_items: Vec::new(),
                removed_items: vec![PlaylistItemId(301)],
                import: None,
            }),
            action: Some(ActionId(5124)),
            ..receipt(true, 21)
        },
    );
    g.ok(
        "ok_import_m3u",
        48,
        Receipt {
            playlist: Some(PlaylistReceipt {
                playlist: PlaylistId(17),
                revision: PlaylistRevision(0),
                created_items: vec![PlaylistItemId(310), PlaylistItemId(311)],
                removed_items: Vec::new(),
                import: Some(M3uReport {
                    matched_count: 2,
                    missing: vec!["../Downloads/live at the plugged nickel.flac".into()],
                    missing_count: 1,
                }),
            }),
            action: Some(ActionId(5125)),
            ..receipt(true, 21)
        },
    );
    g.ok(
        "ok_confirm",
        27,
        Receipt {
            confirmed: Some(ConfirmationOutcome {
                token: "c-5f0e2a9b".into(),
                approved: true,
                result: Some(Box::new(CommandResult::Ok(Receipt {
                    root: Some(RootId(4)),
                    job: Some(JobId(8)),
                    action: Some(ActionId(5126)),
                    ..receipt(true, 21)
                }))),
                reason: Some(ConfirmationReason::Approved),
            }),
            ..receipt(true, 21)
        },
    );
    // A session disconnected after approval, while the command was still being rechecked.
    g.ok(
        "ok_confirm_withdrawn",
        28,
        Receipt {
            confirmed: Some(ConfirmationOutcome {
                token: "c-5e6f7a8b".into(),
                approved: false,
                result: None,
                reason: Some(ConfirmationReason::Withdrawn),
            }),
            ..receipt(true, 18)
        },
    );
    g.ok(
        "ok_reject",
        28,
        Receipt {
            confirmed: Some(ConfirmationOutcome {
                token: "c-77d1c3e0".into(),
                approved: false,
                result: None,
                reason: Some(ConfirmationReason::Rejected),
            }),
            ..receipt(true, 21)
        },
    );

    // One error of each kind, between them every `details` field.
    g.err(
        "err_conflict",
        14,
        ContractError::conflict(
            ErrorCode::QueueConflict,
            "the queue changed meanwhile: refresh and retry",
            CLOCK + 17,
            CLOCK + 18,
        ),
    );
    g.err(
        "err_action_conflict",
        51,
        error(
            ErrorKind::Conflict,
            ErrorCode::ActionConflict,
            "later actions changed 2 of the objects; nothing was undone",
        )
        .with_changed(vec![
            ObjectRef::Track { id: TrackId(1201) },
            ObjectRef::Playlist { id: PlaylistId(12) },
        ]),
    );
    g.err(
        "err_not_found",
        14,
        error(
            ErrorKind::NotFound,
            ErrorCode::TrackNotFound,
            "2 tracks do not exist",
        )
        .with_missing(vec![
            ObjectRef::Track { id: TrackId(4040) },
            ObjectRef::Track { id: TrackId(4041) },
        ])
        .with_field("command.tracks"),
    );
    // Shape only: no one command reports all of these, but each may appear.
    g.err(
        "err_not_found_every_object",
        52,
        error(ErrorKind::NotFound, ErrorCode::EntryNotFound, "not found").with_missing(vec![
            ObjectRef::Entry { id: entry(99) },
            ObjectRef::Track { id: TrackId(4040) },
            ObjectRef::Playlist { id: PlaylistId(99) },
            ObjectRef::PlaylistItem {
                id: PlaylistItemId(999),
            },
            ObjectRef::Root { id: RootId(9) },
            ObjectRef::Album {
                key: album_key("nobody", "nothing"),
            },
            ObjectRef::Confirmation {
                token: "c-00000000".into(),
            },
            ObjectRef::Job { id: JobId(99) },
            ObjectRef::Action { id: ActionId(9999) },
            ObjectRef::Agent {
                name: "gemini".into(),
            },
            ObjectRef::Grant { id: GrantId(99) },
        ]),
    );
    g.err(
        "err_invalid",
        13,
        ContractError::invalid(
            ErrorCode::OutOfRange,
            "position 9000000 ms is past the end of the track (562000 ms)",
        )
        .with_field("command.position_ms"),
    );
    g.err(
        "err_forbidden",
        22,
        ContractError::forbidden(
            ErrorCode::PathNotAllowed,
            "remote may not name local paths (add_root)",
        ),
    );
    g.err(
        "err_unsupported",
        10,
        ContractError::unsupported(
            ErrorCode::UnknownField,
            "this core does not know the field command.fade_ms",
        )
        .with_field("command.fade_ms"),
    );
    g.err(
        "err_failed",
        48,
        ContractError::failed(ErrorCode::Io, "reading the playlist file failed: busy").retryable(),
    );
    let mut recovery = ContractError::failed(
        ErrorCode::Io,
        "directory undo committed; retry RemoveRoot to finish participant recovery",
    );
    recovery.details.recovery_command = Some("remove_root".into());
    g.err("err_directory_undo_recovery", 49, recovery);
    let mut needs = error(
        ErrorKind::NeedsConfirmation,
        ErrorCode::NeedsConfirmation,
        "waiting for the user to confirm",
    );
    needs.details.confirmation = Some(ConfirmationTicket {
        token: "c-5f0e2a9b".into(),
        summary: "Add the music folder /home/me/Music".into(),
        expires_at_ms: NOW_MS + 300_000,
    });
    g.err("err_needs_confirmation", 22, needs);

    // Agent permissions.
    g.ok(
        "ok_register_agent",
        60,
        Receipt {
            agent_token: Some(
                "pmpa_3f9c1e7a5b2d4c6e8f0a1b3c5d7e9f2a4b6c8d0e1f3a5b7c9d2e4f6a8b0c1d3e".into(),
            ),
            ..receipt(true, 21)
        },
    );
    g.ok(
        "ok_grant_permission",
        63,
        Receipt {
            grant: Some(GrantId(31)),
            ..receipt(true, 21)
        },
    );
    // No `action`: its audit entry (5137) is only listed by `Actions` (a receipt's action
    // can be undone).
    g.ok("ok_purge_playlists", 41, receipt(true, 21));
    g.err(
        "err_permission_denied",
        70,
        ContractError::forbidden(
            ErrorCode::PermissionDenied,
            concat!(
                "the user forbade agent:claude this command (add_root); forbidden by grant ",
                "33 (local_path commands: all of what it acts on), grant 35 (add_root: a ",
                "part of what it acts on)",
            ),
        )
        .with_forbidden_by(vec![
            ForbiddenBy {
                grant: GrantId(33),
                operation: Operation::Class {
                    class: OperationClass::LocalPath,
                },
                covers: Covers::All,
            },
            ForbiddenBy {
                grant: GrantId(35),
                operation: Operation::Command {
                    command: "add_root".into(),
                },
                covers: Covers::Part,
            },
        ]),
    );
    g.err(
        "err_session_ended",
        73,
        ContractError::invalid(
            ErrorCode::SessionEnded,
            concat!(
                "claude's session 0f9a6c2e-5b1d-4e8a-9c3f-7d2b6e4a1c58 has ended: a grant ",
                "for it could never apply",
            ),
        )
        .with_field("command.session"),
    );
    g.err(
        "err_not_grantable",
        64,
        ContractError::invalid(
            ErrorCode::NotGrantable,
            concat!(
                "this grant could never apply: an agent may do this without asking anyway, ",
                "or it is never allowed without asking (permanently deleting library data, ",
                "the user's own commands)",
            ),
        )
        .with_field("command.operation"),
    );
    g.err(
        "err_invalid_location",
        71,
        ContractError::invalid(
            ErrorCode::InvalidLocation,
            concat!(
                "\"/home/me/Music/new/../mix.m3u\" is not a location core can act on ",
                "(relative, `..` past what exists, a link to nothing, or longer than 4096 ",
                "characters)",
            ),
        )
        .with_field("command.path"),
    );
    g.err(
        "err_location_changed",
        72,
        ContractError::invalid(
            ErrorCode::LocationChanged,
            concat!(
                "/home/me/Music/Playlists/mix.m3u no longer leads where it did when the ",
                "user was asked: it changed while the command waited",
            ),
        )
        .with_field("command.path"),
    );
    g.err(
        "err_output_failed",
        29,
        ContractError::failed(
            ErrorCode::OutputFailed,
            "output unavailable: default activation and concrete endpoint failed",
        )
        .retryable(),
    );
}

fn query_results(g: &mut Goldens) {
    g.result(
        "snapshot",
        QueryResult::Snapshot {
            snapshot: Box::new(snapshot_playing()),
        },
    );
    g.result(
        "snapshot_idle",
        QueryResult::Snapshot {
            snapshot: Box::new(snapshot_idle()),
        },
    );
    // What the app is told when core's data is in trouble: here the agents and grants
    // could not be read at startup (agents are refused), and the library database later
    // turned out to be damaged.
    g.result(
        "snapshot_problems",
        QueryResult::Snapshot {
            snapshot: Box::new(Snapshot {
                problems: vec![
                    Problem {
                        code: ErrorCode::PermissionsUnavailable,
                        message: "the agents and grants could not be read: database error: \
                                  disk I/O error"
                            .into(),
                        since_ms: NOW_MS - 3_600_000,
                    },
                    Problem {
                        code: ErrorCode::LibraryDamaged,
                        message: "database error: database disk image is malformed".into(),
                        since_ms: NOW_MS,
                    },
                ],
                ..snapshot_idle()
            }),
        },
    );
    g.result(
        "snapshot_output_failed",
        QueryResult::Snapshot {
            snapshot: Box::new(Snapshot {
                problems: vec![Problem {
                    code: ErrorCode::OutputFailed,
                    message: "output unavailable: default activation and concrete endpoint failed"
                        .into(),
                    since_ms: NOW_MS,
                }],
                ..snapshot_idle()
            }),
        },
    );
    g.result(
        "queue_page",
        QueryResult::QueuePage {
            revision: revision(17),
            offset: 0,
            total: 9,
            next_offset: Some(3),
            entries: vec![
                QueueEntry {
                    entry: entry(1),
                    track: unavailable_track(),
                },
                QueueEntry {
                    entry: entry(2),
                    track: so_what(),
                },
                QueueEntry {
                    entry: entry(3),
                    track: freddie(),
                },
            ],
        },
    );
    g.result(
        "search",
        QueryResult::Search {
            results: SearchResults {
                total: 8,
                offset: 0,
                next_offset: None,
                tracks: vec![so_what()],
                albums: vec![album_summary()],
                artists: vec![ArtistSummary {
                    key: None,
                    name: "Miren Dalen".into(),
                    album_count: 2,
                    track_count: 10,
                }],
                tracks_total: 5,
                albums_total: 2,
                artists_total: 1,
            },
        },
    );
    g.result(
        "search_tracks_page",
        QueryResult::Search {
            results: SearchResults {
                total: 5,
                offset: 1,
                next_offset: Some(2),
                tracks: vec![freddie()],
                albums: vec![],
                artists: vec![],
                tracks_total: 5,
                albums_total: 2,
                artists_total: 1,
            },
        },
    );
    g.result(
        "albums",
        QueryResult::Albums {
            albums: vec![album_summary(), unknown_album_summary()],
            offset: 0,
            total: 412,
            next_offset: Some(2),
        },
    );
    g.result(
        "album",
        QueryResult::Album {
            album: AlbumDetail {
                total: album_summary().track_count,
                offset: 0,
                next_offset: Some(2),
                summary: album_summary(),
                tracks: vec![so_what(), freddie()],
            },
        },
    );
    g.result(
        "tracks",
        QueryResult::Tracks {
            tracks: vec![with_tags(so_what()), hires_track()],
            offset: 0,
            total: 7,
            next_offset: Some(2),
        },
    );
    g.result(
        "facet",
        QueryResult::Facet {
            values: vec![
                FacetValue {
                    value: Some(Value::Text("Jazz".into())),
                    track_count: 120,
                },
                FacetValue {
                    value: Some(Value::Text("Bossa Nova".into())),
                    track_count: 30,
                },
                FacetValue {
                    value: None,
                    track_count: 4,
                },
            ],
            offset: 0,
            total: 3,
            next_offset: None,
        },
    );
    g.result(
        "facet_years",
        QueryResult::Facet {
            values: vec![
                FacetValue {
                    value: Some(Value::Int(1959)),
                    track_count: 10,
                },
                FacetValue {
                    value: Some(Value::Int(1964)),
                    track_count: 8,
                },
            ],
            offset: 0,
            total: 14,
            next_offset: Some(2),
        },
    );
    g.result(
        "playlists",
        QueryResult::Playlists {
            playlists: vec![smart_playlist(), manual_playlist(), broken_smart_playlist()],
            offset: 3,
            total: 9,
            next_offset: Some(6),
        },
    );
    g.result(
        "playlists_trash",
        QueryResult::Playlists {
            playlists: vec![PlaylistSummary {
                id: PlaylistId(9),
                name: "Old mix".into(),
                deleted_at_ms: Some(NOW_MS - 3 * DAY_MS),
                ..manual_playlist()
            }],
            offset: 0,
            total: 1,
            next_offset: None,
        },
    );
    g.result(
        "playlist_items",
        QueryResult::PlaylistItems {
            playlist: manual_playlist(),
            items: vec![
                PlaylistEntry {
                    item: Some(PlaylistItemId(301)),
                    track: with_tags(so_what()),
                },
                PlaylistEntry {
                    item: Some(PlaylistItemId(302)),
                    track: unavailable_track(),
                },
            ],
            offset: 0,
            total: 2,
            next_offset: None,
        },
    );
    g.result(
        "playlist_items_smart",
        QueryResult::PlaylistItems {
            playlist: smart_playlist(),
            items: vec![PlaylistEntry {
                item: None,
                track: so_what(),
            }],
            offset: 0,
            total: 64,
            next_offset: Some(1),
        },
    );
    g.result(
        "artists",
        QueryResult::Artists {
            indexing: None,
            artists: vec![
                ArtistSummary {
                    key: None,
                    name: "Miren Dalen".into(),
                    album_count: 2,
                    track_count: 10,
                },
                ArtistSummary {
                    key: None,
                    name: "Soren Glen & João Galveno".into(),
                    album_count: 1,
                    track_count: 8,
                },
            ],
            offset: 0,
            total: 230,
            next_offset: Some(2),
        },
    );
    let played = |id: i64, track: TrackInfo, hours_ago: i64, played_ms: u64, ended| HistoryItem {
        id: PlayEventId(id),
        track,
        started_at_ms: NOW_MS - hours_ago * 3_600_000,
        played_ms,
        ended,
    };
    g.result(
        "history",
        QueryResult::History {
            items: vec![
                played(9004, so_what(), 1, 562_000, EndReason::Completed),
                played(9003, freddie(), 2, 31_000, EndReason::Skipped),
                played(9002, mp3_track(), 3, 120_500, EndReason::Stopped),
                played(9001, unavailable_track(), 4, 0, EndReason::Failed),
            ],
            offset: 0,
            total: 5_120,
            next_offset: Some(4),
        },
    );
    g.result(
        "roots",
        QueryResult::Roots {
            roots: vec![
                RootInfo {
                    released: false,
                    id: RootId(1),
                    path: "/home/me/Music".into(),
                    track_count: 4_800,
                },
                RootInfo {
                    released: false,
                    id: RootId(3),
                    path: "/mnt/nas/hires".into(),
                    track_count: 312,
                },
            ],
        },
    );
    use ActionKind as K;
    g.result(
        "actions",
        QueryResult::Actions {
            actions: vec![
                ActionSummary {
                    undoable: false,
                    grant: Some(GrantId(32)),
                    details: Some(ActionDetails::Root {
                    root:RootId(1),
                     path:"/home/me/Music".into()}
                    ),

                    ..action(5141,
                     0,
                     agent(),
                     K::RootResume,
                     "request root_resume for library root 1",
                     1,
                     None,
                     None)
                },
                ActionSummary {
                    undoable: false,
                    grant: Some(GrantId(32)),
                    details: Some(ActionDetails::Root {
                    root:RootId(1),
                     path:"/home/me/Music".into()}
                    ),

                    ..action(5140,
                     0,
                     agent(),
                     K::RootRelease,
                     "request root_release for library root 1",
                     1,
                     None,
                     None)
                },
                // An agent's export to a new file is an audit entry too.
                ActionSummary {
                    undoable: false,
                    grant: Some(GrantId(32)),
                    ..action(
                        5139,
                        0,
                        agent(),
                        K::FileWrite,
                        concat!(
"write the new file /home/me/Music/Playlists/road trip.m3u8 from playlist ",
"\"Road trip\"",
),
                        1,
                        None,
                        None,
                    )
                },
                ActionSummary {
                    undoable: false,
                    grant: Some(GrantId(32)),
                    ..action(
                        5138,
                        0,
                        agent(),
                        K::FileWrite,
                        "replace the file /home/me/Music/Playlists/late night.m3u8 with playlist \"Late night\"",
                        1,
                        None,
                        None,
                    )
                },
                ActionSummary {
                    undoable: false,
                    ..action(
                        5137,
                        0,
                        Origin::Ui,
                        K::Purge,
                        "permanently delete playlist \"Old mix\"",
                        1,
                        None,
                        None,
                    )
                },
                ActionSummary {
                    grant: Some(GrantId(31)),
                    ..action(
                        5136,
                        1,
                        agent(),
                        K::RootAdd,
                        "add library folder /home/me/Music/Jazz",
                        1,
                        None,
                        None,
                    )
                },
                action(
                    5135,
                    1,
                    agent(),
                    K::Redo,
                    "redo: rate 2 tracks 5 stars",
                    2,
                    None,
                    Some(5134),
                ),
                action(
                    5134,
                    2,
                    Origin::Ui,
                    K::Undo,
                    "undo: rate 2 tracks 5 stars",
                    2,
                    Some(5135),
                    Some(5127),
                ),
                action(
                    5133,
                    3,
                    Origin::System,
                    K::RootRemove,
                    "remove the music folder /mnt/usb",
                    1,
                    None,
                    None,
                ),
                action(
                    5132,
                    4,
                    Origin::Ui,
                    K::RootAdd,
                    "add the music folder /mnt/nas/hires",
                    1,
                    None,
                    None,
                ),
                action(
                    5131,
                    5,
                    remote(),
                    K::PlaylistRestore,
                    "restore the playlist Old mix",
                    1,
                    None,
                    None,
                ),
                action(
                    5130,
                    6,
                    remote(),
                    K::PlaylistDelete,
                    "move the playlist Old mix to the trash",
                    1,
                    None,
                    None,
                ),
                action(
                    5129,
                    7,
                    agent(),
                    K::PlaylistSetQuery,
                    "change the query of Forgotten favourites",
                    1,
                    None,
                    None,
                ),
                action(
                    5128,
                    8,
                    agent(),
                    K::PlaylistMove,
                    "move 1 item in Late night",
                    1,
                    None,
                    None,
                ),
                action(
                    5127,
                    9,
                    agent(),
                    K::SetRating,
                    "rate 2 tracks 5 stars",
                    2,
                    Some(5134),
                    None,
                ),
                action(
                    5126,
                    10,
                    Origin::MediaControls,
                    K::SetLoved,
                    "love 1 track",
                    1,
                    None,
                    None,
                ),
                action(
                    5125,
                    11,
                    Origin::Ui,
                    K::PlaylistImport,
                    "import the playlist Road trip (2 tracks)",
                    1,
                    None,
                    None,
                ),
                action(
                    5124,
                    12,
                    Origin::Ui,
                    K::PlaylistRemove,
                    "remove 1 item from Late night",
                    1,
                    None,
                    None,
                ),
                action(
                    5123,
                    13,
                    remote(),
                    K::PlaylistAdd,
                    "add 2 tracks to Late night",
                    1,
                    None,
                    None,
                ),
                action(
                    5122,
                    14,
                    Origin::Ui,
                    K::PlaylistCreate,
                    "create the playlist Late night",
                    1,
                    None,
                    None,
                ),
                action(
                    5121,
                    15,
                    Origin::Ui,
                    K::PlaylistRename,
                    "rename the playlist Late night",
                    1,
                    None,
                    None,
                ),
            ],
            offset: 0,
            total: 2_310,
            next_offset: Some(15),
        },
    );
    g.result(
        "err",
        QueryResult::Err {
            error: ContractError::invalid(
                ErrorCode::InvalidQuery,
                "`contains` needs a text field, `year` is a number",
            )
            .with_field("query.filter.all[1].field"),
        },
    );
}

fn permission_results(g: &mut Goldens) {
    g.result(
        "agents",
        QueryResult::Agents {
            agents: vec![
                AgentInfo {
                    name: "claude".into(),
                    created_at_ms: NOW_MS - 30 * DAY_MS,
                    last_seen_at_ms: Some(NOW_MS - 60_000),
                },
                AgentInfo {
                    name: "codex".into(),
                    created_at_ms: NOW_MS - DAY_MS,
                    last_seen_at_ms: None,
                },
            ],
        },
    );
    let class = |class| Operation::Class { class };
    g.result(
        "permissions",
        QueryResult::Permissions {
            grants: vec![
                grant(
                    31,
                    Some("conn-7f3a"),
                    class(OperationClass::LocalPath),
                    Scope::Location {
                        location: "/home/me/Music".into(),
                    },
                    Expiry::Session,
                    Effect::Allow,
                ),
                grant(
                    32,
                    None,
                    Operation::Command {
                        command: "export_m3u".into(),
                    },
                    Scope::Library {
                        root: RootId(1),
                        prefix: Some("Playlists".into()),
                    },
                    Expiry::At {
                        at_ms: NOW_MS + 7 * DAY_MS,
                    },
                    Effect::Allow,
                ),
                grant(
                    33,
                    None,
                    class(OperationClass::FileWrite),
                    Scope::Library {
                        root: RootId(2),
                        prefix: None,
                    },
                    Expiry::Forever,
                    Effect::Forbid,
                ),
                // Shape only: forbidding classes whose commands never ask (by default
                // allowed, or the user's own) changes nothing, but is accepted.
                grant(
                    34,
                    None,
                    class(OperationClass::LibraryEdit),
                    Scope::All,
                    Expiry::Forever,
                    Effect::Forbid,
                ),
                grant(
                    35,
                    None,
                    class(OperationClass::Playback),
                    Scope::All,
                    Expiry::Forever,
                    Effect::Forbid,
                ),
                grant(
                    36,
                    None,
                    class(OperationClass::Purge),
                    Scope::All,
                    Expiry::Forever,
                    Effect::Forbid,
                ),
                grant(
                    37,
                    None,
                    class(OperationClass::Permissions),
                    Scope::All,
                    Expiry::Forever,
                    Effect::Forbid,
                ),
            ],
            offset: 0,
            total: 9,
            next_offset: Some(7),
        },
    );
}

fn lyrics_results(g: &mut Goldens) {
    let line = |start_ms, text: &str| LyricsLine {
        start_ms,
        text: text.into(),
        words: Vec::new(),
        translation: None,
    };
    let word = |start_ms, text: &str| LyricsWord {
        start_ms,
        text: text.into(),
    };
    // A file with word timing on one line and a translation on another.
    g.result(
        "lyrics",
        QueryResult::Lyrics {
            lyrics: Some(Lyrics {
                source: LyricsSource::File,
                format: LyricsFormat::EnhancedLrc,
                lines: vec![
                    line(Some(0), ""),
                    LyricsLine {
                        words: vec![
                            word(12_300, "Hel"),
                            word(12_650, "lo "),
                            word(13_100, "world"),
                        ],
                        ..line(Some(12_300), "Hello world")
                    },
                    LyricsLine {
                        translation: Some("再见".into()),
                        ..line(Some(15_000), "Goodbye")
                    },
                ],
                raw: "[00:00.00]\n[00:12.30]<00:12.30>Hel<00:12.65>lo <00:13.10>world\n\
                      [00:15.00]Goodbye\n[00:15.00]再见\n"
                    .into(),
            }),
        },
    );
    // Timed lines from the tags, no words.
    g.result(
        "lyrics_lrc",
        QueryResult::Lyrics {
            lyrics: Some(Lyrics {
                source: LyricsSource::Embedded,
                format: LyricsFormat::Lrc,
                lines: vec![line(Some(1_000), "First"), line(Some(4_500), "Second")],
                raw: "[00:01.00]First\n[00:04.50]Second".into(),
            }),
        },
    );
    g.result(
        "lyrics_plain",
        QueryResult::Lyrics {
            lyrics: Some(Lyrics {
                source: LyricsSource::Embedded,
                format: LyricsFormat::Plain,
                lines: vec![
                    line(None, "First line"),
                    line(None, ""),
                    line(None, "Second"),
                ],
                raw: "First line\n\nSecond".into(),
            }),
        },
    );
    g.result("lyrics_none", QueryResult::Lyrics { lyrics: None });
}

fn waveform_results(g: &mut Goldens) {
    // A real waveform has 2048 buckets; four keep the example short.
    g.result(
        "waveform",
        QueryResult::Waveform {
            waveform: Some(Waveform {
                buckets: 4,
                channels: vec![
                    WaveformChannel {
                        min: vec![-3, -61, -128, 0],
                        max: vec![4, 58, 127, 0],
                        rms: vec![2, 40, 181, 0],
                    },
                    WaveformChannel {
                        min: vec![-2, -64, -120, 0],
                        max: vec![2, 63, 119, 0],
                        rms: vec![1, 45, 170, 0],
                    },
                ],
            }),
        },
    );
    g.result("waveform_none", QueryResult::Waveform { waveform: None });
}

fn waveform_events(g: &mut Goldens) {
    g.event(
        "waveform_ready",
        5010,
        None,
        Event::WaveformReady { track: TrackId(42) },
    );
}

// --- minor 1: CUE sheets ---

/// A track of a CUE sheet: `n`, from `start_ms` to `end_ms` of the image `Live.flac`.
fn cue_track(id: i64, n: u32, title: &str, start_ms: u64, end_ms: u64) -> TrackInfo {
    TrackInfo {
        id: TrackId(id),
        title: title.into(),
        artist: Some("Kellan Jorven".into()),
        album: Some("The Nölm Compass".into()),
        album_artist: Some("Kellan Jorven".into()),
        group_artist: Some("Kellan Jorven".into()),
        album_key: album_key("kellan jorven", "the nolm compass"),
        track_no: Some(n),
        track_total: Some(4),
        year: Some(1975),
        genre: Some("Jazz".into()),
        duration_ms: Some(end_ms - start_ms),
        codec: Some("flac".into()),
        sample_rate: Some(44_100),
        channels: Some(2),
        bits_per_sample: Some(16),
        bitrate_kbps: Some(745),
        lossless: true,
        root: RootId(1),
        path: "Kellan Jorven/The Nölm Compass/Live.flac".into(),
        size_bytes: 368_934_210,
        modified_at_ms: 1_700_000_000_000,
        added_at_ms: 1_745_000_000_000,
        available: true,
        has_cover: true,
        play_count: 3,
        cue: Some(TrackCue {
            sheet: "Kellan Jorven/The Nölm Compass/Live.cue".into(),
            number: n,
            start_ms,
            end_ms: Some(end_ms),
        }),
        ..Default::default()
    }
}

fn cue_queries(g: &mut Goldens) {
    g.query(
        "tracks_from_cue_sheets",
        Query::Tracks {
            query: TrackQuery {
                filter: Filter::Compare {
                    field: Field::CueSheet,
                    op: CompareOp::Eq,
                    value: Value::Bool(true),
                },
                sort: Vec::new(),
            },
            offset: 0,
            limit: 100,
            tags: Vec::new(),
        },
    );
}

fn cue_results(g: &mut Goldens) {
    g.result(
        "tracks_from_cue_sheets",
        QueryResult::Tracks {
            tracks: vec![
                cue_track(3101, 1, "Part I", 0, 1_559_973),
                cue_track(3102, 2, "Part II a", 1_559_973, 2_456_826),
            ],
            offset: 0,
            total: 4,
            next_offset: None,
        },
    );
}

fn events(g: &mut Goldens) {
    g.event(
        "state_changed",
        4190,
        Some(Origin::Ui),
        Event::StateChanged {
            snapshot: Box::new(snapshot_paused()),
        },
    );
    let started = hires_track();
    g.event(
        "track_started",
        4191,
        None,
        Event::TrackStarted {
            entry: entry(4),
            signal_path: Some(Box::new(signal_path(
                &started,
                Vec::new(),
                96_000,
                SampleFormat::I24,
                BitPerfectInfo::Yes,
            ))),
            track: Box::new(started),
        },
    );
    g.event(
        "position",
        4192,
        Some(Origin::MediaControls),
        Event::Position {
            position: PositionInfo {
                entry: entry(4),
                position_ms: 15_000,
                at_ms: 3_700_000,
                rate: 1.0,
            },
        },
    );
    g.event(
        "queue_changed",
        4193,
        Some(agent()),
        Event::QueueChanged {
            queue: QueueSummary {
                revision: revision(18),
                entry_count: 11,
                current_index: Some(3),
                next_entry: Some(entry(5)),
                natural_next_entry: Some(entry(5)),
            },
        },
    );
    g.event(
        "library_changed",
        4194,
        Some(Origin::System),
        Event::LibraryChanged,
    );
    g.event("history_changed", 4195, None, Event::HistoryChanged);
    g.event(
        "playlists_changed",
        4196,
        Some(remote()),
        Event::PlaylistsChanged {
            playlists: vec![PlaylistId(12), PlaylistId(15)],
        },
    );
    g.event(
        "tracks_changed",
        4197,
        Some(agent()),
        Event::TracksChanged {
            tracks: vec![TrackId(1201), TrackId(1202)],
        },
    );
    g.event(
        "actions_changed",
        4198,
        Some(agent()),
        Event::ActionsChanged,
    );
    g.event(
        "job_progress",
        4199,
        Some(Origin::Ui),
        Event::JobProgress {
            job: JobId(7),
            kind: JobKind::Scan,
            done: 1_250,
            total: Some(4_800),
        },
    );
    g.event(
        "job_finished",
        4250,
        Some(Origin::Ui),
        Event::JobFinished {
            job: JobId(7),
            kind: JobKind::Scan,
            outcome: JobOutcome::Completed {
                report: JobReport::Scan {
                    added_count: 4_790,
                    updated_count: 3,
                    restored_count: 2,
                    removed_count: 1,
                    error_count: 2,
                    first_errors: vec![
                        ScanErrorInfo {
                            code: None,
                            path: "/home/me/Music/broken.flac".into(),
                            message: "not a FLAC stream".into(),
                        },
                        ScanErrorInfo {
                            code: None,
                            path: "/home/me/Music/private".into(),
                            message: "permission denied".into(),
                        },
                    ],
                },
            },
        },
    );
    g.event(
        "job_finished_cancelled",
        4251,
        Some(Origin::Ui),
        Event::JobFinished {
            job: JobId(8),
            kind: JobKind::Scan,
            outcome: JobOutcome::Cancelled,
        },
    );
    g.event(
        "job_finished_failed",
        4252,
        None,
        Event::JobFinished {
            job: JobId(9),
            kind: JobKind::Scan,
            outcome: JobOutcome::Failed {
                error: ContractError::failed(ErrorCode::Db, "database is locked").retryable(),
            },
        },
    );
    g.event(
        "track_failed",
        4253,
        None,
        Event::TrackFailed {
            entry: entry(5),
            code: ErrorCode::FileMissing,
            message: "/home/me/Music/old rips/track03.mp3 no longer exists".into(),
            upcoming: true,
        },
    );
    g.event(
        "confirmation_requested",
        4254,
        Some(agent()),
        Event::ConfirmationRequested {
            confirmation: Box::new(PendingConfirmation {
                token: "c-9a4b7d21".into(),
                summary: "Permanently delete 1 playlist in the trash (Old mix)".into(),
                command: Command::PurgePlaylists {
                    playlists: Some(vec![PlaylistId(9)]),
                },
                requested_by: agent(),
                expires_at_ms: NOW_MS + 300_000,
                operation: Some(OperationInfo {
                    class: OperationClass::Purge,
                    command: "purge_playlists".into(),
                }),
                scope: None,
                display_location: None,
            }),
        },
    );
    // What a UI needs for "allow claude to access local paths in /home/me/Music for this
    // session": a `GrantPermission` with the requester's session, then `Confirm`.
    g.event(
        "confirmation_requested_add_root",
        4256,
        Some(agent_session()),
        Event::ConfirmationRequested {
            confirmation: Box::new(PendingConfirmation {
                token: "c-2b8e6f10".into(),
                summary: "add the folder /home/me/Music/Jazz to the library".into(),
                command: Command::AddRoot {
                    path: "/home/me/Music/Jazz".into(),
                },
                requested_by: agent_session(),
                expires_at_ms: NOW_MS + 300_000,
                operation: Some(OperationInfo {
                    class: OperationClass::LocalPath,
                    command: "add_root".into(),
                }),
                scope: Some(Scope::Location {
                    location: "/home/me/Music/Jazz".into(),
                }),
                display_location: Some("/home/me/Music/Jazz".into()),
            }),
        },
    );
    g.event(
        "permissions_changed",
        4257,
        Some(Origin::Ui),
        Event::PermissionsChanged,
    );
    g.event(
        "agents_changed",
        4258,
        Some(Origin::Ui),
        Event::AgentsChanged,
    );
    g.event(
        "confirmation_resolved",
        4255,
        Some(agent()),
        Event::ConfirmationResolved {
            outcome: ConfirmationOutcome {
                token: "c-9a4b7d21".into(),
                approved: true,
                result: Some(Box::new(CommandResult::Ok(receipt(true, 18)))),
                reason: Some(ConfirmationReason::Approved),
            },
        },
    );
    g.event(
        "confirmation_resolved_expired",
        4300,
        None,
        Event::ConfirmationResolved {
            outcome: ConfirmationOutcome {
                token: "c-1c2d3e4f".into(),
                approved: false,
                result: None,
                reason: Some(ConfirmationReason::Expired),
            },
        },
    );
    g.event(
        "confirmation_resolved_withdrawn",
        4301,
        None,
        Event::ConfirmationResolved {
            outcome: ConfirmationOutcome {
                token: "c-5e6f7a8b".into(),
                approved: false,
                result: None,
                reason: Some(ConfirmationReason::Withdrawn),
            },
        },
    );
    g.event(
        "resync",
        5003,
        None,
        Event::Resync {
            snapshot: Box::new(snapshot_lossy()),
        },
    );
}

// --- minor 1: tag editing (docs/design/media-resources.md) ---

fn tag_changes() -> Vec<TagChange> {
    vec![
        TagChange::Set {
            key: "ARTIST".into(),
            values: vec!["Miren Dalen".into(), "Jorin Calven".into()],
        },
        TagChange::Remove {
            key: "COMMENT".into(),
        },
    ]
}

fn tag_edit_requests(g: &mut Goldens) {
    g.request(
        "edit_tags",
        Some("ui-tags-1"),
        Command::EditTags {
            tracks: vec![TrackId(42), TrackId(43), TrackId(44)],
            changes: tag_changes(),
        },
    );
}

fn tag_edit_queries(g: &mut Goldens) {
    g.query(
        "tag_edit_preview",
        Query::TagEditPreview {
            tracks: vec![TrackId(42), TrackId(43), TrackId(44)],
            changes: tag_changes(),
        },
    );
}

fn tag_edit_responses(g: &mut Goldens) {
    // A job: what it writes, and the action, come with its `JobFinished`.
    g.ok(
        "ok_edit_tags",
        61,
        Receipt {
            job: Some(JobId(9)),
            ..receipt(true, 17)
        },
    );
    g.err(
        "err_invalid_tag",
        62,
        error(
            ErrorKind::Invalid,
            ErrorCode::InvalidTag,
            "changes[0].key: the library shows TPE1 as ARTIST: change that key",
        )
        .with_field("command.changes[0].key"),
    );
}

fn tag_edit_results(g: &mut Goldens) {
    let tags = |pairs: &[(&str, &[&str])]| -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(k, vs)| {
                (
                    (*k).to_owned(),
                    vs.iter().map(|v| (*v).to_owned()).collect(),
                )
            })
            .collect()
    };
    g.result(
        "tag_edit_preview",
        QueryResult::TagEditPreview {
            files: vec![TagFilePreview {
                changed: Some(true),
                encoding_conversion: Some(true),
                display_location: Some("/music/Jazz/Kites of Brass/Soft Wheels.flac".into()),
                track: TrackId(42),
                before: tags(&[
                    ("ARTIST", &["Miren Dalen"]),
                    ("COMMENT", &["remastered"]),
                    ("TITLE", &["Soft Wheels"]),
                ]),
                after: tags(&[
                    ("ARTIST", &["Miren Dalen", "Jorin Calven"]),
                    ("TITLE", &["Soft Wheels"]),
                ]),
            }],
            skipped: vec![
                TagFileResult {
                    track: TrackId(43),
                    code: ErrorCode::FileChanged,
                },
                TagFileResult {
                    track: TrackId(44),
                    code: ErrorCode::UnsupportedFormat,
                },
            ],
        },
    );
    g.result(
        "actions_tag_edit",
        QueryResult::Actions {
            actions: vec![action(
                5160,
                2,
                agent(),
                ActionKind::TagEdit,
                "edit the tags of 2 files: set ARTIST, remove COMMENT",
                2,
                None,
                None,
            )],
            offset: 0,
            total: 1,
            next_offset: None,
        },
    );
}

fn tag_edit_events(g: &mut Goldens) {
    g.event(
        "job_progress_tag_edit",
        5020,
        Some(Origin::Ui),
        Event::JobProgress {
            job: JobId(9),
            kind: JobKind::TagEdit,
            done: 1,
            total: Some(3),
        },
    );
    g.event(
        "job_finished_tag_edit",
        5023,
        Some(Origin::Ui),
        Event::JobFinished {
            job: JobId(9),
            kind: JobKind::TagEdit,
            outcome: JobOutcome::Completed {
                report: JobReport::TagEdit {
                    written: Some(vec![TrackId(42)]),
                    encoding_converted: Some(vec![TrackId(42)]),
                    written_count: 1,
                    action: Some(ActionId(5160)),
                    skipped: vec![TagFileResult {
                        track: TrackId(43),
                        code: ErrorCode::FileInUse,
                    }],
                    failed: vec![TagFileResult {
                        track: TrackId(44),
                        code: ErrorCode::WriteFailed,
                    }],
                },
            },
        },
    );
    // Undoing a tag edit whose file changed since: nothing is written back.
    g.event(
        "job_finished_tag_undo_conflict",
        5030,
        Some(Origin::Ui),
        Event::JobFinished {
            job: JobId(10),
            kind: JobKind::TagEdit,
            outcome: JobOutcome::Failed {
                error: error(
                    ErrorKind::Conflict,
                    ErrorCode::ActionConflict,
                    "action 5160 cannot be undone: 1 of its objects changed since",
                )
                .with_changed(vec![ObjectRef::Track { id: TrackId(42) }]),
            },
        },
    );
    g.event(
        "confirmation_requested_edit_tags",
        5010,
        Some(agent_session()),
        Event::ConfirmationRequested {
            confirmation: Box::new(PendingConfirmation {
                token: "c-5d1e0a77".into(),
                summary: "rewrite the tags of 3 files: set ARTIST to \"Miren Dalen\", \
                          \"Jorin Calven\"; remove COMMENT; 1 file will have legacy text \
                          converted to standard Unicode encoding"
                    .into(),
                command: Command::EditTags {
                    tracks: vec![TrackId(42), TrackId(43), TrackId(44)],
                    changes: tag_changes(),
                },
                requested_by: agent_session(),
                expires_at_ms: NOW_MS + 300_000,
                operation: Some(OperationInfo {
                    class: OperationClass::FileWrite,
                    command: "edit_tags".into(),
                }),
                scope: Some(Scope::Library {
                    root: RootId(1),
                    prefix: Some("Miren Dalen/Kites of Brass".into()),
                }),
                display_location: Some("/home/me/Music/Miren Dalen/Kites of Brass".into()),
            }),
        },
    );
}

fn loudness_examples(g: &mut Goldens) {
    g.request(
        "release_root",
        None,
        Command::ReleaseRoot { root: RootId(1) },
    );
    g.request("resume_root", None, Command::ResumeRoot { root: RootId(1) });
    for (name, kind) in [
        ("search_tracks", SearchKind::Tracks),
        ("search_albums", SearchKind::Albums),
        ("search_artists", SearchKind::Artists),
    ] {
        g.query(
            name,
            Query::Search {
                artist_view: None,
                text: "Miren".into(),
                kind: Some(kind),
                offset: 1,
                limit: 2,
            },
        );
    }
    g.query("loudness", Query::Loudness { track: TrackId(42) });
    g.result(
        "loudness",
        QueryResult::Loudness {
            loudness: Some(LoudnessInfo {
                integrated_lufs: Some(-24.5),
                sample_peak: 0.5,
                album_lufs: Some(-20.0),
                album_peak: Some(0.9),
                measured_at_ms: NOW_MS,
            }),
        },
    );
    g.result(
        "loudness_silent",
        QueryResult::Loudness {
            loudness: Some(LoudnessInfo {
                integrated_lufs: None,
                sample_peak: 0.0,
                album_lufs: None,
                album_peak: None,
                measured_at_ms: NOW_MS,
            }),
        },
    );
    g.result("loudness_none", QueryResult::Loudness { loudness: None });
    g.event(
        "job_progress_loudness",
        5040,
        None,
        Event::JobProgress {
            job: JobId(12),
            kind: JobKind::LoudnessScan,
            done: 1,
            total: None,
        },
    );
    g.event(
        "job_finished_loudness",
        5041,
        None,
        Event::JobFinished {
            job: JobId(12),
            kind: JobKind::LoudnessScan,
            outcome: JobOutcome::Completed {
                report: JobReport::LoudnessScan {
                    measured_count: 1,
                    album_count: 0,
                    silent_count: 0,
                    failed: vec![TagFileResult {
                        track: TrackId(43),
                        code: ErrorCode::FileChanged,
                    }],
                },
            },
        },
    );
}

fn minor_2(g: &mut Goldens) {
    g.result(
        "err_query_scalar_path",
        QueryResult::Err {
            error: ContractError::invalid(
                ErrorCode::InvalidRequest,
                "invalid type: string \"ten\", expected a boolean",
            )
            .with_field("query.sort[0].descending"),
        },
    );
    // Syntax failures keep serde's actual 1-based position, without a guessed field.
    let syntax_error =
        serde_json::from_str::<serde_json::Value>("{\n\"type\":\"tracks\",\"offset\":0,,}")
            .unwrap_err();
    let mut error = ContractError::invalid(ErrorCode::InvalidJson, syntax_error.to_string());
    error.details.line = Some(syntax_error.line() as u32);
    error.details.column = Some(syntax_error.column() as u32);
    g.result("err_query_json_position", QueryResult::Err { error });
    g.err(
        "err_grant_location_not_normal",
        74,
        ContractError::invalid(
            ErrorCode::LocationNotNormal,
            "grant location is not normal; normalize and review the scope again",
        )
        .with_field("command.scope.location"),
    );
    g.query("job", Query::Job { job: JobId(7) });
    g.query(
        "jobs",
        Query::Jobs {
            offset: 0,
            limit: 4,
        },
    );
    let codes = [
        ScanErrorCode::RootUnreachable,
        ScanErrorCode::NoAudioFiles,
        ScanErrorCode::DirectoryReadFailed,
        ScanErrorCode::InvalidPath,
        ScanErrorCode::StatFailed,
        ScanErrorCode::AudioReadFailed,
        ScanErrorCode::CueReadFailed,
        ScanErrorCode::ReaderPanicked,
        ScanErrorCode::CueMissingFiles,
    ];
    let scan_path = |code| match code {
        ScanErrorCode::RootUnreachable => "/music/offline",
        ScanErrorCode::NoAudioFiles => "/music/empty",
        ScanErrorCode::DirectoryReadFailed => "/music/restricted",
        ScanErrorCode::InvalidPath => "/music/invalid-name",
        ScanErrorCode::StatFailed => "/music/stat.flac",
        ScanErrorCode::AudioReadFailed => "/music/audio.flac",
        ScanErrorCode::CueReadFailed => "/music/sheet.cue",
        ScanErrorCode::ReaderPanicked => "/music/panic.flac",
        ScanErrorCode::CueMissingFiles => "/music/missing.cue",
        _ => unreachable!(),
    };
    let report = JobReport::Scan {
        added_count: 0,
        updated_count: 0,
        restored_count: 0,
        removed_count: 0,
        error_count: codes.len() as u32,
        first_errors: codes
            .into_iter()
            .map(|code| ScanErrorInfo {
                code: Some(code),
                path: scan_path(code).into(),
                message: "could not read file or folder".into(),
            })
            .collect(),
    };
    let jobs = vec![
        JobInfo {
            id: JobId(10),
            kind: JobKind::TagEdit,
            state: JobState::Running,
            progress: Some(JobProgressInfo {
                done: 1,
                total: Some(2),
            }),
            outcome: None,
        },
        JobInfo {
            id: JobId(9),
            kind: JobKind::Scan,
            state: JobState::Completed,
            progress: None,
            outcome: Some(JobOutcome::Completed { report }),
        },
        JobInfo {
            id: JobId(8),
            kind: JobKind::LoudnessScan,
            state: JobState::Cancelled,
            progress: Some(JobProgressInfo {
                done: 3,
                total: None,
            }),
            outcome: Some(JobOutcome::Cancelled),
        },
        JobInfo {
            id: JobId(7),
            kind: JobKind::TagEdit,
            state: JobState::Failed,
            progress: None,
            outcome: Some(JobOutcome::Failed {
                error: ContractError::failed(ErrorCode::Io, "cannot write tags"),
            }),
        },
    ];
    g.result(
        "job",
        QueryResult::Job {
            job: jobs[0].clone(),
        },
    );
    g.result(
        "job_cancelled",
        QueryResult::Job {
            job: jobs[2].clone(),
        },
    );
    g.result(
        "job_failed",
        QueryResult::Job {
            job: jobs[3].clone(),
        },
    );
    g.result(
        "jobs",
        QueryResult::Jobs {
            jobs: jobs
                .into_iter()
                .map(|job| JobSummary {
                    id: job.id,
                    kind: job.kind,
                    state: job.state,
                    progress: job.progress,
                    counts: (job.state == JobState::Completed).then_some(JobCounts::Scan {
                        added_count: 0,
                        updated_count: 0,
                        restored_count: 0,
                        removed_count: 0,
                        error_count: 9,
                    }),
                    error: match &job.outcome {
                        Some(JobOutcome::Failed { error }) => Some(JobError {
                            kind: error.kind,
                            code: error.code,
                        }),
                        _ => None,
                    },
                })
                .collect(),
            offset: 0,
            total: 5,
            next_offset: Some(4),
        },
    );
    g.result(
        "jobs_counts",
        QueryResult::Jobs {
            jobs: vec![
                JobSummary {
                    id: JobId(12),
                    kind: JobKind::TagEdit,
                    state: JobState::Completed,
                    progress: Some(JobProgressInfo {
                        done: 1000,
                        total: Some(1000),
                    }),
                    counts: Some(JobCounts::TagEdit {
                        written_count: 997,
                        skipped_count: 2,
                        failed_count: 1,
                    }),
                    error: None,
                },
                JobSummary {
                    id: JobId(11),
                    kind: JobKind::LoudnessScan,
                    state: JobState::Completed,
                    progress: None,
                    counts: Some(JobCounts::LoudnessScan {
                        measured_count: 3,
                        album_count: 1,
                        silent_count: 0,
                        failed_count: 0,
                    }),
                    error: None,
                },
            ],
            total: 2,
            offset: 0,
            next_offset: None,
        },
    );
    g.result(
        "job_scan_codes",
        QueryResult::Job {
            job: JobInfo {
                id: JobId(9),
                kind: JobKind::Scan,
                state: JobState::Completed,
                progress: None,
                outcome: Some(JobOutcome::Completed {
                    report: JobReport::Scan {
                        added_count: 0,
                        updated_count: 0,
                        restored_count: 0,
                        removed_count: 0,
                        error_count: 9,
                        first_errors: codes
                            .into_iter()
                            .map(|code| ScanErrorInfo {
                                code: Some(code),
                                path: scan_path(code).into(),
                                message: "cannot read".into(),
                            })
                            .collect(),
                    },
                }),
            },
        },
    );
    let details = vec![
        ActionDetails::TagEdit {
            changes: vec![
                TagKeyChange::Set {
                    key: "TITLE".into(),
                },
                TagKeyChange::Remove {
                    key: "COMMENT".into(),
                },
            ],
        },
        ActionDetails::Root {
            root: RootId(1),
            path: "/home/me/Music".into(),
        },
        ActionDetails::SetRating { rating: Some(5) },
        ActionDetails::SetRating { rating: None },
        ActionDetails::SetLoved { loved: true },
        ActionDetails::Playlist {
            playlist: PlaylistId(4),
            name: "Road trip".into(),
        },
        ActionDetails::Undo {
            original_kind: ActionKind::TagEdit,
        },
    ];
    let total = details.len() as u32;
    let actions = details
        .into_iter()
        .enumerate()
        .map(|(i, details)| {
            let kind = match &details {
                ActionDetails::TagEdit { .. } => ActionKind::TagEdit,
                ActionDetails::Root { .. } => ActionKind::RootAdd,
                ActionDetails::SetRating { .. } => ActionKind::SetRating,
                ActionDetails::SetLoved { .. } => ActionKind::SetLoved,
                ActionDetails::Playlist { .. } => ActionKind::PlaylistCreate,
                _ => ActionKind::Undo,
            };
            ActionSummary {
                details: Some(details),
                ..action(
                    6000 + i as i64,
                    1,
                    Origin::Ui,
                    kind,
                    "an operation",
                    1,
                    None,
                    None,
                )
            }
        })
        .collect();
    g.result(
        "actions_details",
        QueryResult::Actions {
            actions,
            offset: 0,
            total,
            next_offset: None,
        },
    );
}

fn playback_limits(g: &mut Goldens) {
    g.request(
        "set_stop_after_current",
        None,
        Command::SetStopAfterCurrent {
            enabled: true,
            expected_entry: Some(EntryId(500)),
        },
    );
    g.request(
        "cancel_stop_after_current",
        None,
        Command::SetStopAfterCurrent {
            enabled: false,
            expected_entry: None,
        },
    );
    g.request(
        "set_sleep_timer",
        None,
        Command::SetSleepTimer {
            duration_ms: Some(86_400_000),
        },
    );
    g.request(
        "cancel_sleep_timer",
        None,
        Command::SetSleepTimer { duration_ms: None },
    );
    g.query("playback_limits", Query::PlaybackLimits);
    g.result(
        "playback_limits",
        QueryResult::PlaybackLimits {
            playback_limits: PlaybackLimitsInfo {
                stop_after_current: Some(StopAfterCurrentInfo {
                    entry: EntryId(500),
                }),
                sleep_timer: Some(SleepTimerInfo {
                    remaining_ms: 1234,
                    sampled_at_ms: 100,
                    includes_suspend: Some(true),
                }),
            },
            now_ms: 100,
        },
    );
    for (name, reason, action) in [
        (
            "end_of_current",
            PlaybackLimitReason::EndOfCurrent,
            PlaybackLimitAction::Stop,
        ),
        (
            "sleep_timer",
            PlaybackLimitReason::SleepTimer,
            PlaybackLimitAction::Pause,
        ),
        (
            "current_failed",
            PlaybackLimitReason::CurrentFailed,
            PlaybackLimitAction::Stop,
        ),
    ] {
        g.event(
            &format!("playback_limit_{name}"),
            6000,
            None,
            Event::PlaybackLimitReached {
                reason,
                action,
                entry: Some(EntryId(500)),
            },
        );
    }
    g.event(
        "playback_limit_idle",
        6001,
        None,
        Event::PlaybackLimitReached {
            reason: PlaybackLimitReason::SleepTimer,
            action: PlaybackLimitAction::Pause,
            entry: None,
        },
    );
}

fn output_examples(g: &mut Goldens) {
    let fixed = OutputDeviceSelection::Device {
        id: "cpal:fixed-1".into(),
    };
    g.request(
        "set_output_device_default",
        None,
        Command::SetOutputDevice {
            selection: OutputDeviceSelection::SystemDefault,
            expected_revision: None,
        },
    );
    g.request(
        "set_output_device_fixed",
        None,
        Command::SetOutputDevice {
            selection: fixed.clone(),
            expected_revision: Some(7),
        },
    );
    g.request(
        "set_output_mode_empty",
        None,
        Command::SetOutputMode {
            mode: None,
            fallback: None,
            rate_policy: None,
            expected_revision: None,
        },
    );
    for (name, mode, fallback, rate) in [
        (
            "shared",
            OutputShareMode::Shared,
            OutputFallbackPolicy::Pause,
            OutputRatePolicy::Mix,
        ),
        (
            "exclusive",
            OutputShareMode::Exclusive,
            OutputFallbackPolicy::Shared,
            OutputRatePolicy::Source,
        ),
    ] {
        g.request(
            &format!("set_output_mode_{name}"),
            None,
            Command::SetOutputMode {
                mode: Some(mode),
                fallback: Some(fallback),
                rate_policy: Some(rate),
                expected_revision: Some(7),
            },
        );
    }
    g.query("output_devices", Query::OutputDevices);
    g.query("output_state", Query::OutputState);
    for (name, selection) in [
        ("default", OutputDeviceSelection::SystemDefault),
        ("fixed", fixed.clone()),
    ] {
        g.query(
            &format!("output_capabilities_{name}"),
            Query::OutputDeviceCapabilities { selection },
        );
    }
    let device = OutputDeviceInfo {
        id: "cpal:fixed-1".into(),
        name: "Speakers".into(),
        is_default: true,
        available: true,
        id_stability: OutputDeviceIdStability::Persistent,
    };
    g.result(
        "output_devices",
        QueryResult::OutputDevices {
            generation: 4,
            default_device: Some(device.id.clone()),
            devices: vec![
                device.clone(),
                OutputDeviceInfo {
                    id: "cpal:session-2".into(),
                    name: device.name.clone(),
                    available: false,
                    is_default: false,
                    id_stability: OutputDeviceIdStability::Session,
                },
            ],
        },
    );
    g.result(
        "output_devices_empty",
        QueryResult::OutputDevices {
            generation: 5,
            default_device: None,
            devices: vec![],
        },
    );
    let format = SignalFormat {
        sample_rate: 48000,
        channels: 2,
        bits_per_sample: Some(32),
        valid_bits_per_sample: None,
        sample_format: Some(SampleFormat::F32),
    };
    for (index, backend) in [
        OutputBackend::Cpal,
        OutputBackend::Wasapi,
        OutputBackend::CoreAudio,
        OutputBackend::Alsa,
    ]
    .into_iter()
    .enumerate()
    {
        let future = index != 0;
        g.result(
            &format!("output_capabilities_{index}"),
            QueryResult::OutputDeviceCapabilities {
                generation: 4,
                resolved_device: device.clone(),
                capabilities: OutputDeviceCapabilities {
                    backend,
                    exclusive_access: if future {
                        CapabilitySupport::Supported
                    } else {
                        CapabilitySupport::Unsupported
                    },
                    source_rate_switch: if future {
                        CapabilitySupport::Supported
                    } else {
                        CapabilitySupport::Unsupported
                    },
                    bit_exact_transport: CapabilitySupport::Undetermined,
                    physical_route_known: future,
                    mix_format: if future { None } else { Some(format.clone()) },
                    exclusive_formats: if future {
                        vec![
                            OutputFormatRange {
                                channels: 2,
                                sample_format: SampleFormat::I32,
                                valid_bits_per_sample: Some(24),
                                sample_rate_min: 44100,
                                sample_rate_max: 44100,
                            },
                            OutputFormatRange {
                                channels: 2,
                                sample_format: SampleFormat::F32,
                                valid_bits_per_sample: None,
                                sample_rate_min: 48000,
                                sample_rate_max: 96000,
                            },
                        ]
                    } else {
                        vec![]
                    },
                    formats_complete: !future,
                },
            },
        );
    }
    for (index, status) in [
        OutputStatus::Idle,
        OutputStatus::Ready,
        OutputStatus::Rebuilding,
        OutputStatus::Unavailable,
    ]
    .into_iter()
    .enumerate()
    {
        let active = index == 1;
        let state = OutputStateInfo {
            settings_revision: 7,
            generation: 4,
            status,
            actual: active.then(|| OutputActualInfo {
                device_id: device.id.clone(),
                name: device.name.clone(),
                share_mode: OutputShareMode::Shared,
                backend: OutputBackend::Cpal,
                format: format.clone(),
                physical_device_id: None,
                exclusive_verified: false,
                sample_integrity: SampleIntegrity::Unverified,
            }),
            failure: (index == 3).then(|| OutputFailureInfo {
                code: ErrorCode::OutputDeviceUnavailable,
                message: "Selected endpoint is offline".into(),
                retryable: true,
            }),
            fallback: None,
        };
        g.result(
            &format!("output_state_{index}"),
            QueryResult::OutputState {
                settings: OutputSettings {
                    selection: fixed.clone(),
                    settings_revision: 7,
                    ..Default::default()
                },
                state: state.clone(),
            },
        );
        g.event(
            &format!("output_changed_{index}"),
            7000 + index as u64,
            None,
            Event::OutputChanged { output: state },
        );
    }
    for (index, reason) in [
        OutputFallbackReason::ExclusiveUnavailable,
        OutputFallbackReason::ModeUnsupported,
        OutputFallbackReason::FormatUnsupported,
        OutputFallbackReason::DeviceIdentityExpired,
    ]
    .into_iter()
    .enumerate()
    {
        let actual = (index != 3).then(|| OutputActualInfo {
            device_id: device.id.clone(),
            name: device.name.clone(),
            share_mode: OutputShareMode::Shared,
            backend: OutputBackend::Wasapi,
            format: format.clone(),
            physical_device_id: Some("physical:example".into()),
            exclusive_verified: false,
            sample_integrity: SampleIntegrity::Verified,
        });
        g.result(
            &format!("output_fallback_{index}"),
            QueryResult::OutputState {
                settings: OutputSettings {
                    mode: OutputShareMode::Exclusive,
                    rate_policy: OutputRatePolicy::Source,
                    fallback: OutputFallbackPolicy::Shared,
                    ..Default::default()
                },
                state: OutputStateInfo {
                    status: if index == 3 {
                        OutputStatus::Idle
                    } else {
                        OutputStatus::Ready
                    },
                    actual,
                    fallback: Some(OutputFallbackInfo {
                        reason,
                        requested_mode: OutputShareMode::Exclusive,
                        actual_mode: if index == 3 {
                            None
                        } else {
                            Some(OutputShareMode::Shared)
                        },
                    }),
                    ..Default::default()
                },
            },
        );
    }
    g.event(
        "output_devices_changed",
        7010,
        None,
        Event::OutputDevicesChanged { generation: 5 },
    );
    for code in [
        ErrorCode::OutputSettingsConflict,
        ErrorCode::OutputDeviceNotFound,
        ErrorCode::OutputDeviceUnavailable,
        ErrorCode::OutputModeUnsupported,
        ErrorCode::OutputFormatUnsupported,
        ErrorCode::ExclusiveUnavailable,
    ] {
        g.response(
            &format!("err_{}", code.as_str()),
            7,
            CommandResult::Err {
                error: error(
                    code.kind(),
                    code,
                    "Output configuration could not be satisfied",
                ),
            },
        );
    }
    for (index, code) in [
        SignalReasonCode::PrecisionLoss,
        SignalReasonCode::Dither,
        SignalReasonCode::BackendProcessing,
        SignalReasonCode::PathUnverified,
    ]
    .into_iter()
    .enumerate()
    {
        let mut snapshot = snapshot_playing();
        let path = snapshot
            .current
            .as_mut()
            .unwrap()
            .signal_path
            .as_mut()
            .unwrap();
        path.share_mode = Some(OutputShareMode::Exclusive);
        path.backend = Some(OutputBackend::Wasapi);
        path.output_rate = path.source_rate;
        path.stages.clear();
        path.sample_integrity = Some(SampleIntegrity::Unverified);
        path.reason_codes = vec![code];
        path.bit_perfect = if code == SignalReasonCode::PathUnverified {
            BitPerfectInfo::Uncertain {
                reason: "Path is unverified".into(),
            }
        } else {
            BitPerfectInfo::No {
                reasons: vec!["Samples are altered".into()],
            }
        };
        g.result(
            &format!("snapshot_output_reason_{index}"),
            QueryResult::Snapshot {
                snapshot: Box::new(snapshot),
            },
        );
    }
}

fn cover_examples(g: &mut Goldens) {
    for (name, resolution) in [
        (
            "resolve_cover_recovery_restore_before",
            CoverRecoveryResolution::RestoreBefore,
        ),
        (
            "resolve_cover_recovery_restore_after",
            CoverRecoveryResolution::RestoreAfter,
        ),
        (
            "resolve_cover_recovery_cleanup",
            CoverRecoveryResolution::CleanupCandidates,
        ),
    ] {
        g.request(
            name,
            None,
            Command::ResolveCoverRecovery {
                track: TrackId(42),
                resolution,
                selection: if resolution == CoverRecoveryResolution::RestoreBefore {
                    Some(CoverRecoverySelection {
                        intent: 5161,
                        role: CoverCandidateRole::Backup,
                        sha256: "0123456789abcdef".repeat(4),
                    })
                } else if resolution == CoverRecoveryResolution::RestoreAfter {
                    Some(CoverRecoverySelection {
                        intent: 5161,
                        role: CoverCandidateRole::RollbackAfter,
                        sha256: "abcdef0123456789".repeat(4),
                    })
                } else {
                    None
                },
            },
        );
    }
    g.event(
        "cover_recovery_resolved",
        50,
        Some(Origin::Ui),
        Event::CoverRecoveryResolved {
            track: TrackId(42),
            summary: concat!(
                "Accepted current; backup: /music/.cover-backup-example (SHA-256 ",
                "0123456789ab); staged: missing; retained for separately confirmed cleanup",
            )
            .into(),
        },
    );
    g.request(
        "resolve_cover_recovery",
        None,
        Command::ResolveCoverRecovery {
            track: TrackId(42),
            resolution: CoverRecoveryResolution::AcceptCurrent,
            selection: None,
        },
    );
    g.result(
        "cover_recovery_resolve_action",
        QueryResult::Actions {
            actions: vec![ActionSummary {
                id: ActionId(5162),
                recorded_at_ms: NOW_MS,
                origin: Origin::Ui,
                kind: ActionKind::CoverRecoveryResolve,
                summary: "Accept current file and abandon pending cover recovery".into(),
                target_count: 1,
                undone_by: None,
                undo_of: None,
                undoable: false,
                grant: None,
                details: Some(ActionDetails::CoverRecoveryResolve {
                    track: TrackId(42),
                    resolution: CoverRecoveryResolution::AcceptCurrent,
                    intent_count: 1,
                }),
            }],
            total: 1,
            offset: 0,
            next_offset: None,
        },
    );
    let image=CoverImageInput {
    mime:"image/png".into(),
    data_base64:
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGMQFNf8DwAB3wFRsS+UFAAAAABJRU5ErkJggg=="
    .into()}
    ;

    let set = CoverChange::Set { image };
    g.request(
        "edit_cover_set",
        None,
        Command::EditCover {
            tracks: vec![TrackId(42)],
            change: set.clone(),
        },
    );
    g.request(
        "edit_cover_remove",
        None,
        Command::EditCover {
            tracks: vec![TrackId(42)],
            change: CoverChange::Remove,
        },
    );
    g.query(
        "cover_edit_preview_set",
        Query::CoverEditPreview {
            tracks: vec![TrackId(42)],
            change: set,
        },
    );
    g.query(
        "cover_edit_preview_remove",
        Query::CoverEditPreview {
            tracks: vec![TrackId(42)],
            change: CoverChange::Remove,
        },
    );
    let info = CoverImageInfo {
        mime: "image/png".into(),
        bytes: 68,
        width: 1,
        height: 1,
        sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
    };
    g.result(
        "cover_edit_preview",
        QueryResult::CoverEditPreview {
            files: vec![
                CoverFilePreview {
                    track: TrackId(42),
                    before: Some(info.clone()),
                    after: Some(info.clone()),
                    changed: true,
                    undo_bytes: 160,
                    fallback: true,
                    scope: CoverScope::FrontCover,
                },
                CoverFilePreview {
                    track: TrackId(43),
                    before: None,
                    after: Some(info),
                    changed: true,
                    undo_bytes: 80,
                    fallback: false,
                    scope: CoverScope::AllCovr,
                },
            ],
            skipped: vec![
                CoverFileResult {
                    track: TrackId(44),
                    code: ErrorCode::CueTrack,
                    message: "This track uses part of an audio file".into(),
                },
                CoverFileResult {
                    track: TrackId(45),
                    code: ErrorCode::FileReadOnly,
                    message: "file is read-only".into(),
                },
            ],
        },
    );
    let outcome = JobOutcome::Completed {
        report: JobReport::CoverEdit {
            written: vec![TrackId(42)],
            unchanged: vec![TrackId(43)],
            skipped: vec![CoverFileResult {
                track: TrackId(44),
                code: ErrorCode::CueTrack,
                message: "CUE track".into(),
            }],
            failed: vec![CoverFileResult {
                track: TrackId(45),
                code: ErrorCode::CoverRecoveryRequired,
                message: "Recovery required".into(),
            }],
            action: Some(ActionId(5160)),
        },
    };
    g.result(
        "cover_edit_job",
        QueryResult::Job {
            job: JobInfo {
                id: JobId(51),
                kind: JobKind::CoverEdit,
                state: JobState::Completed,
                progress: Some(JobProgressInfo {
                    done: 4,
                    total: Some(4),
                }),
                outcome: Some(outcome.clone()),
            },
        },
    );
    g.result(
        "cover_edit_jobs",
        QueryResult::Jobs {
            jobs: vec![JobSummary {
                id: JobId(51),
                kind: JobKind::CoverEdit,
                state: JobState::Completed,
                progress: None,
                counts: Some(JobCounts::CoverEdit {
                    written_count: 1,
                    unchanged_count: 1,
                    skipped_count: 1,
                    failed_count: 1,
                }),
                error: None,
            }],
            total: 1,
            offset: 0,
            next_offset: None,
        },
    );
    for (i, operation) in [CoverOperation::Set, CoverOperation::Remove]
        .into_iter()
        .enumerate()
    {
        g.result(
            &format!("cover_edit_actions_{i}"),
            QueryResult::Actions {
                actions: vec![ActionSummary {
                    id: ActionId(5160 + i as i64),
                    recorded_at_ms: NOW_MS,
                    origin: Origin::Ui,
                    kind: ActionKind::CoverEdit,
                    summary: "Set embedded front covers (MP4: all covr items)".into(),
                    target_count: 1,
                    undone_by: None,
                    undo_of: None,
                    undoable: true,
                    grant: None,
                    details: Some(ActionDetails::CoverEdit {
                        operation,
                        scope: CoverScope::FrontCoverOrAllCovr,
                        file_count: 1,
                    }),
                }],
                total: 1,
                offset: 0,
                next_offset: None,
            },
        );
    }
    g.event(
        "covers_changed",
        51,
        Some(Origin::Ui),
        Event::CoversChanged {
            tracks: vec![TrackId(42)],
        },
    );
    g.event(
        "cover_edit_finished",
        52,
        Some(Origin::Ui),
        Event::JobFinished {
            job: JobId(51),
            kind: JobKind::CoverEdit,
            outcome,
        },
    );
    for code in [
        ErrorCode::CoverRecoveryNotFound,
        ErrorCode::CoverRecoveryChanged,
        ErrorCode::InvalidImage,
        ErrorCode::ImageTooLarge,
        ErrorCode::CoverEditUnsupported,
        ErrorCode::CoverRecoveryRequired,
    ] {
        g.err(
            &format!("err_{}", code.as_str()),
            90,
            ContractError::new(code.kind(), code, code.as_str()),
        );
    }
    g.request(
        "resolve_cover_recovery_selected_staged",
        None,
        Command::ResolveCoverRecovery {
            track: TrackId(42),
            resolution: CoverRecoveryResolution::RestoreAfter,
            selection: Some(CoverRecoverySelection {
                intent: 5161,
                role: CoverCandidateRole::Staged,
                sha256: "abcdef0123456789".repeat(4),
            }),
        },
    );
}

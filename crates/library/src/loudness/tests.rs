use super::*;
use crate::db::writer::ScanBatch;
use crate::tests::{fixture, id_of, meta, record, upsert, write};

fn pair() -> (crate::tests::Fixture, TrackId, TrackId) {
    let f = fixture();
    let mut m = meta("One", "Artist", "Album");
    m.replay_gain.track_gain_db = Some(-2.0);
    m.replay_gain.track_peak = Some(0.8);
    m.replay_gain.album_gain_db = Some(-3.0);
    m.replay_gain.album_peak = Some(0.9);
    upsert(
        &f.lib,
        vec![
            record(f.root, "one.flac", m),
            record(f.root, "two.flac", meta("Two", "Artist", "Album")),
        ],
    );
    let one = id_of(&f.lib, "One");
    let two = id_of(&f.lib, "Two");
    (f, one, two)
}
fn update(lib: &Library, track: TrackId, lufs: Option<f64>) -> LoudnessUpdate {
    LoudnessUpdate {
        statistics: None,
        track,
        version: lib.loudness_source(track).unwrap().unwrap().version,
        integrated_lufs: lufs,
        sample_peak: 0.5,
    }
}
fn album(lib: &Library, track: TrackId) -> AlbumLoudnessUpdate {
    let key = lib.track(track).unwrap().unwrap().album_key;
    AlbumLoudnessUpdate {
        members: lib.loudness_members(&key).unwrap(),
        album: key,
        integrated_lufs: -12.0,
        sample_peak: 0.6,
    }
}
#[test]
fn persists_overrides_tags_and_clear_restores_them_without_journal() {
    let (f, one, two) = pair();
    let before = f
        .lib
        .actions(
            None,
            crate::Page {
                offset: 0,
                limit: 100,
            },
        )
        .unwrap();
    let committed = f
        .lib
        .store_loudness(
            vec![update(&f.lib, one, Some(-10.0)), update(&f.lib, two, None)],
            vec![album(&f.lib, one)],
        )
        .unwrap();
    assert_eq!(committed.tracks, vec![one, two]);
    assert_eq!(committed.album_count, 1);
    let rg = f.lib.track(one).unwrap().unwrap().replay_gain;
    assert_eq!(rg.track_gain_db, Some(-8.0));
    assert_eq!(rg.album_gain_db, Some(-6.0));
    assert_eq!(rg.album_peak, Some(0.6));
    assert_eq!(
        f.lib
            .actions(
                None,
                crate::Page {
                    offset: 0,
                    limit: 100
                }
            )
            .unwrap(),
        before
    );
    let db = f._dir.path().join("db/library.sqlite");
    let reopened = Library::open(&db).unwrap();
    assert_eq!(
        reopened.loudness(one).unwrap(),
        f.lib.loudness(one).unwrap()
    );
    let affected = f.lib.clear_loudness(&[one]).unwrap();
    assert_eq!(affected, vec![one, two]);
    let rg = f.lib.track(one).unwrap().unwrap().replay_gain;
    assert_eq!(rg.track_gain_db, Some(-2.0));
    assert_eq!(rg.album_gain_db, Some(-3.0));
    assert!(f.lib.loudness(one).unwrap().is_none());
    assert!(f.lib.loudness(two).unwrap().unwrap().album_lufs.is_none());
}
#[test]
fn stale_versions_partial_albums_and_rescans_cannot_publish_old_values() {
    let (f, one, two) = pair();
    let a = album(&f.lib, one);
    let saved = f
        .lib
        .store_loudness(vec![update(&f.lib, one, Some(-10.0))], vec![a.clone()])
        .unwrap();
    assert_eq!(saved.album_count, 0);
    let old = update(&f.lib, one, Some(-11.0));
    let mut changed = record(f.root, "one.flac", meta("One", "Artist", "Album"));
    changed.mtime_ms = 2;
    upsert(&f.lib, vec![changed]);
    assert!(f.lib.loudness(one).unwrap().is_none());
    let saved = f
        .lib
        .store_loudness(vec![old, update(&f.lib, two, Some(-15.0))], vec![a])
        .unwrap();
    assert_eq!(saved.tracks, vec![two]);
    assert_eq!(saved.album_count, 0);
    let saved = f
        .lib
        .store_loudness(
            vec![
                update(&f.lib, one, Some(-11.0)),
                update(&f.lib, two, Some(-15.0)),
            ],
            vec![album(&f.lib, one)],
        )
        .unwrap();
    assert_eq!(saved.album_count, 1);
    write(
        &f.lib,
        f.root,
        ScanBatch {
            unavailable: vec![two],
            ..ScanBatch::default()
        },
    );
    assert!(f.lib.loudness(one).unwrap().unwrap().album_lufs.is_none());
    // Member changes invalidate albums but retain valid track measurements.
    assert!(f.lib.loudness(one).unwrap().is_some());
    assert!(
        f.lib
            .store_loudness(
                vec![update(&f.lib, one, Some(-11.0))],
                vec![album(&f.lib, one)]
            )
            .unwrap()
            .album_count
            == 1
    );
    upsert(
        &f.lib,
        vec![record(
            f.root,
            "three.flac",
            meta("Three", "Artist", "Album"),
        )],
    );
    assert!(f.lib.loudness(one).unwrap().unwrap().album_lufs.is_none());
}
#[test]
fn finite_data_unknown_albums_and_metadata_only_changes() {
    let (f, one, _) = pair();
    for lufs in [f64::INFINITY, f64::NAN] {
        assert!(
            f.lib
                .store_loudness(vec![update(&f.lib, one, Some(lufs))], vec![])
                .is_err()
        );
    }
    f.lib
        .store_loudness(vec![update(&f.lib, one, Some(-11.0))], vec![])
        .unwrap();
    // Rescanning unchanged file versions preserves its derived measurement.
    upsert(
        &f.lib,
        vec![record(
            f.root,
            "one.flac",
            meta("Renamed", "Artist", "Album"),
        )],
    );
    assert!(f.lib.loudness(one).unwrap().is_some());
    upsert(
        &f.lib,
        vec![record(
            f.root,
            "unknown.flac",
            meta("Unknown", "Artist", ""),
        )],
    );
    let id = id_of(&f.lib, "Unknown");
    assert_eq!(
        f.lib
            .store_loudness(
                vec![update(&f.lib, id, Some(-12.0))],
                vec![album(&f.lib, id)]
            )
            .unwrap()
            .album_count,
        0
    );
}
#[test]
fn rereading_a_root_keeps_unchanged_measurements_and_silence_keeps_its_own_peak() {
    let (f, one, two) = pair();
    let both = |lib: &Library| {
        lib.store_loudness(
            vec![update(lib, one, Some(-10.0)), update(lib, two, Some(-12.0))],
            vec![album(lib, one)],
        )
        .unwrap()
    };
    assert_eq!(both(&f.lib).album_count, 1);
    let rows = |two_mtime: i64| {
        let mut changed = record(f.root, "two.flac", meta("Two", "Artist", "Album"));
        changed.mtime_ms = two_mtime;
        let mut tagged = meta("One", "Artist", "Album");
        tagged.replay_gain.track_gain_db = Some(-2.0);
        tagged.replay_gain.track_peak = Some(0.8);
        vec![record(f.root, "one.flac", tagged), changed]
    };
    // A new forced encoding marks every file to be read again (`mtime_ms = -1`); the
    // scan that finds the stamps measured keeps the measurements and the album's.
    f.lib.set_root_encoding(f.root, Some("gbk")).unwrap();
    assert!(f.lib.loudness(one).unwrap().unwrap().album_lufs.is_some());
    upsert(&f.lib, rows(1));
    assert!(f.lib.loudness(one).unwrap().unwrap().album_lufs.is_some());
    assert!(f.lib.loudness(two).unwrap().is_some());
    // One that changed meanwhile loses its measurement, and the album its aggregate.
    f.lib.set_root_encoding(f.root, None).unwrap();
    upsert(&f.lib, rows(5));
    assert!(f.lib.loudness(two).unwrap().is_none());
    let kept = f.lib.loudness(one).unwrap().unwrap();
    assert_eq!(kept.integrated_lufs, Some(-10.0));
    assert!(kept.album_lufs.is_none());
    // Silence measured: no track gain, and the measured peak with it, not the tags'
    // gain (-2 dB) paired with a measured peak.
    f.lib
        .store_loudness(vec![update(&f.lib, one, None)], vec![])
        .unwrap();
    let rg = f.lib.track(one).unwrap().unwrap().replay_gain;
    assert_eq!((rg.track_gain_db, rg.track_peak), (None, Some(0.5)));
    f.lib.clear_loudness(&[one]).unwrap();
    let rg = f.lib.track(one).unwrap().unwrap().replay_gain;
    assert_eq!((rg.track_gain_db, rg.track_peak), (Some(-2.0), Some(0.8)));
}
#[test]
fn automatic_pages_and_album_publication_are_not_limited_to_1000_tracks() {
    let f = fixture();
    let records = (0..1025)
        .map(|i| {
            record(
                f.root,
                &format!("{i}.flac"),
                meta(&format!("Track {i}"), "Artist", "Large album"),
            )
        })
        .collect();
    upsert(&f.lib, records);
    let mut after = 0;
    let mut count = 0;
    let mut album_key = None;
    loop {
        let page = f.lib.automatic_loudness_tracks(after, 128).unwrap();
        if page.is_empty() {
            break;
        }
        assert!(page.iter().all(|t| !t.measured && t.album_pending));
        let ids: Vec<_> = page.iter().map(|t| t.track).collect();
        assert!(ids.len() <= 128);
        after = ids.last().unwrap().0;
        let updates = ids
            .iter()
            .map(|&id| {
                let mut u = update(&f.lib, id, Some(-9.95));
                let mut stats = b"R128\x01".to_vec();
                stats.resize(8005, 0);
                stats[5 + 600 * 8..5 + 601 * 8].copy_from_slice(&1u64.to_le_bytes());
                u.statistics = Some(stats);
                u
            })
            .collect();
        count += f
            .lib
            .store_automatic_loudness(updates)
            .unwrap()
            .tracks
            .len();
        album_key = Some(f.lib.track(ids[0]).unwrap().unwrap().album_key);
    }
    assert_eq!(count, 1025);
    let key = album_key.unwrap();
    let members = f.lib.pending_album_loudness(&key).unwrap();
    assert_eq!(members.len(), 1025);
    let update = AlbumLoudnessUpdate {
        album: key.clone(),
        members: members.into_iter().map(|(id, v, _)| (id, v)).collect(),
        integrated_lufs: -9.95,
        sample_peak: 0.5,
    };
    assert!(f.lib.store_automatic_album_loudness(update).unwrap());
    assert!(f.lib.pending_album_loudness(&key).unwrap().is_empty());
    let id = f.lib.loudness_members(&key).unwrap()[0].0;
    assert_eq!(f.lib.loudness(id).unwrap().unwrap().album_lufs, Some(-9.95));
}

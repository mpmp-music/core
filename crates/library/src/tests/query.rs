//! Tests of flexible track queries, facets, tags, play statistics and user data on synthetic
//! rows (no audio files).

use std::time::{Duration, Instant};

use crate::db::writer::ScanBatch;
use crate::db::writer::TrackRecord;
use crate::tags::read::TrackMeta;
use crate::tests::{Fixture, LATIN, Lcg, UI, fixture, meta, record, synthetic, upsert, write};
use crate::track_query::{CompareOp, Field, Filter, SortKey, TextOp, TrackQuery, Value};
use crate::{EndedReason, LibraryError, Page, PlayEvent, TrackId, db::schema};

const ALL: Page = Page::new(0, 1000);

struct Spec {
    path: &'static str,
    title: &'static str,
    artist: Option<&'static str>,
    album: Option<&'static str>,
    genre: Option<&'static str>,
    year: Option<i32>,
    lossless: Option<bool>,
    track_no: u32,
    tags: &'static [(&'static str, &'static str)],
}

const LIBRARY: &[Spec] = &[
    Spec {
        path: "chalven/03.flac",
        title: "Étude for Paper Moons",
        artist: Some("Chalven"),
        album: Some("Études"),
        genre: Some("Classical"),
        year: Some(1990),
        lossless: Some(true),
        track_no: 3,
        tags: &[("COMPOSER", "Frédéric Chalven"), ("ARTIST", "Chalven")],
    },
    Spec {
        path: "dvoral/09.flac",
        title: "Symphony of Tin Moons",
        artist: Some("Dvořál"),
        album: Some("Nacre Windows"),
        genre: Some("Classical"),
        year: Some(1985),
        lossless: Some(true),
        track_no: 9,
        tags: &[("COMPOSER", "Antonín Dvořál"), ("LABEL", "DG")],
    },
    Spec {
        path: "miren/01.mp3",
        title: "Soft Wheels",
        artist: Some("Miren Dalen"),
        album: Some("Kites of Brass"),
        genre: Some("Jazz"),
        year: Some(1959),
        lossless: Some(false),
        track_no: 1,
        tags: &[
            ("ARTIST", "Miren Dalen"),
            ("ARTIST", "Jorin Calven"),
            ("LABEL", "Cloudbox"),
        ],
    },
    Spec {
        path: "miren/03.mp3",
        title: "Brass in Glass",
        artist: Some("Miren Dalen"),
        album: Some("Kites of Brass"),
        genre: Some("Jazz"),
        year: Some(1959),
        lossless: Some(false),
        track_no: 3,
        tags: &[("LABEL", "Cloudbox")],
    },
    Spec {
        path: "misc/untitled.wav",
        title: "Untitled",
        artist: None,
        album: None,
        genre: None,
        year: None,
        lossless: None,
        track_no: 1,
        tags: &[],
    },
    Spec {
        path: "briven/03.flac",
        title: "Tin Ferns",
        artist: Some("Daren Briven"),
        album: Some("Tidal Oars"),
        genre: Some("jazz"),
        year: Some(1959),
        lossless: Some(true),
        track_no: 3,
        tags: &[],
    },
];

fn build() -> Fixture {
    let f = fixture();
    let records = LIBRARY
        .iter()
        .map(|spec| {
            let mut m: TrackMeta = meta(spec.title, "", "");
            m.artist = spec.artist.map(str::to_owned);
            m.album = spec.album.map(str::to_owned);
            m.genre = spec.genre.map(str::to_owned);
            m.year = spec.year;
            m.lossless = spec.lossless;
            m.track_no = Some(spec.track_no);
            m.duration_ms = Some(200_000);
            m.tags = spec
                .tags
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            record(f.root, spec.path, m)
        })
        .collect();
    upsert(&f.lib, records);
    f
}

fn query(filter: Filter) -> TrackQuery {
    TrackQuery {
        filter,
        sort: vec![SortKey {
            field: Field::Title,
            descending: false,
        }],
    }
}

/// Titles matching `filter`, sorted by title.
fn titles(f: &Fixture, filter: Filter) -> Vec<String> {
    let (rows, total) = f.lib.tracks_query(&query(filter), ALL, &[]).unwrap();
    assert_eq!(rows.len() as u64, total);
    rows.into_iter()
        .map(|r| r.track.title.unwrap_or_default())
        .collect()
}

fn id(f: &Fixture, title: &str) -> TrackId {
    let (rows, _) = f
        .lib
        .tracks_query(
            &query(Filter::Text {
                field: Field::Title,
                op: TextOp::Equals,
                text: title.into(),
            }),
            ALL,
            &[],
        )
        .unwrap();
    rows[0].track.id
}

fn cmp(field: Field, op: CompareOp, value: Value) -> Filter {
    Filter::Compare { field, op, value }
}

fn txt(field: Field, op: TextOp, text: &str) -> Filter {
    Filter::Text {
        field,
        op,
        text: text.into(),
    }
}

fn tag(key: &str) -> Field {
    Field::Tag { key: key.into() }
}

#[test]
fn compare_numbers_booleans_and_times() {
    let f = build();
    assert_eq!(
        titles(&f, cmp(Field::Year, CompareOp::Lt, Value::Int(1960))),
        ["Brass in Glass", "Soft Wheels", "Tin Ferns"]
    );
    assert_eq!(
        titles(&f, cmp(Field::Year, CompareOp::Ge, Value::Int(1985))),
        ["Étude for Paper Moons", "Symphony of Tin Moons"]
    );
    // Ne keeps tracks without a value.
    assert_eq!(
        titles(&f, cmp(Field::Year, CompareOp::Ne, Value::Int(1959))),
        ["Étude for Paper Moons", "Symphony of Tin Moons", "Untitled"]
    );
    assert_eq!(
        titles(&f, cmp(Field::Lossless, CompareOp::Eq, Value::Bool(false))),
        ["Brass in Glass", "Soft Wheels"]
    );
    assert_eq!(
        titles(&f, cmp(Field::TrackNo, CompareOp::Le, Value::Int(1))).len(),
        2
    );
    assert_eq!(
        titles(&f, cmp(Field::AddedAt, CompareOp::Gt, Value::DaysAgo(1))).len(),
        6
    );
    assert!(titles(&f, cmp(Field::AddedAt, CompareOp::Lt, Value::DaysAgo(1))).is_empty());
    assert_eq!(
        titles(&f, cmp(Field::AddedAt, CompareOp::Gt, Value::Timestamp(0))).len(),
        6
    );
    // A text field compares search keys.
    assert_eq!(
        titles(
            &f,
            cmp(
                Field::Title,
                CompareOp::Eq,
                Value::Text("SOFT WHEELS".into())
            )
        ),
        ["Soft Wheels"]
    );
    assert_eq!(
        titles(
            &f,
            cmp(Field::Codec, CompareOp::Eq, Value::Text("x".into()))
        )
        .len(),
        0
    );
}

#[test]
fn text_ops_fold_accents_and_case() {
    let f = build();
    assert_eq!(
        titles(&f, txt(Field::Title, TextOp::Contains, "etude")),
        ["Étude for Paper Moons"]
    );
    assert_eq!(
        titles(&f, txt(Field::Artist, TextOp::Equals, "dvoral")),
        ["Symphony of Tin Moons"]
    );
    assert_eq!(
        titles(&f, txt(Field::Title, TextOp::StartsWith, "BRASS")),
        ["Brass in Glass"]
    );
    assert!(titles(&f, txt(Field::Title, TextOp::StartsWith, "glass")).is_empty());
    assert_eq!(
        titles(&f, txt(Field::Album, TextOp::Equals, "etudes")),
        ["Étude for Paper Moons"]
    );
    assert_eq!(
        titles(&f, txt(Field::Path, TextOp::StartsWith, "MIREN/")).len(),
        2
    );
    // Full-text search folds accents too, and combines with other conditions.
    let results = f.lib.search("dvoral", 10).unwrap();
    assert_eq!(
        results.tracks[0].title.as_deref(),
        Some("Symphony of Tin Moons")
    );
    assert_eq!(f.lib.search("etude", 10).unwrap().tracks.len(), 1);
    assert_eq!(
        titles(
            &f,
            Filter::And {
                all: vec![
                    Filter::Search {
                        text: "kites of brass".into()
                    },
                    txt(Field::Title, TextOp::Contains, "so")
                ]
            }
        ),
        ["Soft Wheels"]
    );
    assert_eq!(titles(&f, Filter::Search { text: "  ".into() }).len(), 6);
}

#[test]
fn tags_multi_valued_and_case_insensitive() {
    let f = build();
    assert_eq!(
        titles(&f, txt(tag("artist"), TextOp::Equals, "jorin calven")),
        ["Soft Wheels"]
    );
    assert_eq!(
        titles(&f, txt(tag("Composer"), TextOp::Contains, "dvoral")),
        ["Symphony of Tin Moons"]
    );
    assert_eq!(
        titles(
            &f,
            cmp(tag("LABEL"), CompareOp::Eq, Value::Text("cloudbox".into()))
        ),
        ["Brass in Glass", "Soft Wheels"]
    );
    // Ne: no value equals.
    assert_eq!(
        titles(
            &f,
            cmp(
                tag("ARTIST"),
                CompareOp::Ne,
                Value::Text("Miren Dalen".into())
            )
        )
        .len(),
        5
    );
    assert_eq!(
        titles(
            &f,
            Filter::Missing {
                field: tag("label")
            }
        ),
        ["Étude for Paper Moons", "Tin Ferns", "Untitled"]
    );

    let (rows, _) = f
        .lib
        .tracks_query(
            &query(txt(Field::Title, TextOp::Equals, "soft wheels")),
            ALL,
            &["artist".into(), "LABEL".into(), "MISSING".into()],
        )
        .unwrap();
    let tags = &rows[0].tags;
    assert_eq!(tags["ARTIST"], ["Miren Dalen", "Jorin Calven"]);
    assert_eq!(tags["LABEL"], ["Cloudbox"]);
    assert!(!tags.contains_key("MISSING"));

    // Sorting by a tag: missing values last.
    let (rows, _) = f
        .lib
        .tracks_query(
            &TrackQuery {
                filter: Filter::All,
                sort: vec![SortKey {
                    field: tag("label"),
                    descending: false,
                }],
            },
            ALL,
            &[],
        )
        .unwrap();
    let label_order: Vec<&str> = rows
        .iter()
        .map(|r| r.track.title.as_deref().unwrap())
        .collect();
    // Cloudbox (two tracks, stable by album artist/album/track) before DG, then the rest.
    assert_eq!(
        &label_order[..3],
        ["Soft Wheels", "Brass in Glass", "Symphony of Tin Moons"]
    );
}

#[test]
fn missing_not_or_nesting() {
    let f = build();
    assert_eq!(
        titles(
            &f,
            Filter::Missing {
                field: Field::Genre
            }
        ),
        ["Untitled"]
    );
    assert_eq!(
        titles(
            &f,
            Filter::Missing {
                field: Field::Lossless
            }
        ),
        ["Untitled"]
    );
    // NOT of a comparison with a missing value includes the track (no SQL NULL logic).
    assert_eq!(
        titles(
            &f,
            Filter::Not {
                filter: Box::new(cmp(Field::Year, CompareOp::Lt, Value::Int(1960)))
            }
        ),
        ["Étude for Paper Moons", "Symphony of Tin Moons", "Untitled"]
    );
    let jazz_or_unknown = Filter::Or {
        any: vec![
            txt(Field::Genre, TextOp::Equals, "jazz"),
            Filter::Missing {
                field: Field::Genre,
            },
        ],
    };
    assert_eq!(titles(&f, jazz_or_unknown.clone()).len(), 4);
    assert_eq!(
        titles(
            &f,
            Filter::And {
                all: vec![
                    jazz_or_unknown,
                    Filter::Not {
                        filter: Box::new(cmp(Field::Lossless, CompareOp::Eq, Value::Bool(true)))
                    }
                ]
            }
        ),
        ["Brass in Glass", "Soft Wheels", "Untitled"]
    );
    assert!(titles(&f, Filter::Or { any: vec![] }).is_empty());
    assert_eq!(titles(&f, Filter::And { all: vec![] }).len(), 6);
}

#[test]
fn sorting_is_stable_across_pages() {
    let f = build();
    let q = TrackQuery {
        filter: Filter::All,
        sort: vec![SortKey {
            field: Field::Genre,
            descending: true,
        }],
    };
    let (all, total) = f.lib.tracks_query(&q, ALL, &[]).unwrap();
    assert_eq!(total, 6);
    let mut paged = Vec::new();
    for offset in (0..6).step_by(2) {
        let (rows, total) = f.lib.tracks_query(&q, Page::new(offset, 2), &[]).unwrap();
        assert_eq!(total, 6);
        paged.extend(rows);
    }
    assert_eq!(
        paged.iter().map(|r| r.track.id).collect::<Vec<_>>(),
        all.iter().map(|r| r.track.id).collect::<Vec<_>>()
    );
    // Descending genre, missing last; ties by album artist, album, disc, track (search
    // keys, so `Étude` sorts as `etude`).
    let order: Vec<&str> = all
        .iter()
        .map(|r| r.track.title.as_deref().unwrap())
        .collect();
    assert_eq!(
        order,
        [
            "Tin Ferns",
            "Soft Wheels",
            "Brass in Glass",
            "Étude for Paper Moons",
            "Symphony of Tin Moons",
            "Untitled"
        ]
    );
    let (rows, total) = f.lib.tracks_query(&q, Page::new(10, 5), &[]).unwrap();
    assert!(rows.is_empty());
    assert_eq!(total, 6);
    assert_eq!(
        f.lib.track_ids(&q, 2).unwrap(),
        [all[0].track.id, all[1].track.id]
    );
}

#[test]
fn facets_with_unknown_group() {
    let f = build();
    let (values, total) = f.lib.facet(&Field::Genre, &Filter::All, ALL).unwrap();
    assert_eq!(total, 3);
    let shown: Vec<(Option<Value>, u64)> =
        values.into_iter().map(|v| (v.value, v.tracks)).collect();
    assert_eq!(
        shown,
        [
            (Some(Value::Text("Classical".into())), 2),
            (Some(Value::Text("Jazz".into())), 3),
            (None, 1)
        ]
    );
    let (values, _) = f.lib.facet(&Field::Year, &Filter::All, ALL).unwrap();
    assert_eq!(values[0].value, Some(Value::Int(1959)));
    assert_eq!(values[0].tracks, 3);
    assert_eq!(values.last().unwrap().value, None);

    // Tree browsing: album artists within one genre.
    let jazz = txt(Field::Genre, TextOp::Equals, "jazz");
    let (values, total) = f.lib.facet(&Field::AlbumArtist, &jazz, ALL).unwrap();
    assert_eq!(total, 1);
    assert_eq!(values[0].value, None);
    let (values, total) = f.lib.facet(&Field::Artist, &jazz, ALL).unwrap();
    assert_eq!(total, 2);
    assert_eq!(values[1].value, Some(Value::Text("Miren Dalen".into())));

    // Tags: a track counts under each of its values; no-tag tracks are the None group.
    let (values, total) = f.lib.facet(&tag("artist"), &Filter::All, ALL).unwrap();
    assert_eq!(total, 4);
    let names: Vec<_> = values.iter().map(|v| (v.value.clone(), v.tracks)).collect();
    assert_eq!(
        names,
        [
            (Some(Value::Text("Chalven".into())), 1),
            (Some(Value::Text("Jorin Calven".into())), 1),
            (Some(Value::Text("Miren Dalen".into())), 1),
            (None, 4)
        ]
    );
    // Paging, and a page past the end still reports the total.
    let (values, total) = f
        .lib
        .facet(&tag("artist"), &Filter::All, Page::new(3, 2))
        .unwrap();
    assert_eq!((values.len(), total), (1, 4));
    let (values, total) = f
        .lib
        .facet(&Field::Genre, &Filter::All, Page::new(9, 2))
        .unwrap();
    assert_eq!((values.len(), total), (0, 3));
    let (values, _) = f.lib.facet(&Field::Loved, &Filter::All, ALL).unwrap();
    assert_eq!(values[0].value, Some(Value::Bool(false)));
}

#[test]
fn unavailable_tracks_only_on_request() {
    let f = build();
    let take_five = id(&f, "Tin Ferns");
    write(
        &f.lib,
        f.root,
        ScanBatch {
            unavailable: vec![take_five],
            ..ScanBatch::default()
        },
    );
    assert_eq!(titles(&f, Filter::All).len(), 5);
    let gone = cmp(Field::Available, CompareOp::Eq, Value::Bool(false));
    assert_eq!(titles(&f, gone.clone()), ["Tin Ferns"]);
    let (values, _) = f.lib.facet(&Field::Genre, &Filter::All, ALL).unwrap();
    assert_eq!(values[1].tracks, 2);
    let (values, _) = f.lib.facet(&Field::Genre, &gone, ALL).unwrap();
    assert_eq!(values.len(), 1);
}

#[test]
fn invalid_queries_are_reported() {
    let f = build();
    let bad = [
        cmp(Field::Title, CompareOp::Lt, Value::Int(3)),
        cmp(Field::Year, CompareOp::Eq, Value::Text("1959".into())),
        cmp(Field::Loved, CompareOp::Gt, Value::Bool(false)),
        txt(Field::PlayCount, TextOp::Contains, "1"),
    ];
    for filter in bad {
        let err = f
            .lib
            .tracks_query(&query(filter.clone()), ALL, &[])
            .unwrap_err();
        assert!(matches!(err, LibraryError::InvalidQuery(_)), "{filter:?}");
        assert!(matches!(
            f.lib.facet(&Field::Genre, &filter, ALL),
            Err(LibraryError::InvalidQuery(_))
        ));
    }
    let empty_tag_sort = TrackQuery {
        filter: Filter::All,
        sort: vec![SortKey {
            field: tag(""),
            descending: false,
        }],
    };
    assert!(matches!(
        f.lib.tracks_query(&empty_tag_sort, ALL, &[]),
        Err(LibraryError::InvalidQuery(_))
    ));
    assert!(matches!(
        f.lib.tracks_query(&query(Filter::All), ALL, &[" ".into()]),
        Err(LibraryError::InvalidQuery(_))
    ));
    // Ratings are 1-5 stars.
    for bad in [0, 6] {
        let err = f.lib.set_rating(&[TrackId(1)], Some(bad), UI).unwrap_err();
        assert_eq!(err.code(), "out_of_range", "{err}");
    }
}

#[test]
fn ratings_and_loved_survive_rescans_and_root_removal() {
    let f = build();
    let etude = id(&f, "Étude for Paper Moons");
    let so_what = id(&f, "Soft Wheels");
    f.lib.set_rating(&[etude, so_what], Some(5), UI).unwrap();
    f.lib.set_rating(&[so_what], Some(3), UI).unwrap();
    f.lib
        .set_loved(&[so_what, TrackId(999_999)], true, UI)
        .unwrap();
    assert_eq!(
        titles(&f, cmp(Field::Rating, CompareOp::Ge, Value::Int(4))),
        ["Étude for Paper Moons"]
    );
    assert_eq!(
        titles(&f, cmp(Field::Loved, CompareOp::Eq, Value::Bool(true))),
        ["Soft Wheels"]
    );
    assert_eq!(
        titles(
            &f,
            Filter::Missing {
                field: Field::Rating
            }
        )
        .len(),
        4
    );
    // Tracks without user data: unrated and not loved.
    assert_eq!(
        titles(&f, cmp(Field::Loved, CompareOp::Eq, Value::Bool(false))).len(),
        5
    );
    assert_eq!(
        titles(&f, cmp(Field::Rating, CompareOp::Ne, Value::Int(5))).len(),
        5
    );
    assert_eq!(
        titles(&f, cmp(Field::Rating, CompareOp::Lt, Value::Int(4))),
        ["Soft Wheels"]
    );

    let check = |f: &Fixture| {
        let (rows, _) = f
            .lib
            .tracks_query(
                &query(txt(Field::Title, TextOp::Equals, "soft wheels")),
                ALL,
                &[],
            )
            .unwrap();
        assert_eq!((rows[0].rating, rows[0].loved), (Some(3), true));
    };
    check(&f);

    // A rescan re-reads the file under the same id.
    upsert(
        &f.lib,
        vec![record(
            f.root,
            "miren/01.mp3",
            meta("Soft Wheels", "Miren Dalen", "Kites of Brass"),
        )],
    );
    check(&f);

    // Soft removal and re-adding keep the rows.
    let music = f.lib.roots().unwrap()[0].path.clone();
    f.lib.remove_root(f.root, UI).unwrap();
    assert_eq!(f.lib.add_root(&music, UI).unwrap().value, f.root);
    write(
        &f.lib,
        f.root,
        ScanBatch {
            available: vec![so_what],
            ..ScanBatch::default()
        },
    );
    check(&f);

    f.lib.set_rating(&[so_what], None, UI).unwrap();
    f.lib.set_loved(&[so_what], false, UI).unwrap();
    let (rows, _) = f
        .lib
        .tracks_query(
            &query(txt(Field::Title, TextOp::Equals, "soft wheels")),
            ALL,
            &[],
        )
        .unwrap();
    assert_eq!((rows[0].rating, rows[0].loved), (None, false));
}

fn play(id: TrackId, at: i64, played_s: u64, ended: EndedReason) -> PlayEvent {
    PlayEvent {
        track_id: id,
        context: None,
        started_at: at,
        played_ms: played_s * 1000,
        ended_reason: ended,
        origin: "test".into(),
    }
}

#[test]
fn play_statistics_follow_the_last_fm_rule() {
    let f = build();
    let so_what = id(&f, "Soft Wheels");
    let blue = id(&f, "Brass in Glass");
    // 200 s tracks: a play needs 100 s heard, however it ended; a skip is under 100 s.
    for event in [
        play(so_what, 1_000, 100, EndedReason::Completed),
        // Seeked near the end and let finish: not a play.
        play(so_what, 1_500, 30, EndedReason::Completed),
        play(so_what, 2_000, 10, EndedReason::Skipped),
        play(so_what, 3_000, 120, EndedReason::Stopped),
        play(so_what, 4_000, 50, EndedReason::Stopped),
        play(blue, 5_000, 99, EndedReason::Skipped),
        play(blue, 6_000, 100, EndedReason::Skipped),
    ] {
        f.lib.record_play(event).unwrap();
    }
    f.lib.flush().unwrap();

    let stats = |f: &Fixture| {
        let (rows, _) = f
            .lib
            .tracks_query(
                &TrackQuery {
                    filter: cmp(Field::PlayCount, CompareOp::Ge, Value::Int(1)),
                    sort: vec![SortKey {
                        field: Field::PlayCount,
                        descending: true,
                    }],
                },
                ALL,
                &[],
            )
            .unwrap();
        rows.into_iter()
            .map(|r| {
                (
                    r.track.title.unwrap(),
                    r.play_count,
                    r.skip_count,
                    r.first_played_at,
                    r.last_played_at,
                )
            })
            .collect::<Vec<_>>()
    };
    let expected = vec![
        ("Soft Wheels".to_owned(), 2, 1, Some(1_000), Some(3_000)),
        ("Brass in Glass".to_owned(), 1, 1, Some(6_000), Some(6_000)),
    ];
    assert_eq!(stats(&f), expected);
    assert_eq!(
        titles(
            &f,
            Filter::Missing {
                field: Field::LastPlayedAt
            }
        )
        .len(),
        4
    );
    assert_eq!(
        titles(&f, cmp(Field::SkipCount, CompareOp::Eq, Value::Int(1))).len(),
        2
    );
    // Tracks without statistics count as 0 plays and never played.
    let count = |filter| titles(&f, filter).len();
    assert_eq!(
        count(cmp(Field::PlayCount, CompareOp::Lt, Value::Int(1))),
        4
    );
    assert_eq!(
        count(cmp(Field::PlayCount, CompareOp::Eq, Value::Int(0))),
        4
    );
    assert_eq!(
        count(cmp(Field::PlayCount, CompareOp::Ne, Value::Int(2))),
        5
    );
    assert_eq!(
        count(cmp(Field::SkipCount, CompareOp::Ge, Value::Int(0))),
        6
    );
    assert_eq!(
        count(cmp(Field::PlayCount, CompareOp::Gt, Value::Int(1))),
        1
    );
    assert_eq!(
        count(cmp(
            Field::LastPlayedAt,
            CompareOp::Ne,
            Value::Timestamp(3_000)
        )),
        5
    );
    assert_eq!(
        count(cmp(
            Field::LastPlayedAt,
            CompareOp::Le,
            Value::Timestamp(3_000)
        )),
        1
    );
    // The same numbers from the raw history.
    f.lib.rebuild_play_stats().unwrap();
    assert_eq!(stats(&f), expected);
}

// ---- scale ---

/// A labelled query run returning its total.
type Case<'a> = (&'a str, Box<dyn Fn() -> u64 + 'a>);

/// A synthetic track with the tags, numbers and flags real libraries have (~11 tag rows).
fn tagged(rng: &mut Lcg, root: crate::RootId, i: usize, artists: &[String]) -> TrackRecord {
    let mut r = synthetic(rng, root, i, artists);
    let m = &mut r.meta;
    m.year = Some(1950 + ((i * 7) % 75) as i32);
    m.lossless = Some(!i.is_multiple_of(3));
    m.duration_ms = Some(120_000 + (i % 300) as u64 * 1000);
    let mut tags = vec![
        ("TITLE".to_owned(), m.title.clone().unwrap()),
        ("ARTIST".to_owned(), m.artist.clone().unwrap()),
        ("ALBUM".to_owned(), m.album.clone().unwrap()),
        ("GENRE".to_owned(), m.genre.clone().unwrap()),
        ("DATE".to_owned(), m.year.unwrap().to_string()),
        ("TRACKNUMBER".to_owned(), m.track_no.unwrap().to_string()),
        (
            "COMPOSER".to_owned(),
            format!("Composer {}", (i * 13) % 500),
        ),
        ("LABEL".to_owned(), format!("Label {}", i % 50)),
        ("ISRC".to_owned(), format!("XX{i:010}")),
        ("COMMENT".to_owned(), rng.words(LATIN, 4, " ")),
    ];
    if i.is_multiple_of(10) {
        tags.push((
            "COMPOSER".to_owned(),
            format!("Composer {}", (i * 17) % 500),
        ));
    }
    m.tags = tags;
    r
}

/// Builds `rows` tagged tracks with statistics and ratings; returns the worst best-of-three
/// latency of the typical (non-tag, non-search) queries and of the facets.
fn query_latency(rows: usize) -> (Duration, Duration) {
    let f = fixture();
    let lib = &f.lib;
    let mut rng = Lcg(7);
    let artists: Vec<String> = (0..rows.div_ceil(50))
        .map(|i| format!("{} {i}", rng.words(LATIN, 2, " ")))
        .collect();
    let start = Instant::now();
    let mut batch = Vec::with_capacity(500);
    for i in 0..rows {
        batch.push(tagged(&mut rng, f.root, i, &artists));
        if batch.len() == 500 {
            upsert(lib, std::mem::take(&mut batch));
        }
    }
    upsert(lib, batch);
    {
        // Statistics and ratings for a share of the tracks, straight into the tables.
        let conn = rusqlite::Connection::open(lib.inner().pool.path()).unwrap();
        conn.execute_batch(
            "INSERT INTO track_stats SELECT id, id % 40, id % 7, 1600000000000 + id * 1000,
                 1700000000000 + (id % 1000) * 86400000 FROM tracks WHERE id % 3 <> 0;
             INSERT INTO track_user (track_id, rating, loved)
                 SELECT id, NULLIF(id % 6, 0), id % 11 = 0 FROM tracks WHERE id % 5 = 0;",
        )
        .unwrap();
    }
    // What the library itself runs after a scan: measure the plans users get.
    lib.inner()
        .writer
        .run(|conn| Ok(schema::optimize(conn)?))
        .unwrap();
    eprintln!("built {rows} tagged rows in {:.2?}", start.elapsed());
    if let Some(out) = std::env::var_os("LIBRARY_SCALE_DB") {
        // For experimenting with query plans outside the test.
        let conn = rusqlite::Connection::open(lib.inner().pool.path()).unwrap();
        conn.execute("VACUUM INTO ?1", [out.to_str().unwrap()])
            .unwrap();
    }

    let time = |label: &str, run: &dyn Fn() -> u64| {
        run();
        let mut best = Duration::MAX;
        let mut n = 0;
        for _ in 0..3 {
            let t = Instant::now();
            n = run();
            best = best.min(t.elapsed());
        }
        eprintln!("{label}: {best:.2?} ({n})");
        best
    };
    let page = Page::new(0, 50);
    let tracks = |q: TrackQuery, page: Page| move || lib.tracks_query(&q, page, &[]).unwrap().1;
    let by = |field: Field, descending: bool| vec![SortKey { field, descending }];
    let jazz = txt(Field::Genre, TextOp::Contains, "jazz");
    let not_recent = Filter::Or {
        any: vec![
            Filter::Missing {
                field: Field::LastPlayedAt,
            },
            cmp(Field::LastPlayedAt, CompareOp::Lt, Value::DaysAgo(30)),
        ],
    };
    let typical: Vec<Case> = vec![
        (
            "all, by title",
            Box::new(tracks(
                TrackQuery {
                    filter: Filter::All,
                    sort: by(Field::Title, false),
                },
                page,
            )),
        ),
        (
            "all, by artist, first page",
            Box::new(tracks(
                TrackQuery {
                    filter: Filter::All,
                    sort: by(Field::Artist, false),
                },
                page,
            )),
        ),
        (
            "genre contains jazz, by play count",
            Box::new(tracks(
                TrackQuery {
                    filter: jazz.clone(),
                    sort: by(Field::PlayCount, true),
                },
                page,
            )),
        ),
        (
            "design example (jazz, lossless, not played 30 days)",
            Box::new(tracks(
                TrackQuery {
                    filter: Filter::And {
                        all: vec![
                            jazz.clone(),
                            cmp(Field::Lossless, CompareOp::Eq, Value::Bool(true)),
                            not_recent.clone(),
                        ],
                    },
                    sort: by(Field::PlayCount, true),
                },
                page,
            )),
        ),
        (
            "year 1990-1999, rating >= 4, by year",
            Box::new(tracks(
                TrackQuery {
                    filter: Filter::And {
                        all: vec![
                            cmp(Field::Year, CompareOp::Ge, Value::Int(1990)),
                            cmp(Field::Year, CompareOp::Le, Value::Int(1999)),
                            cmp(Field::Rating, CompareOp::Ge, Value::Int(4)),
                        ],
                    },
                    sort: by(Field::Year, false),
                },
                page,
            )),
        ),
        (
            "loved, by last played",
            Box::new(tracks(
                TrackQuery {
                    filter: cmp(Field::Loved, CompareOp::Eq, Value::Bool(true)),
                    sort: by(Field::LastPlayedAt, true),
                },
                page,
            )),
        ),
    ];
    let mut worst_typical = Duration::ZERO;
    for (label, run) in &typical {
        worst_typical = worst_typical.max(time(label, run.as_ref()));
    }
    let slow: Vec<Case> = vec![
        (
            "all, by artist, deep page (offset rows/2)",
            Box::new(tracks(
                TrackQuery {
                    filter: Filter::All,
                    sort: by(Field::Artist, false),
                },
                Page::new(rows as u32 / 2, 50),
            )),
        ),
        (
            "track_ids of the whole view, by artist",
            Box::new(|| {
                let q = TrackQuery {
                    filter: Filter::All,
                    sort: by(Field::Artist, false),
                };
                lib.track_ids(&q, usize::MAX).unwrap().len() as u64
            }),
        ),
        (
            "track_rows for a window of 50 ids, 2 tags",
            Box::new(|| {
                let ids: Vec<TrackId> = (rows as i64 / 2..rows as i64 / 2 + 50)
                    .map(TrackId)
                    .collect();
                lib.track_rows(&ids, &["COMPOSER".into(), "LABEL".into()])
                    .unwrap()
                    .len() as u64
            }),
        ),
        (
            "tag COMPOSER contains '17', by title",
            Box::new(tracks(
                TrackQuery {
                    filter: txt(tag("composer"), TextOp::Contains, "17"),
                    sort: by(Field::Title, false),
                },
                page,
            )),
        ),
        (
            "tag LABEL equals, by play count",
            Box::new(tracks(
                TrackQuery {
                    filter: txt(tag("label"), TextOp::Equals, "label 7"),
                    sort: by(Field::PlayCount, true),
                },
                page,
            )),
        ),
        (
            "all, sorted by tag LABEL",
            Box::new(tracks(
                TrackQuery {
                    filter: Filter::All,
                    sort: by(tag("label"), false),
                },
                page,
            )),
        ),
        (
            "search 'love' + year > 2000",
            Box::new(tracks(
                TrackQuery {
                    filter: Filter::And {
                        all: vec![
                            Filter::Search {
                                text: "love".into(),
                            },
                            cmp(Field::Year, CompareOp::Gt, Value::Int(2000)),
                        ],
                    },
                    sort: by(Field::Title, false),
                },
                page,
            )),
        ),
        (
            "search '梦' + jazz",
            Box::new(tracks(
                TrackQuery {
                    filter: Filter::And {
                        all: vec![Filter::Search { text: "梦".into() }, jazz.clone()],
                    },
                    sort: vec![],
                },
                page,
            )),
        ),
    ];
    for (label, run) in &slow {
        time(label, run.as_ref());
    }
    let facet = |field: Field, filter: Filter| move || lib.facet(&field, &filter, page).unwrap().1;
    let facets: Vec<Case> = vec![
        ("facet genre", Box::new(facet(Field::Genre, Filter::All))),
        ("facet year", Box::new(facet(Field::Year, Filter::All))),
        (
            "facet album artist",
            Box::new(facet(Field::AlbumArtist, Filter::All)),
        ),
        (
            "facet artist within jazz",
            Box::new(facet(Field::Artist, jazz.clone())),
        ),
        ("facet loved", Box::new(facet(Field::Loved, Filter::All))),
    ];
    let mut worst_facet = Duration::ZERO;
    for (label, run) in &facets {
        worst_facet = worst_facet.max(time(label, run.as_ref()));
    }
    let tag_facets: Vec<Case> = vec![
        (
            "facet tag LABEL",
            Box::new(facet(tag("label"), Filter::All)),
        ),
        (
            "facet tag COMPOSER within jazz",
            Box::new(facet(tag("composer"), jazz)),
        ),
    ];
    for (label, run) in &tag_facets {
        time(label, run.as_ref());
    }
    eprintln!("worst typical query {worst_typical:.2?}, worst facet {worst_facet:.2?}");
    (worst_typical, worst_facet)
}

/// Runs every query shape of the scale test on a small library (fast; checks they work).
#[test]
fn query_shapes_on_2k_rows() {
    query_latency(2_000);
}

/// Design target (release, 100k tracks): typical filter + sort + page < 50 ms, facets
/// < 50 ms; tag and search queries are reported. Run with
/// `cargo test -p mpmp-library --release -- --ignored scale_100k_query`.
#[test]
#[ignore = "builds 100k tagged rows; run with --ignored, preferably --release"]
fn scale_100k_query_latency() {
    let (typical, facet) = query_latency(100_000);
    if !cfg!(debug_assertions) {
        assert!(
            typical < Duration::from_millis(50),
            "typical query {typical:?}"
        );
        assert!(facet < Duration::from_millis(50), "facet {facet:?}");
    }
}

// Missing-value filters, pagination, and query validation.

/// Text fields store '' for a missing value: it must still sort last ascending (it did
/// only descending).
#[test]
fn review_missing_text_values_sort_last_ascending() {
    let f = build();
    for field in [Field::Artist, Field::Album, Field::Genre] {
        for descending in [false, true] {
            let q = TrackQuery {
                filter: Filter::All,
                sort: vec![SortKey {
                    field: field.clone(),
                    descending,
                }],
            };
            let (rows, _) = f.lib.tracks_query(&q, ALL, &[]).unwrap();
            let order: Vec<&str> = rows
                .iter()
                .map(|r| r.track.title.as_deref().unwrap())
                .collect();
            assert_eq!(
                order.last().copied(),
                Some("Untitled"),
                "{field:?} {order:?}"
            );
        }
    }
}

/// Missing text values satisfy no comparison or text match except `ne`, consistently with
/// `Missing` and the facets' unknown group.
#[test]
fn missing_text_values_match_nothing_but_ne() {
    let f = build();
    for field in [Field::Artist, Field::Album, Field::Genre] {
        for op in [CompareOp::Lt, CompareOp::Le, CompareOp::Eq] {
            let found = titles(&f, cmp(field.clone(), op, Value::Text("zzz".into())));
            assert!(!found.contains(&"Untitled".to_owned()), "{field:?} {op:?}");
        }
        let found = titles(
            &f,
            cmp(field.clone(), CompareOp::Eq, Value::Text(String::new())),
        );
        assert!(found.is_empty(), "{field:?} eq ''");
        for op in [TextOp::StartsWith, TextOp::Contains, TextOp::Equals] {
            assert!(
                titles(&f, txt(field.clone(), op, ""))
                    .iter()
                    .all(|t| t != "Untitled")
            );
        }
        let ne = titles(
            &f,
            cmp(field.clone(), CompareOp::Ne, Value::Text("zzz".into())),
        );
        assert!(ne.contains(&"Untitled".to_owned()), "{field:?} ne");
        assert_eq!(
            titles(
                &f,
                Filter::Missing {
                    field: field.clone()
                }
            ),
            ["Untitled"]
        );
        let (facet, _) = f.lib.facet(&field, &Filter::All, ALL).unwrap();
        let unknown = facet.last().unwrap();
        assert_eq!((&unknown.value, unknown.tracks), (&None, 1), "{field:?}");
    }
    // Title falls back to the file name (documented): an untagged file is "missing" a
    // title but sorts and matches by its file name.
    let f = fixture();
    let untagged = crate::tags::read::TrackMeta {
        duration_ms: Some(1000),
        ..Default::default()
    };
    upsert(
        &f.lib,
        vec![
            record(f.root, "x/Zebra.flac", untagged),
            record(f.root, "x/a.flac", meta("Apple", "A", "B")),
        ],
    );
    assert_eq!(titles(&f, txt(Field::Title, TextOp::Equals, "zebra")), [""]);
    assert_eq!(
        titles(
            &f,
            Filter::Missing {
                field: Field::Title
            }
        ),
        [""]
    );
    let (rows, _) = f
        .lib
        .tracks_query(
            &TrackQuery {
                filter: Filter::All,
                sort: vec![SortKey {
                    field: Field::Title,
                    descending: true,
                }],
            },
            ALL,
            &[],
        )
        .unwrap();
    assert_eq!(rows[0].track.rel_path, "x/Zebra.flac");
}

/// A flat `Or` of many comparisons used to compile to `a OR b OR c...`, which SQLite
/// parses as a tree of depth n and refuses beyond 1000 (a `Failed` at read time, also for
/// a smart playlist saved with it). Over-wide queries are now `InvalidQuery` everywhere a
/// query is checked, and the widest accepted ones execute.
#[test]
fn review_wide_or_filter_is_accepted() {
    let f = build();
    let years = |n: i64| -> Vec<Filter> {
        (0..n)
            .map(|i| cmp(Field::Year, CompareOp::Eq, Value::Int(1900 + i)))
            .collect()
    };
    let too_wide = query(Filter::Or { any: years(1500) });
    let is_invalid = |r: Result<u64, LibraryError>| match r {
        Err(LibraryError::InvalidQuery(m)) => m.contains("1500 parts (at most 500)"),
        other => panic!("{other:?}"),
    };
    assert!(is_invalid(
        f.lib.tracks_query(&too_wide, ALL, &[]).map(|r| r.1)
    ));
    assert!(is_invalid(
        f.lib
            .facet(&Field::Year, &too_wide.filter, ALL)
            .map(|r| r.1)
    ));
    assert!(is_invalid(
        f.lib
            .create_playlist(
                "wide",
                crate::PlaylistKind::Smart {
                    query: too_wide.clone(),
                    max_tracks: None,
                },
                UI,
            )
            .map(|done| done.value.0 as u64)
    ));
    let p = f
        .lib
        .create_playlist(
            "ok",
            crate::PlaylistKind::Smart {
                query: query(Filter::All),
                max_tracks: None,
            },
            UI,
        )
        .unwrap()
        .value;
    assert!(is_invalid(
        f.lib
            .set_smart_query(p, too_wide, None, None, UI)
            .map(|done| done.value)
    ));

    // The widest accepted `Or` runs, as a query and as a smart playlist.
    let widest = query(Filter::Or { any: years(500) });
    let (rows, _) = f.lib.tracks_query(&widest, ALL, &[]).unwrap();
    assert_eq!(rows.len(), 5);
    f.lib.set_smart_query(p, widest, None, None, UI).unwrap();
    assert_eq!(f.lib.playlist(p).unwrap().unwrap().track_count, 5);

    // So do the largest accepted queries: 2000 nodes of wide, nested groups...
    let mut big = Filter::Or { any: years(499) };
    let mut nodes = 500;
    for _ in 0..3 {
        let mut any = years(497);
        any.push(big);
        big = Filter::Or { any };
        nodes += 498;
    }
    while nodes < 2000 {
        big = Filter::Not {
            filter: Box::new(Filter::Not {
                filter: Box::new(big),
            }),
        };
        nodes += 2;
    }
    let (rows, _) = f.lib.tracks_query(&query(big.clone()), ALL, &[]).unwrap();
    assert_eq!(rows.len(), 5);
    let (values, _) = f.lib.facet(&Field::Genre, &big, ALL).unwrap();
    assert!(!values.is_empty());
    //... and a wide group as deep as nesting goes.
    let mut deep = Filter::Or { any: years(500) };
    for _ in 0..62 {
        deep = Filter::Not {
            filter: Box::new(deep),
        };
    }
    let (rows, _) = f.lib.tracks_query(&query(deep), ALL, &[]).unwrap();
    assert_eq!(rows.len(), 5);

    // One node more than allowed.
    let too_many = Filter::Not {
        filter: Box::new(big),
    };
    match f.lib.tracks_query(&query(too_many), ALL, &[]) {
        Err(LibraryError::InvalidQuery(m)) => assert!(m.contains("2000"), "{m}"),
        other => panic!("{other:?}"),
    }
}

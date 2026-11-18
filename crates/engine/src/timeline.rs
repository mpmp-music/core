//! Maps the audible content index to (entry, source frame).
//!
//! The renderer reports where each accepted segment begins in the content index; the
//! clock seqlock says which content index is being heard right now.
//!
//! The same reports give the time heard per entry: a segment played from its report's
//! content index up to the next report's (at most its own length), and the last one up to
//! the renderer's current content index. Flushed frames never enter the content index, so
//! they are never counted.

use std::collections::VecDeque;
use std::time::Duration;

use rtrb::Consumer;

use crate::segment::{ClockSample, EntryId, SegmentReport, flags};

/// Where playback is, in the source's own frames, for the entry being heard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub entry: EntryId,
    /// Track frame at the source sample rate, relative to the track's range start.
    pub frame: u64,
    pub sample_rate: u32,
}

impl Position {
    pub fn as_duration(self) -> Duration {
        Duration::from_secs_f64(self.frame as f64 / f64::from(self.sample_rate))
    }
}

/// How much of an entry the output has played: content frames at the device rate.
///
/// Counts every frame the renderer played of the entry, across seeks, pauses and gapless
/// transitions (see [`Engine::heard`](crate::Engine::heard)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Heard {
    pub frames: u64,
    /// The device rate the frames were played at.
    pub sample_rate: u32,
}

impl Heard {
    /// The frames as time, exact to the nanosecond (truncated).
    pub fn as_duration(self) -> Duration {
        let rate = u64::from(self.sample_rate.max(1));
        let secs = self.frames / rate;
        let nanos = (self.frames % rate) * 1_000_000_000 / rate;
        Duration::new(secs, nanos as u32)
    }
}

/// Entries kept in a [`HeardLog`]: the entry playing and the last few before it, so that
/// core can ask after the next track started, after a stop or after a failure.
pub(crate) const HEARD_HISTORY: usize = 16;

/// Played frames per entry, at one device rate, for the [`HEARD_HISTORY`] entries most
/// recently added to (least recent first).
#[derive(Debug, Clone, Default)]
pub(crate) struct HeardLog {
    sample_rate: u32,
    entries: VecDeque<(EntryId, u64)>,
}

impl HeardLog {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            entries: VecDeque::new(),
        }
    }

    /// Adds `frames` to `entry` (0 just makes it known) and makes it the most recent.
    pub fn add(&mut self, entry: EntryId, frames: u64) {
        let mut total = frames;
        if let Some(at) = self.entries.iter().position(|(e, _)| *e == entry) {
            total += self.entries.remove(at).map_or(0, |(_, f)| f);
        } else if self.entries.len() == HEARD_HISTORY {
            self.entries.pop_front();
        }
        self.entries.push_back((entry, total));
    }

    pub fn get(&self, entry: EntryId) -> Option<Heard> {
        let (_, frames) = self.entries.iter().find(|(e, _)| *e == entry)?;
        Some(Heard {
            frames: *frames,
            sample_rate: self.sample_rate,
        })
    }

    /// Adds a later log (of a newer output stream) to this one. Frames played at another
    /// device rate are converted to the later rate.
    pub fn merge(&mut self, later: &HeardLog) {
        if self.sample_rate != later.sample_rate {
            for (_, frames) in &mut self.entries {
                *frames = rescale(*frames, self.sample_rate, later.sample_rate);
            }
            self.sample_rate = later.sample_rate;
        }
        for &(entry, frames) in &later.entries {
            self.add(entry, frames);
        }
    }
}

/// `frames` at rate `from`, as frames at rate `to` (rounded down).
pub(crate) fn rescale(frames: u64, from: u32, to: u32) -> u64 {
    if from == 0 {
        return 0;
    }
    (u128::from(frames) * u128::from(to) / u128::from(from)) as u64
}

pub(crate) struct Timeline {
    out_rate: u32,
    audible_cursor: u64,
    requested_seek: Option<(u64, EntryId, u64)>,
    applied_seek: Option<(u64, u32)>,
    /// Reports from the renderer; drained by whoever holds the timeline, so a position
    /// query is never staler than the last rendered buffer.
    incoming: Option<Consumer<SegmentReport>>,
    reports: VecDeque<SegmentReport>,
    /// Track starts not yet audible.
    pending_starts: VecDeque<SegmentReport>,
    /// Frames played per entry, of every segment but the last reported one.
    heard: HeardLog,
    /// The last reported segment: how much of it has played depends on the renderer's
    /// current content index.
    open: Option<SegmentReport>,
}

impl Timeline {
    pub fn new(out_rate: u32, incoming: Option<Consumer<SegmentReport>>) -> Self {
        Self {
            out_rate,
            audible_cursor: 0,
            requested_seek: None,
            applied_seek: None,
            incoming,
            reports: VecDeque::new(),
            pending_starts: VecDeque::new(),
            heard: HeardLog::new(out_rate),
            open: None,
        }
    }

    pub fn request_seek(&mut self, entry: EntryId, frame: u64) -> u64 {
        let token = self.requested_seek.map_or(1, |(token, _, _)| token + 1);
        self.requested_seek = Some((token, entry, frame));
        token
    }

    pub fn apply_seek(&mut self, token: u64, generation: u32) {
        self.applied_seek = Some((token, generation));
    }

    pub fn pending_seek_at(&self, index: u64) -> Option<(EntryId, u64)> {
        let (requested, entry, frame) = self.requested_seek?;
        let rendered = self.reports.iter().rev().find(|r| r.content_index < index);
        let fulfilled = self.applied_seek.is_some_and(|(applied, generation)| {
            applied == requested && rendered.is_some_and(|r| r.segment.generation >= generation)
        });
        (!fulfilled).then_some((entry, frame))
    }

    /// Takes in everything the renderer has reported so far.
    pub fn sync(&mut self) {
        while let Some(report) = self.incoming.as_mut().and_then(|c| c.pop().ok()) {
            self.push(report);
        }
    }

    pub fn push(&mut self, report: SegmentReport) {
        // The previous segment played until this one began, unless it ran out (or was
        // cut short by a flush, whose dropped frames never advance the content index).
        if let Some(previous) = self.open.replace(report) {
            let played = report
                .content_index
                .saturating_sub(previous.content_index)
                .min(u64::from(previous.segment.frames));
            self.heard.add(previous.segment.entry, played);
        }
        self.heard.add(report.segment.entry, 0);
        if report.segment.flags & flags::TRACK_START != 0 {
            self.pending_starts.push_back(report);
        }
        self.reports.push_back(report);
    }

    /// Makes `entry` known with nothing heard yet (it was just started).
    pub fn register(&mut self, entry: EntryId) {
        self.heard.add(entry, 0);
    }

    /// Frames of `entry` played so far, given the renderer's content index
    /// `frames_played`, read *before* the last [`sync`](Self::sync): the renderer pushes
    /// a segment's report before it publishes frames of it, so every report the index
    /// covers has been taken in.
    pub fn heard(&self, entry: EntryId, frames_played: u64) -> Option<Heard> {
        let mut heard = self.heard.get(entry)?;
        if let Some(open) = self.open.filter(|r| r.segment.entry == entry) {
            heard.frames += frames_played
                .saturating_sub(open.content_index)
                .min(u64::from(open.segment.frames));
        }
        Some(heard)
    }

    /// The output stream has stopped at content index `frames_played` (read after the
    /// renderer is gone): takes in the last reports and returns what each entry played.
    pub fn finish(&mut self, frames_played: u64) -> HeardLog {
        self.sync();
        if let Some(open) = self.open.take() {
            let played = frames_played
                .saturating_sub(open.content_index)
                .min(u64::from(open.segment.frames));
            self.heard.add(open.segment.entry, played);
        }
        std::mem::replace(&mut self.heard, HeardLog::new(self.out_rate))
    }

    /// The content index being heard at `now_ns` (same clock as `playback_at_ns`).
    pub fn audible_index(&mut self, clock: ClockSample, now_ns: u64) -> u64 {
        let elapsed = i128::from(now_ns) - i128::from(clock.playback_at_ns);
        let frames = (elapsed * i128::from(self.out_rate)).div_euclid(1_000_000_000);
        // Never ahead of what was actually rendered: fades, underruns and pauses publish
        // fewer content frames than the buffer holds.
        let frames = frames.min(i128::from(clock.content_frames));
        let index = (i128::from(clock.content_start) + frames).max(0) as u64;
        // Content indices remain monotonic even across seeks. Jitter or a larger
        // (but still plausible) device delay must never revisit pruned reports.
        self.audible_cursor = self.audible_cursor.max(index);
        self.audible_cursor
    }

    pub fn position_at(&self, index: u64) -> Option<Position> {
        let at = self.reports.partition_point(|r| r.content_index <= index);
        let report = self.reports.get(at.checked_sub(1)?)?;
        let segment = report.segment;
        let offset = (index - report.content_index).min(u64::from(segment.frames));
        let source_offset =
            offset * u64::from(segment.source_frames) / u64::from(segment.frames.max(1));
        Some(Position {
            entry: segment.entry,
            frame: segment.source_start + source_offset,
            sample_rate: segment.source_rate,
        })
    }

    /// Track starts that have become audible by `index`, oldest first.
    pub fn take_due_starts(&mut self, index: u64) -> impl Iterator<Item = SegmentReport> + '_ {
        let due = self
            .pending_starts
            .partition_point(|r| r.content_index <= index);
        self.pending_starts.drain(..due)
    }

    /// Forgets segments that ended before `index`, keeping the one in play.
    pub fn prune(&mut self, index: u64) {
        let at = self.reports.partition_point(|r| r.content_index <= index);
        let drop = at.saturating_sub(1);
        self.reports.drain(..drop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::Segment;

    fn report(
        content_index: u64,
        entry: u64,
        source_start: u64,
        frames: u32,
        start: bool,
    ) -> SegmentReport {
        SegmentReport {
            content_index,
            segment: Segment {
                generation: 0,
                entry: EntryId(entry),
                frames,
                // 48 kHz output for a 44.1 kHz source.
                source_start,
                source_frames: frames * 147 / 160,
                source_rate: 44_100,
                flags: if start { flags::TRACK_START } else { 0 },
                gain: 1.0,
            },
        }
    }

    #[test]
    fn maps_content_index_to_source_frames() {
        let mut timeline = Timeline::new(48_000, None);
        timeline.push(report(0, 1, 0, 1600, true));
        timeline.push(report(1600, 1, 1470, 1600, false));
        timeline.push(report(3200, 2, 0, 1600, true));
        let p = timeline.position_at(1600 + 800).unwrap();
        assert_eq!((p.entry, p.frame), (EntryId(1), 1470 + 735));
        let p = timeline.position_at(3200).unwrap();
        assert_eq!((p.entry, p.frame), (EntryId(2), 0));

        let starts: Vec<_> = timeline
            .take_due_starts(3199)
            .map(|r| r.segment.entry)
            .collect();
        assert_eq!(starts, [EntryId(1)]);
        let starts: Vec<_> = timeline
            .take_due_starts(3200)
            .map(|r| r.segment.entry)
            .collect();
        assert_eq!(starts, [EntryId(2)]);

        timeline.prune(3300);
        assert_eq!(timeline.position_at(3300).unwrap().entry, EntryId(2));
        assert!(timeline.position_at(100).is_none());
    }

    #[test]
    fn heard_counts_played_frames_per_entry() {
        let mut timeline = Timeline::new(48_000, None);
        assert_eq!(timeline.heard(EntryId(1), 0), None);
        timeline.register(EntryId(1));
        assert_eq!(timeline.heard(EntryId(1), 0).unwrap().frames, 0);
        // Entry 1 plays 1000 frames of a segment, a seek flushes the rest (the content
        // index does not move for dropped frames), and its new segment plays 300.
        timeline.push(report(0, 1, 0, 1600, true));
        assert_eq!(timeline.heard(EntryId(1), 1000).unwrap().frames, 1000);
        timeline.push(report(1000, 1, 50_000, 1600, false));
        assert_eq!(timeline.heard(EntryId(1), 1300).unwrap().frames, 1300);
        // A segment that ran out before the next report: capped at its length.
        timeline.push(report(2600, 2, 0, 1600, true));
        timeline.push(report(5000, 2, 1470, 1600, false));
        assert_eq!(timeline.heard(EntryId(1), 5000).unwrap().frames, 2600);
        assert_eq!(timeline.heard(EntryId(2), 5000).unwrap().frames, 1600);
        // The open segment, never past its own length.
        assert_eq!(timeline.heard(EntryId(2), 5100).unwrap().frames, 1700);
        assert_eq!(timeline.heard(EntryId(2), 9999).unwrap().frames, 3200);
        let log = timeline.finish(5400);
        assert_eq!(log.get(EntryId(2)).unwrap().frames, 2000);
        assert_eq!(log.get(EntryId(1)).unwrap().frames, 2600);
        assert_eq!(log.get(EntryId(3)), None);
    }

    #[test]
    fn heard_log_is_bounded_and_merges_across_rates() {
        let mut log = HeardLog::new(44_100);
        for entry in 0..HEARD_HISTORY as u64 + 2 {
            log.add(EntryId(entry), 441);
        }
        assert_eq!(log.get(EntryId(0)), None);
        assert_eq!(log.get(EntryId(1)), None);
        assert_eq!(log.get(EntryId(2)).unwrap().frames, 441);
        let mut later = HeardLog::new(48_000);
        later.add(EntryId(2), 480);
        later.add(EntryId(100), 48_000);
        log.merge(&later);
        let heard = log.get(EntryId(2)).unwrap();
        assert_eq!((heard.frames, heard.sample_rate), (960, 48_000));
        assert_eq!(
            log.get(EntryId(100)).unwrap().as_duration(),
            Duration::from_secs(1)
        );
        assert_eq!(
            Heard {
                frames: 44_100 + 1,
                sample_rate: 44_100
            }
            .as_duration(),
            Duration::new(1, 22_675)
        );
    }

    #[test]
    fn audible_index_never_runs_ahead_of_rendered_content() {
        let mut timeline = Timeline::new(48_000, None);
        let clock = ClockSample {
            content_start: 1000,
            content_frames: 100,
            playback_at_ns: 1_000_000_000,
        };
        // 1 ms before the buffer is heard: 48 frames earlier.
        assert_eq!(timeline.audible_index(clock, 999_000_000), 952);
        assert_eq!(timeline.audible_index(clock, 1_001_000_000), 1048);
        // Long after: clamped to the content actually rendered (e.g. paused since).
        assert_eq!(timeline.audible_index(clock, 9_000_000_000), 1100);
    }

    #[test]
    fn plausible_delay_increase_cannot_revisit_pruned_reports() {
        let mut timeline = Timeline::new(48000, None);
        timeline.push(report(0, 1, 0, 1000, true));
        timeline.push(report(1000, 2, 0, 1000, true));
        let index = timeline.audible_index(
            ClockSample {
                content_start: 1200,
                content_frames: 100,
                playback_at_ns: 1_000_000_000,
            },
            1_000_000_000,
        );
        timeline.prune(index);
        let next = timeline.audible_index(
            ClockSample {
                content_start: 1300,
                content_frames: 100,
                playback_at_ns: 1_100_000_000,
            },
            1_001_000_000,
        );
        assert_eq!(next, index);
        assert_eq!(timeline.position_at(next).unwrap().entry, EntryId(2));
    }

    #[test]
    fn only_rendered_content_of_the_latest_seek_fulfills_its_target() {
        let mut timeline = Timeline::new(44_100, None);
        let entry = EntryId(1);
        let token = timeline.request_seek(entry, 1000);
        assert_eq!(timeline.pending_seek_at(0), Some((entry, 1000)));
        timeline.apply_seek(token, 2);
        let segment = Segment {
            generation: 1,
            entry,
            frames: 100,
            source_start: 0,
            source_frames: 100,
            source_rate: 44_100,
            flags: 0,
            gain: 1.0,
        };
        timeline.push(SegmentReport {
            content_index: 0,
            segment,
        });
        assert_eq!(timeline.pending_seek_at(100), Some((entry, 1000)));
        timeline.push(SegmentReport {
            content_index: 100,
            segment: Segment {
                generation: 2,
                source_start: 1000,
                ..segment
            },
        });
        assert_eq!(timeline.pending_seek_at(100), Some((entry, 1000)));
        assert_eq!(timeline.pending_seek_at(150), None);
        assert_eq!(timeline.position_at(150).unwrap().frame, 1050);
        timeline.request_seek(entry, 20);
        assert_eq!(timeline.pending_seek_at(150), Some((entry, 20)));
    }
}

//! Lock-free structures shared between the decode thread and the renderer.

use std::sync::atomic::{AtomicU64, Ordering};

/// Identifies one entry of the play queue, so the same file queued twice stays distinct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryId(pub u64);

/// Monotonic id of a run of contiguous content (a track start, a seek, a re-render after a
/// partial flush). Limited to 21 bits so that three fit in one atomic word.
pub(crate) type Generation = u32;

pub(crate) const MAX_GENERATION: Generation = (1 << 21) - 1;

pub(crate) mod flags {
    /// First segment of a track (or of a track restarted by a seek).
    pub const TRACK_START: u8 = 1;
    /// The samples may not be exact 16-bit values (a source deeper than 16 bits, a lossy
    /// or float source, resampling, a mixing channel map): integer 16-bit output must
    /// dither them even at unity gain. Without it the segment is "clean" for 16
    /// bits and unity gain leaves it bit-exact.
    pub const NEEDS_DITHER: u8 = 2;
}

/// Describes samples already pushed to the sample ring. Written after its samples, so the
/// renderer never reads samples no descriptor covers.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Segment {
    pub generation: Generation,
    pub entry: EntryId,
    /// Frames in the sample ring, at the device rate.
    pub frames: u32,
    /// Track frame (source rate) of the first frame.
    pub source_start: u64,
    /// Source frames this segment spans; with `frames` it maps device frames to source
    /// frames exactly per segment rather than by one global ratio.
    pub source_frames: u32,
    pub source_rate: u32,
    pub flags: u8,
    /// Linear gain of the segment's track (ReplayGain), fixed when the decode thread
    /// writes the segment, so a change follows the audio exactly.
    pub gain: f32,
}

/// The renderer tells the decode thread exactly where each segment it accepted begins in
/// the content index. Pushed from the realtime thread through an SPSC ring.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SegmentReport {
    pub content_index: u64,
    pub segment: Segment,
}

/// Which generations may be played: `lo <= g < keep_below`, or `g >= valid_from`.
///
/// All three live in one `AtomicU64` so that updates never tear. Invariant:
/// `lo <= keep_below <= valid_from`.
pub(crate) struct FlushWord(AtomicU64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FlushState {
    pub lo: Generation,
    pub keep_below: Generation,
    pub valid_from: Generation,
}

impl FlushState {
    pub fn allows(self, g: Generation) -> bool {
        (self.lo <= g && g < self.keep_below) || g >= self.valid_from
    }

    fn pack(self) -> u64 {
        (u64::from(self.lo) << 42) | (u64::from(self.keep_below) << 21) | u64::from(self.valid_from)
    }

    fn unpack(word: u64) -> Self {
        let mask = u64::from(MAX_GENERATION);
        Self {
            lo: ((word >> 42) & mask) as Generation,
            keep_below: ((word >> 21) & mask) as Generation,
            valid_from: (word & mask) as Generation,
        }
    }
}

/// Returned when the renderer has already begun playing the generation a partial flush
/// wanted to drop (when a partial flush arrives too late).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AlreadyStarted;

impl FlushWord {
    pub fn new() -> Self {
        Self(AtomicU64::new(
            FlushState {
                lo: 0,
                keep_below: 0,
                valid_from: 0,
            }
            .pack(),
        ))
    }

    pub fn load(&self) -> FlushState {
        FlushState::unpack(self.0.load(Ordering::Acquire))
    }

    /// Drops everything before `first`; content from `first` on plays.
    pub fn flush_all(&self, first: Generation) {
        self.0.store(
            FlushState {
                lo: first,
                keep_below: first,
                valid_from: first,
            }
            .pack(),
            Ordering::Release,
        );
    }

    /// Drops generations `drop_from..replacement`, keeping what is older (the current
    /// track) and what comes after (the new next track). Fails if the renderer has
    /// already committed to `drop_from`.
    pub fn flush_from(
        &self,
        drop_from: Generation,
        replacement: Generation,
    ) -> Result<(), AlreadyStarted> {
        let mut word = self.0.load(Ordering::Acquire);
        loop {
            let state = FlushState::unpack(word);
            if state.keep_below > drop_from {
                return Err(AlreadyStarted);
            }
            let next = FlushState {
                lo: state.lo.min(drop_from),
                keep_below: drop_from,
                valid_from: replacement,
            };
            match self.0.compare_exchange_weak(
                word,
                next.pack(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => word = actual,
            }
        }
    }

    /// Called by the renderer when it starts on generation `g`. Returns false if `g` was
    /// flushed; otherwise records the commitment so a later partial flush of `g` fails.
    pub fn commit(&self, g: Generation) -> bool {
        let mut word = self.0.load(Ordering::Acquire);
        loop {
            let state = FlushState::unpack(word);
            if !state.allows(g) {
                return false;
            }
            let keep_below = state.keep_below.max((g + 1).min(MAX_GENERATION));
            let next = FlushState {
                lo: state.lo.min(g),
                keep_below,
                valid_from: state.valid_from.max(keep_below),
            };
            if next == state {
                return true;
            }
            match self.0.compare_exchange_weak(
                word,
                next.pack(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => word = actual,
            }
        }
    }
}

/// What the renderer last published about the audible clock, via a seqlock so the
/// realtime side never waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ClockSample {
    /// Content index at the start of the last rendered buffer.
    pub content_start: u64,
    /// Content frames in that buffer (excludes silence).
    pub content_frames: u64,
    /// When that buffer's first frame is heard, in ns since the engine's clock epoch.
    pub playback_at_ns: u64,
}

pub(crate) struct ClockSeqlock {
    seq: AtomicU64,
    content_start: AtomicU64,
    content_frames: AtomicU64,
    playback_at_ns: AtomicU64,
}

impl ClockSeqlock {
    pub fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            content_start: AtomicU64::new(0),
            content_frames: AtomicU64::new(0),
            playback_at_ns: AtomicU64::new(0),
        }
    }

    /// Single writer: the renderer.
    pub fn publish(&self, sample: ClockSample) {
        let seq = self.seq.load(Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Release);
        self.content_start
            .store(sample.content_start, Ordering::Relaxed);
        self.content_frames
            .store(sample.content_frames, Ordering::Relaxed);
        self.playback_at_ns
            .store(sample.playback_at_ns, Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(2), Ordering::Release);
    }

    pub fn read(&self) -> ClockSample {
        loop {
            let before = self.seq.load(Ordering::Acquire);
            if before % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let sample = ClockSample {
                content_start: self.content_start.load(Ordering::Relaxed),
                content_frames: self.content_frames.load(Ordering::Relaxed),
                playback_at_ns: self.playback_at_ns.load(Ordering::Relaxed),
            };
            std::sync::atomic::fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == before {
                return sample;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_flush_drops_older_generations() {
        let word = FlushWord::new();
        assert!(word.commit(0));
        word.flush_all(3);
        assert!(!word.commit(1));
        assert!(!word.commit(2));
        assert!(word.commit(3));
        assert!(word.commit(4));
    }

    #[test]
    fn partial_flush_keeps_current_and_replacement() {
        // Current track is generation 5, prefetched next is 6, replacement is 7.
        let word = FlushWord::new();
        word.flush_all(5);
        assert!(word.commit(5));
        word.flush_from(6, 7).unwrap();
        assert!(word.load().allows(5));
        assert!(!word.commit(6));
        assert!(word.commit(7));
    }

    #[test]
    fn two_partial_flushes_in_a_row_drop_both_old_nexts() {
        let word = FlushWord::new();
        word.flush_all(5);
        assert!(word.commit(5));
        word.flush_from(6, 7).unwrap();
        word.flush_from(6, 8).unwrap();
        let state = word.load();
        assert!(state.allows(5));
        assert!(!state.allows(6));
        assert!(!state.allows(7));
        assert!(state.allows(8));
    }

    #[test]
    fn partial_flush_after_seek_does_not_revive_pre_seek_data() {
        // Generation 2 was playing, a seek started generation 3, and the renderer has not
        // reached 3 yet when the next track (4) is replaced by 5.
        let word = FlushWord::new();
        assert!(word.commit(2));
        word.flush_all(3);
        word.flush_from(4, 5).unwrap();
        let state = word.load();
        assert!(!state.allows(2), "{state:?}");
        assert!(state.allows(3));
        assert!(!state.allows(4));
        assert!(state.allows(5));
    }

    #[test]
    fn partial_flush_fails_once_next_has_started() {
        let word = FlushWord::new();
        word.flush_all(5);
        assert!(word.commit(5));
        assert!(word.commit(6));
        assert_eq!(word.flush_from(6, 7), Err(AlreadyStarted));
    }

    #[test]
    fn seek_after_partial_flush_discards_the_re_rendered_next() {
        let word = FlushWord::new();
        word.flush_all(5);
        assert!(word.commit(5));
        word.flush_from(6, 7).unwrap();
        word.flush_all(8);
        assert!(!word.commit(5));
        assert!(!word.commit(7));
        assert!(word.commit(8));
    }

    #[test]
    fn invariant_holds_after_commits() {
        let word = FlushWord::new();
        word.flush_all(3);
        word.flush_from(5, 9).unwrap();
        for g in [3, 4, 9, 10] {
            assert!(word.commit(g));
            let s = word.load();
            assert!(
                s.lo <= s.keep_below && s.keep_below <= s.valid_from,
                "{s:?}"
            );
        }
    }

    #[test]
    fn seqlock_round_trip() {
        let lock = ClockSeqlock::new();
        let sample = ClockSample {
            content_start: 10,
            content_frames: 512,
            playback_at_ns: 99,
        };
        lock.publish(sample);
        assert_eq!(lock.read(), sample);
    }
}

//! The play queue: stable entry ids, a revision bumped by
//! every edit, optimistic concurrency via `expected_revision`, shuffle and repeat, and
//! single-step undo per revision. Edits that change nothing leave the revision alone.

use crate::contract::{
    ContractError, EntryId, ErrorCode, InsertAt, ObjectRef, PlayContext, QueueRevision as Revision,
    RepeatMode, TrackId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entry {
    pub id: EntryId,
    pub track: TrackId,
}

/// The queue before the last edit. Only the last edit can be undone, so one copy
/// is all there is; the shuffle order is rebuilt rather than restored.
#[derive(Debug, Clone)]
struct Saved {
    /// The revision the edit produced; undo is allowed only while it is current.
    revision: Revision,
    entries: Vec<Entry>,
    context: Option<PlayContext>,
    /// Clear detaches the current entry without changing playback. Reconnect it on
    /// undo; other edits must keep following any navigation performed meanwhile.
    cleared_current: Option<EntryId>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct Queue {
    entries: Vec<Entry>,
    /// The entry being heard (or last heard); playback state lives in the actor.
    current: Option<EntryId>,
    /// When the current entry was removed: what followed it, so playback continues from
    /// there instead of the top of the queue.
    successor: Option<EntryId>,
    revision: Revision,
    next_id: u64,
    shuffle: bool,
    /// Play order while shuffled: a permutation of the entries.
    order: Vec<EntryId>,
    repeat: RepeatMode,
    context: Option<PlayContext>,
    undo: Option<Saved>,
}

pub(crate) type Edit<T> = Result<T, ContractError>;

fn entry_not_found(id: EntryId) -> ContractError {
    ContractError::not_found(ErrorCode::EntryNotFound, format!("queue entry {}", id.0))
        .with_missing(vec![ObjectRef::Entry { id }])
}

impl Queue {
    /// An empty queue whose first new entry gets id `next_entry_id` and whose revision is
    /// `revision` (ids and revisions never go back, also across restarts).
    pub fn with_seeds(next_entry_id: u64, revision: u64) -> Self {
        Self {
            next_id: next_entry_id.saturating_sub(1),
            revision: Revision(revision),
            ..Self::default()
        }
    }

    /// The id the next new entry gets.
    pub fn next_entry_id(&self) -> u64 {
        self.next_id + 1
    }

    /// Puts back a saved queue, keeping its entry ids (an id of 0, or one already taken,
    /// gets a new one). Not an edit: the revision stays.
    pub fn restore(&mut self, entries: &[(EntryId, TrackId)], context: Option<PlayContext>) {
        let mut seen = std::collections::HashSet::new();
        self.next_id = entries
            .iter()
            .map(|(id, _)| id.0)
            .max()
            .unwrap_or(0)
            .max(self.next_id);
        self.entries = entries
            .iter()
            .map(|&(id, track)| {
                let id = if id.0 == 0 || !seen.insert(id) {
                    self.next_id += 1;
                    EntryId(self.next_id)
                } else {
                    id
                };
                Entry { id, track }
            })
            .collect();
        self.context = context;
        self.current = None;
        self.successor = None;
        self.undo = None;
        self.order.clear();
        if self.shuffle {
            self.reshuffle(None);
        }
    }

    pub fn revision(&self) -> Revision {
        self.revision
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn current(&self) -> Option<EntryId> {
        self.current
    }

    pub fn current_index(&self) -> Option<usize> {
        self.current.and_then(|id| self.index_of(id))
    }

    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    pub fn repeat(&self) -> RepeatMode {
        self.repeat
    }

    pub fn context(&self) -> Option<&PlayContext> {
        self.context.as_ref()
    }

    pub fn entry(&self, id: EntryId) -> Option<Entry> {
        self.entries.iter().find(|e| e.id == id).copied()
    }

    pub fn index_of(&self, id: EntryId) -> Option<usize> {
        self.entries.iter().position(|e| e.id == id)
    }

    /// Playback moved to `id` (not a queue edit: the revision stays).
    pub fn set_current(&mut self, id: Option<EntryId>) {
        self.current = id.filter(|&id| self.index_of(id).is_some());
        if self.current.is_some() {
            self.successor = None;
        }
    }

    /// Playback starts at `id` on request (play, jump): while shuffled, the play order is
    /// redrawn starting there, so every entry still plays once.
    pub fn start_at(&mut self, id: EntryId) {
        self.set_current(Some(id));
        if self.shuffle && self.current.is_some() {
            self.reshuffle(Some(id));
        }
    }

    /// What plays after the current playback position: after the current entry, after
    /// a removed current entry's place, or nothing.
    pub fn upcoming(&self) -> Option<EntryId> {
        match self.current {
            Some(current) => self.next_after(Some(current)),
            None => self.successor.filter(|&s| self.index_of(s).is_some()),
        }
    }

    /// Where a skip (`Next`) goes: like [`Queue::upcoming`], except that repeat one does
    /// not hold the current entry (it applies only when a track ends by itself); with it,
    /// a skip moves on as with repeat off.
    pub fn skip_target(&self) -> Option<EntryId> {
        match self.current {
            Some(current) if self.repeat == RepeatMode::One => self.next_after_in_order(current),
            _ => self.upcoming(),
        }
    }

    /// Replaces the queue (a new play context). Returns the new entries.
    pub fn replace(&mut self, tracks: &[TrackId], context: Option<PlayContext>) -> Vec<EntryId> {
        self.save();
        self.entries.clear();
        self.order.clear();
        self.current = None;
        self.successor = None;
        self.context = context;
        let created = self.push_new(tracks, self.entries.len());
        if self.shuffle {
            self.reshuffle(None);
        }
        self.bump();
        created
    }

    /// Adds tracks; none is a no-op (no new revision).
    pub fn enqueue(
        &mut self,
        tracks: &[TrackId],
        at: InsertAt,
        expected: Option<Revision>,
    ) -> Edit<Vec<EntryId>> {
        let index = match at {
            InsertAt::End => self.entries.len(),
            InsertAt::Next => self.current_index().map_or(0, |i| i + 1),
            _ => {
                return Err(
                    ContractError::unknown_variant("insert position").with_field("command.at")
                );
            }
        };
        self.check(expected)?;
        if tracks.is_empty() {
            return Ok(Vec::new());
        }
        self.save();
        let created = self.push_new(tracks, index);
        if self.shuffle {
            // New entries play after the current one: right after it for Next, at
            // random later places otherwise.
            let after = self
                .current
                .and_then(|c| self.order.iter().position(|&e| e == c));
            let base = after.map_or(0, |i| i + 1);
            for (k, &id) in created.iter().enumerate() {
                let at_ = match at {
                    InsertAt::Next => base + k,
                    _ => fastrand::usize(base + k..=self.order.len()),
                };
                self.order.insert(at_, id);
            }
        }
        self.bump();
        Ok(created)
    }

    /// Removes the entries that are in the queue (others are ignored): those removed, in
    /// request order. Removing nothing is a no-op (no new revision).
    pub fn remove(&mut self, ids: &[EntryId], expected: Option<Revision>) -> Edit<Vec<EntryId>> {
        self.check(expected)?;
        let mut removed: Vec<EntryId> = Vec::new();
        for &id in ids {
            if self.index_of(id).is_some() && !removed.contains(&id) {
                removed.push(id);
            }
        }
        if removed.is_empty() {
            return Ok(removed);
        }
        self.save();
        if let Some(current) = self.current.filter(|c| removed.contains(c)) {
            // Remember where playback was: the first following entry that survives.
            let mut after = Some(current);
            self.successor = loop {
                after = after.and_then(|a| self.next_after_in_order(a));
                match after {
                    Some(a) if removed.contains(&a) => continue,
                    other => break other,
                }
            };
            self.current = None;
        }
        self.entries.retain(|e| !removed.contains(&e.id));
        self.order.retain(|e| !removed.contains(e));
        self.bump();
        Ok(removed)
    }

    /// Moves an entry to final index `to`: whether it moved (to where it is: no-op).
    pub fn move_entry(&mut self, id: EntryId, to: usize, expected: Option<Revision>) -> Edit<bool> {
        self.check(expected)?;
        let from = self.index_of(id).ok_or_else(|| entry_not_found(id))?;
        if to >= self.entries.len() {
            return Err(ContractError::invalid(
                ErrorCode::OutOfRange,
                format!(
                    "position {to} is past the end of a {}-entry queue",
                    self.entries.len()
                ),
            )
            .with_field("command.to"));
        }
        if from == to {
            return Ok(false);
        }
        self.save();
        let entry = self.entries.remove(from);
        self.entries.insert(to, entry);
        self.bump();
        Ok(true)
    }

    /// Empties the queue: whether there was anything to clear.
    pub fn clear(&mut self, expected: Option<Revision>) -> Edit<bool> {
        self.check(expected)?;
        if self.entries.is_empty() && self.context.is_none() {
            return Ok(false);
        }
        self.save();
        self.undo.as_mut().unwrap().cleared_current = self.current;
        self.entries.clear();
        self.order.clear();
        self.current = None;
        self.successor = None;
        self.context = None;
        self.bump();
        Ok(true)
    }

    /// Whether it changed.
    pub fn set_shuffle(&mut self, on: bool) -> bool {
        if on == self.shuffle {
            return false;
        }
        self.shuffle = on;
        if on {
            self.reshuffle(self.current);
        } else {
            self.order.clear();
        }
        true
    }

    /// Whether it changed.
    pub fn set_repeat(&mut self, mode: RepeatMode) -> bool {
        let changed = self.repeat != mode;
        self.repeat = mode;
        changed
    }

    /// Undoes the edit that produced `revision`, if nothing changed since.
    pub fn undo(&mut self, revision: Revision) -> Edit<()> {
        if revision != self.revision {
            return Err(ContractError::conflict(
                ErrorCode::QueueConflict,
                format!(
                    "the queue is at revision {}, not {}",
                    self.revision.0, revision.0
                ),
                revision.0,
                self.revision.0,
            ));
        }
        let saved = match self.undo.take() {
            Some(saved) if saved.revision == revision => saved,
            other => {
                self.undo = other;
                return Err(ContractError::not_found(
                    ErrorCode::NothingToUndo,
                    format!("nothing to undo at revision {}", revision.0),
                ));
            }
        };
        self.entries = saved.entries;
        self.context = saved.context;
        if self.current.is_none() {
            self.current = saved.cleared_current;
        }
        if self.current.is_some_and(|c| self.index_of(c).is_none()) {
            self.current = None;
        }
        if self.current.is_some() {
            self.successor = None;
        }
        if self.shuffle {
            self.reshuffle(self.current);
        }
        // Undo is itself a change; it cannot be undone (no redo in v1).
        self.revision = Revision(self.revision.0 + 1);
        Ok(())
    }

    /// The entry to play after `from` (`None`: from the start of the play order).
    pub fn next_after(&self, from: Option<EntryId>) -> Option<EntryId> {
        if self.repeat == RepeatMode::One && from.is_some() {
            return from;
        }
        let order = self.play_order();
        let next = match from.and_then(|id| order.iter().position(|&e| e == id)) {
            Some(i) => order.get(i + 1).copied(),
            None => order.first().copied(),
        };
        next.or_else(|| {
            (self.repeat == RepeatMode::All)
                .then(|| order.first().copied())
                .flatten()
        })
    }

    /// The entry after `from` in play order, ignoring repeat modes.
    fn next_after_in_order(&self, from: EntryId) -> Option<EntryId> {
        let order = self.play_order();
        let i = order.iter().position(|&e| e == from)?;
        order.get(i + 1).copied()
    }

    /// The entry before `from` in play order.
    pub fn previous_before(&self, from: EntryId) -> Option<EntryId> {
        let order = self.play_order();
        let i = order.iter().position(|&e| e == from)?;
        match i.checked_sub(1) {
            Some(p) => order.get(p).copied(),
            None => (self.repeat == RepeatMode::All)
                .then(|| order.last().copied())
                .flatten(),
        }
    }

    fn play_order(&self) -> Vec<EntryId> {
        if self.shuffle {
            self.order.clone()
        } else {
            self.entries.iter().map(|e| e.id).collect()
        }
    }

    fn reshuffle(&mut self, first: Option<EntryId>) {
        let mut rest: Vec<EntryId> = self
            .entries
            .iter()
            .map(|e| e.id)
            .filter(|&id| Some(id) != first)
            .collect();
        fastrand::shuffle(&mut rest);
        self.order = first.into_iter().chain(rest).collect();
    }

    fn push_new(&mut self, tracks: &[TrackId], index: usize) -> Vec<EntryId> {
        let created: Vec<EntryId> = tracks
            .iter()
            .map(|_| {
                self.next_id += 1;
                EntryId(self.next_id)
            })
            .collect();
        let new = created
            .iter()
            .zip(tracks)
            .map(|(&id, &track)| Entry { id, track });
        self.entries.splice(index..index, new);
        created
    }

    fn check(&self, expected: Option<Revision>) -> Edit<()> {
        match expected {
            Some(expected) if expected != self.revision => Err(ContractError::conflict(
                ErrorCode::QueueConflict,
                format!(
                    "the queue changed: expected revision {}, it is at {}",
                    expected.0, self.revision.0
                ),
                expected.0,
                self.revision.0,
            )
            .with_field("command.expected_revision")),
            _ => Ok(()),
        }
    }

    /// Remembers the state before an edit; `bump` then tags it with the new revision.
    fn save(&mut self) {
        self.undo = Some(Saved {
            revision: Revision(0),
            entries: self.entries.clone(),
            context: self.context.clone(),
            cleared_current: None,
        });
    }

    fn bump(&mut self) {
        self.revision = Revision(self.revision.0 + 1);
        if let Some(saved) = &mut self.undo {
            saved.revision = self.revision;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracks(ids: &[i64]) -> Vec<TrackId> {
        ids.iter().map(|&i| TrackId(i)).collect()
    }

    fn track_order(q: &Queue) -> Vec<i64> {
        q.entries().iter().map(|e| e.track.0).collect()
    }

    #[test]
    fn clear_undo_reconnects_current_entry_including_shuffle_and_next_insertion() {
        for shuffle in [false, true] {
            let mut q = Queue::default();
            let created = q.replace(&tracks(&[1, 2, 1, 2, 3]), None);
            q.start_at(created[3]);
            q.set_shuffle(shuffle);
            q.clear(None).unwrap();
            assert_eq!(q.current_index(), None);
            q.undo(q.revision()).unwrap();
            assert_eq!(q.current(), Some(created[3]));
            assert_eq!(q.current_index(), Some(3));
            let added = q.enqueue(&tracks(&[9]), InsertAt::Next, None).unwrap();
            assert_eq!(track_order(&q), [1, 2, 1, 2, 9, 3]);
            assert_eq!(q.skip_target(), Some(added[0]));
        }
    }

    #[test]
    fn entries_are_stable_and_revisions_count_edits() {
        let mut q = Queue::default();
        let created = q.replace(&tracks(&[1, 2, 3]), None);
        assert_eq!(q.revision(), Revision(1));
        q.set_current(Some(created[0]));
        assert_eq!(q.revision(), Revision(1), "playback is not an edit");

        let added = q
            .enqueue(&tracks(&[9]), InsertAt::Next, Some(Revision(1)))
            .unwrap();
        assert_eq!(track_order(&q), [1, 9, 2, 3]);
        assert_eq!(q.next_after(Some(created[0])), Some(added[0]));
        assert_eq!(q.revision(), Revision(2));

        // A client that saw revision 1 must not apply its edit blindly.
        let err = q.remove(&[created[2]], Some(Revision(1))).unwrap_err();
        assert_eq!(err.code, ErrorCode::QueueConflict);
        assert_eq!(
            (err.details.expected, err.details.actual),
            (Some(1), Some(2))
        );
        q.remove(&[created[2]], Some(Revision(2))).unwrap();
        assert_eq!(track_order(&q), [1, 9, 2]);
    }

    #[test]
    fn same_track_twice_gets_two_entries() {
        let mut q = Queue::default();
        let created = q.replace(&tracks(&[5, 5]), None);
        assert_ne!(created[0], created[1]);
        assert_eq!(q.next_after(Some(created[0])), Some(created[1]));
    }

    #[test]
    fn repeat_modes() {
        let mut q = Queue::default();
        let e = q.replace(&tracks(&[1, 2]), None);
        assert_eq!(q.next_after(Some(e[1])), None);
        q.set_repeat(RepeatMode::All);
        assert_eq!(q.next_after(Some(e[1])), Some(e[0]));
        assert_eq!(q.previous_before(e[0]), Some(e[1]));
        q.set_repeat(RepeatMode::One);
        assert_eq!(q.next_after(Some(e[1])), Some(e[1]));
        // A skip is not held by repeat one.
        q.set_current(Some(e[0]));
        assert_eq!(q.skip_target(), Some(e[1]));
        q.set_current(Some(e[1]));
        assert_eq!(q.skip_target(), None);
        assert_eq!(q.previous_before(e[1]), Some(e[0]));
    }

    #[test]
    fn shuffle_plays_every_entry_once_starting_with_the_current() {
        let mut q = Queue::default();
        let e = q.replace(&tracks(&(1..=50).collect::<Vec<_>>()), None);
        q.set_current(Some(e[10]));
        q.set_shuffle(true);
        let mut seen = vec![e[10]];
        let mut at = e[10];
        while let Some(next) = q.next_after(Some(at)) {
            seen.push(next);
            at = next;
        }
        let mut sorted = seen.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 50);
        assert_eq!(seen.len(), 50);
        assert_ne!(
            seen,
            e.iter()
                .copied()
                .skip(10)
                .chain(e.iter().copied().take(10))
                .collect::<Vec<_>>()
        );

        // Next while shuffled plays right after the current entry.
        let added = q.enqueue(&tracks(&[99]), InsertAt::Next, None).unwrap();
        assert_eq!(q.next_after(Some(e[10])), Some(added[0]));
        q.set_shuffle(false);
        assert_eq!(
            q.next_after(Some(e[10])),
            Some(added[0]),
            "inserted after the current entry"
        );
    }

    #[test]
    fn undo_only_while_nothing_changed_since() {
        let mut q = Queue::default();
        q.replace(&tracks(&[1, 2]), None);
        let r1 = q.revision();
        q.enqueue(&tracks(&[3]), InsertAt::End, None).unwrap();
        let r2 = q.revision();
        // An agent undoing its own enqueue while the UI changed something meanwhile:
        q.enqueue(&tracks(&[4]), InsertAt::End, None).unwrap();
        assert_eq!(q.undo(r2).unwrap_err().code, ErrorCode::QueueConflict);
        assert_eq!(track_order(&q), [1, 2, 3, 4]);
        // Undoing the latest edit works.
        q.undo(q.revision()).unwrap();
        assert_eq!(track_order(&q), [1, 2, 3]);
        assert!(q.revision() > r2);
        let _ = r1;
    }

    #[test]
    fn moving_and_bounds() {
        let mut q = Queue::default();
        let e = q.replace(&tracks(&[1, 2, 3]), None);
        assert!(q.move_entry(e[0], 2, None).unwrap());
        assert_eq!(track_order(&q), [2, 3, 1]);
        let revision = q.revision();
        assert!(!q.move_entry(e[0], 2, None).unwrap(), "already there");
        assert_eq!(
            q.move_entry(e[0], 3, None).unwrap_err().code,
            ErrorCode::OutOfRange
        );
        assert_eq!(
            q.move_entry(EntryId(999), 0, None).unwrap_err().code,
            ErrorCode::EntryNotFound
        );
        // Removing what is not there is not an error, and not an edit.
        assert_eq!(q.remove(&[EntryId(999)], None).unwrap(), []);
        assert_eq!(q.enqueue(&[], InsertAt::End, None).unwrap(), []);
        assert_eq!(q.revision(), revision);
        assert_eq!(q.remove(&[EntryId(999), e[1]], None).unwrap(), [e[1]]);
        assert_eq!(q.revision(), Revision(revision.0 + 1));
        assert!(q.clear(None).unwrap());
        assert!(!q.clear(None).unwrap());
    }

    #[test]
    fn seeds_and_restore_keep_ids_unique() {
        let mut q = Queue::with_seeds(1_000, 500);
        assert_eq!(q.revision(), Revision(500));
        let e = q.replace(&tracks(&[1, 2]), None);
        assert_eq!(e, [EntryId(1_000), EntryId(1_001)]);
        assert_eq!(q.next_entry_id(), 1_002);
        let mut r = Queue::with_seeds(2_000, 900);
        r.restore(
            &[
                (EntryId(5), TrackId(1)),
                (EntryId(0), TrackId(2)),
                (EntryId(5), TrackId(3)),
            ],
            None,
        );
        let ids: Vec<_> = r.entries().iter().map(|e| e.id).collect();
        assert_eq!(ids, [EntryId(5), EntryId(2_000), EntryId(2_001)]);
        assert_eq!(r.revision(), Revision(900), "restoring is not an edit");
    }
}

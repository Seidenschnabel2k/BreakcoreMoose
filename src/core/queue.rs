use std::collections::VecDeque;

use rand::seq::SliceRandom;
use serde::Serialize;

use crate::media::Track;

/// One queue item. The id stays the same however the entry moves, so the
/// same song queued twice is still two distinct entries.
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub id: u64,
    pub track: Track,
}

/// Where new tracks go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// End of the regular lane.
    End,
    /// End of the priority lane (`!next`).
    Next,
    /// The very front, in order: into the priority lane if it has entries.
    Front,
    /// Right after this entry, in its lane. Falls back to `End` if it's gone.
    After(u64),
}

/// Upcoming tracks in two lanes: `!next` adds to the priority lane, which
/// plays out completely before the regular lane continues.
#[derive(Default)]
pub struct Queue {
    priority: VecDeque<Entry>,
    regular: VecDeque<Entry>,
    next_id: u64,
    /// Bumped on every change, so watchers can skip resending the list.
    version: u64,
}

impl Queue {
    /// Wraps a track in a new entry with a fresh id.
    pub fn entry(&mut self, track: Track) -> Entry {
        self.next_id += 1;
        Entry {
            id: self.next_id,
            track,
        }
    }

    /// Inserts tracks in order. Returns the 1-based position of the first
    /// added track among all upcoming tracks.
    pub fn push(&mut self, tracks: Vec<Track>, placement: Placement) -> usize {
        self.version += 1;
        let entries: Vec<Entry> = tracks.into_iter().map(|t| self.entry(t)).collect();
        let anchor = match placement {
            Placement::After(id) => self.locate(id),
            _ => None,
        };
        match (placement, anchor) {
            (Placement::Next, _) => {
                let position = self.priority.len() + 1;
                self.priority.extend(entries);
                position
            }
            (Placement::Front, _) => {
                let lane = if self.priority.is_empty() {
                    &mut self.regular
                } else {
                    &mut self.priority
                };
                for (k, e) in entries.into_iter().enumerate() {
                    lane.insert(k, e);
                }
                1
            }
            (Placement::After(_), Some((priority, i))) => {
                let lane = if priority {
                    &mut self.priority
                } else {
                    &mut self.regular
                };
                for (k, e) in entries.into_iter().enumerate() {
                    lane.insert(i + 1 + k, e);
                }
                let offset = if priority { 0 } else { self.priority.len() };
                offset + i + 2
            }
            _ => {
                let position = self.len() + 1;
                self.regular.extend(entries);
                position
            }
        }
    }

    /// Puts an entry back at the very front, e.g. one interrupted by a disconnect.
    pub fn push_front(&mut self, entry: Entry) {
        self.version += 1;
        self.priority.push_front(entry);
    }

    /// Appends an entry that already has an id (repeat-queue mode).
    pub fn push_back(&mut self, entry: Entry) {
        self.version += 1;
        self.regular.push_back(entry);
    }

    pub fn pop(&mut self) -> Option<Entry> {
        let entry = self
            .priority
            .pop_front()
            .or_else(|| self.regular.pop_front())?;
        self.version += 1;
        Some(entry)
    }

    /// Takes one entry out of the queue, wherever it is.
    pub fn take(&mut self, id: u64) -> Option<Entry> {
        let (priority, i) = self.locate(id)?;
        self.version += 1;
        if priority {
            self.priority.remove(i)
        } else {
            self.regular.remove(i)
        }
    }

    /// Removes these entries; returns how many were found.
    pub fn remove(&mut self, ids: &[u64]) -> usize {
        let before = self.len();
        self.priority.retain(|e| !ids.contains(&e.id));
        self.regular.retain(|e| !ids.contains(&e.id));
        let removed = before - self.len();
        if removed > 0 {
            self.version += 1;
        }
        removed
    }

    /// Moves `id` to right after `after`, into that entry's lane. `None`
    /// moves it to the very front. False if either entry is gone.
    pub fn move_after(&mut self, id: u64, after: Option<u64>) -> bool {
        if Some(id) == after || after.is_some_and(|a| self.locate(a).is_none()) {
            return false;
        }
        let Some(entry) = self.take(id) else {
            return false;
        };
        match after.and_then(|a| self.locate(a)) {
            Some((true, i)) => self.priority.insert(i + 1, entry),
            Some((false, i)) => self.regular.insert(i + 1, entry),
            None if !self.priority.is_empty() => self.priority.push_front(entry),
            None => self.regular.push_front(entry),
        }
        true
    }

    /// Lane (true = priority) and index of an entry.
    fn locate(&self, id: u64) -> Option<(bool, usize)> {
        if let Some(i) = self.priority.iter().position(|e| e.id == id) {
            return Some((true, i));
        }
        self.regular
            .iter()
            .position(|e| e.id == id)
            .map(|i| (false, i))
    }

    pub fn len(&self) -> usize {
        self.priority.len() + self.regular.len()
    }

    pub fn priority_len(&self) -> usize {
        self.priority.len()
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// All upcoming entries in play order.
    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.priority.iter().chain(&self.regular)
    }

    /// Shuffles the regular lane. The priority lane keeps its order: those
    /// tracks were put up front on purpose. Returns how many were shuffled.
    pub fn shuffle(&mut self) -> usize {
        self.version += 1;
        self.regular.make_contiguous().shuffle(&mut rand::rng());
        self.regular.len()
    }

    /// Empties both lanes; returns how many tracks were removed.
    pub fn clear(&mut self) -> usize {
        let n = self.len();
        self.version += 1;
        self.priority.clear();
        self.regular.clear();
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracks(titles: &[&str]) -> Vec<Track> {
        titles.iter().map(|t| Track::test(t)).collect()
    }

    fn order(q: &Queue) -> Vec<&str> {
        q.iter().map(|e| e.track.title.as_str()).collect()
    }

    fn id_of(q: &Queue, title: &str) -> u64 {
        q.iter().find(|e| e.track.title == title).unwrap().id
    }

    #[test]
    fn priority_plays_first_in_order() {
        let mut q = Queue::default();
        assert_eq!(q.push(tracks(&["r1", "r2"]), Placement::End), 1);
        assert_eq!(q.push(tracks(&["p1"]), Placement::Next), 1);
        assert_eq!(q.push(tracks(&["p2", "p3"]), Placement::Next), 2);
        assert_eq!(q.push(tracks(&["r3"]), Placement::End), 6);
        assert_eq!(order(&q), ["p1", "p2", "p3", "r1", "r2", "r3"]);
        assert_eq!(q.priority_len(), 3);

        let p1 = q.pop().unwrap();
        assert_eq!(p1.track.title, "p1");
        q.push_front(p1);
        assert_eq!(order(&q), ["p1", "p2", "p3", "r1", "r2", "r3"]);
        assert_eq!(q.clear(), 6);
        assert!(q.pop().is_none());
    }

    #[test]
    fn duplicates_are_separate_entries() {
        let mut q = Queue::default();
        q.push(tracks(&["same", "same"]), Placement::End);
        let ids: Vec<u64> = q.iter().map(|e| e.id).collect();
        assert_ne!(ids[0], ids[1]);
        assert_eq!(q.remove(&[ids[1]]), 1);
        assert_eq!(q.iter().next().unwrap().id, ids[0]);
    }

    #[test]
    fn moves_by_id() {
        let mut q = Queue::default();
        q.push(tracks(&["a", "b", "c", "d"]), Placement::End);
        let (a, c, d) = (id_of(&q, "a"), id_of(&q, "c"), id_of(&q, "d"));

        assert!(q.move_after(a, Some(c)));
        assert_eq!(order(&q), ["b", "c", "a", "d"]);
        assert!(q.move_after(d, None));
        assert_eq!(order(&q), ["d", "b", "c", "a"]);
        assert!(!q.move_after(a, Some(a)));
        assert!(!q.move_after(a, Some(999)));
        assert!(!q.move_after(999, None));
        assert_eq!(order(&q), ["d", "b", "c", "a"]);

        // Moving behind a priority entry joins the priority lane.
        q.push(tracks(&["p"]), Placement::Next);
        assert!(q.move_after(a, Some(id_of(&q, "p"))));
        assert_eq!(order(&q), ["p", "a", "d", "b", "c"]);
        assert_eq!(q.priority_len(), 2);
        // To the top while there's a priority lane: becomes priority.
        assert!(q.move_after(c, None));
        assert_eq!(order(&q), ["c", "p", "a", "d", "b"]);
    }

    #[test]
    fn inserts_after_an_entry() {
        let mut q = Queue::default();
        q.push(tracks(&["a", "b"]), Placement::End);
        q.push(tracks(&["p"]), Placement::Next);
        let a = id_of(&q, "a");
        assert_eq!(q.push(tracks(&["x", "y"]), Placement::After(a)), 3);
        assert_eq!(order(&q), ["p", "a", "x", "y", "b"]);
        let p = id_of(&q, "p");
        assert_eq!(q.push(tracks(&["z"]), Placement::After(p)), 2);
        assert_eq!(q.priority_len(), 2);
        assert_eq!(q.push(tracks(&["end"]), Placement::After(999)), 7);
        assert_eq!(q.push(tracks(&["f1", "f2"]), Placement::Front), 1);
        assert_eq!(order(&q)[..3], ["f1", "f2", "p"]);
        assert_eq!(q.priority_len(), 4);
        let v = q.version();
        assert!(q.take(a).is_some());
        assert!(q.version() > v);
    }

    #[test]
    fn shuffle_keeps_priority_lane() {
        let mut q = Queue::default();
        let regular: Vec<String> = (0..50).map(|i| format!("r{i}")).collect();
        q.push(
            tracks(&regular.iter().map(String::as_str).collect::<Vec<_>>()),
            Placement::End,
        );
        q.push(tracks(&["p1", "p2"]), Placement::Next);

        assert_eq!(q.shuffle(), 50);
        let order = order(&q);
        assert_eq!(order[..2], ["p1", "p2"]);
        let mut rest: Vec<&str> = order[2..].to_vec();
        assert_ne!(rest, regular, "50 tracks came out in the original order");
        rest.sort();
        let mut expected: Vec<&str> = regular.iter().map(String::as_str).collect();
        expected.sort();
        assert_eq!(rest, expected);
    }
}

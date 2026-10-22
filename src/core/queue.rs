use std::collections::VecDeque;

use rand::seq::SliceRandom;

use crate::media::Track;

/// Upcoming tracks in two lanes: `!next` adds to the priority lane, which
/// plays out completely before the regular lane continues.
#[derive(Default)]
pub struct Queue {
    priority: VecDeque<Track>,
    regular: VecDeque<Track>,
}

impl Queue {
    /// Appends to the end of a lane. Returns the 1-based position of the
    /// first added track among all upcoming tracks.
    pub fn push(&mut self, tracks: Vec<Track>, priority: bool) -> usize {
        if priority {
            let position = self.priority.len() + 1;
            self.priority.extend(tracks);
            position
        } else {
            let position = self.len() + 1;
            self.regular.extend(tracks);
            position
        }
    }

    /// Puts a track back at the very front, e.g. one interrupted by a disconnect.
    pub fn push_front(&mut self, track: Track) {
        self.priority.push_front(track);
    }

    pub fn pop(&mut self) -> Option<Track> {
        self.priority.pop_front().or_else(|| self.regular.pop_front())
    }

    pub fn len(&self) -> usize {
        self.priority.len() + self.regular.len()
    }

    pub fn priority_len(&self) -> usize {
        self.priority.len()
    }

    /// All upcoming tracks in play order.
    pub fn iter(&self) -> impl Iterator<Item = &Track> {
        self.priority.iter().chain(&self.regular)
    }

    /// Shuffles the regular lane. The priority lane keeps its order: those
    /// tracks were put up front on purpose. Returns how many were shuffled.
    pub fn shuffle(&mut self) -> usize {
        self.regular.make_contiguous().shuffle(&mut rand::rng());
        self.regular.len()
    }

    /// Empties both lanes; returns how many tracks were removed.
    pub fn clear(&mut self) -> usize {
        let n = self.len();
        self.priority.clear();
        self.regular.clear();
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracks(titles: &[&str]) -> Vec<Track> {
        titles
            .iter()
            .map(|t| Track {
                title: t.to_string(),
                url: String::new(),
                duration_secs: None,
                thumbnail: None,
                requested_by: String::new(),
            })
            .collect()
    }

    fn order(q: &Queue) -> Vec<&str> {
        q.iter().map(|t| t.title.as_str()).collect()
    }

    #[test]
    fn priority_plays_first_in_order() {
        let mut q = Queue::default();
        assert_eq!(q.push(tracks(&["r1", "r2"]), false), 1);
        assert_eq!(q.push(tracks(&["p1"]), true), 1);
        assert_eq!(q.push(tracks(&["p2", "p3"]), true), 2);
        assert_eq!(q.push(tracks(&["r3"]), false), 6);
        assert_eq!(order(&q), ["p1", "p2", "p3", "r1", "r2", "r3"]);
        assert_eq!(q.priority_len(), 3);

        assert_eq!(q.pop().unwrap().title, "p1");
        q.push_front(tracks(&["resume"]).remove(0));
        assert_eq!(order(&q), ["resume", "p2", "p3", "r1", "r2", "r3"]);
        assert_eq!(q.clear(), 6);
        assert!(q.pop().is_none());
    }

    #[test]
    fn shuffle_keeps_priority_lane() {
        let mut q = Queue::default();
        let regular: Vec<String> = (0..50).map(|i| format!("r{i}")).collect();
        q.push(tracks(&regular.iter().map(String::as_str).collect::<Vec<_>>()), false);
        q.push(tracks(&["p1", "p2"]), true);

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

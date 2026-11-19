//! Play queue: a context (playlist, album, search results…) in play order, plus tracks the user
//! queued by hand, which play before the context continues. Pure logic, no I/O.

use std::collections::VecDeque;

use rand::Rng;
use rand::seq::SliceRandom;

use crate::RepeatMode;

/// A track as the queue panel shows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QueueItem {
    /// `spotify:track:…` or `spotify:episode:…`
    pub uri: String,
    pub title: String,
    pub artists: String,
    pub duration_ms: u32,
}

/// What the UI shows about the queue.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QueueSnapshot {
    /// "Ночные маршруты", "Поиск: ночь"…
    pub context_name: String,
    /// The hand-queued tracks first, then the rest of the context.
    pub upcoming: Vec<QueueItem>,
    /// How many of `upcoming` were queued by hand.
    pub manual: usize,
    pub shuffle: bool,
    pub repeat: RepeatMode,
}

#[derive(Debug, Default)]
pub struct Queue {
    context: Vec<QueueItem>,
    context_name: String,
    /// Play order: indices into `context` (identity, or shuffled).
    order: Vec<usize>,
    /// Position in `order` of the current (or last) context track.
    pos: Option<usize>,
    manual: VecDeque<QueueItem>,
    /// Set while a hand-queued track plays; the context position stays where it was.
    playing_manual: Option<QueueItem>,
    shuffle: bool,
    repeat: RepeatMode,
}

impl Queue {
    /// Starts a new context at `start`. Hand-queued tracks are kept.
    pub fn set_context(&mut self, items: Vec<QueueItem>, start: usize, name: String) {
        self.set_context_with(items, start, name, &mut rand::rng());
    }

    fn set_context_with(
        &mut self,
        items: Vec<QueueItem>,
        start: usize,
        name: String,
        rng: &mut impl Rng,
    ) {
        let start = start.min(items.len().saturating_sub(1));
        self.context = items;
        self.context_name = name;
        self.playing_manual = None;
        self.order = (0..self.context.len()).collect();
        self.pos = (!self.context.is_empty()).then_some(start);
        if self.shuffle {
            self.reshuffle(rng);
        }
    }

    /// Shuffled order keeps the current track first, so turning shuffle on never skips it.
    fn reshuffle(&mut self, rng: &mut impl Rng) {
        let current = self.pos.map(|p| self.order[p]);
        let mut rest: Vec<usize> = (0..self.context.len())
            .filter(|i| Some(*i) != current)
            .collect();
        rest.shuffle(rng);
        self.order = current.into_iter().chain(rest).collect();
        self.pos = current.map(|_| 0);
    }

    #[must_use]
    pub fn current(&self) -> Option<&QueueItem> {
        self.playing_manual
            .as_ref()
            .or_else(|| self.context.get(self.order[self.pos?]))
    }

    /// Where `next` would go, without moving. `auto` is the end of a track (repeat-one applies).
    #[must_use]
    pub fn peek_next(&self, auto: bool) -> Option<&QueueItem> {
        if auto && self.repeat == RepeatMode::One {
            return self.current();
        }
        if let Some(m) = self.manual.front() {
            return Some(m);
        }
        self.next_pos().map(|p| &self.context[self.order[p]])
    }

    fn next_pos(&self) -> Option<usize> {
        let len = self.order.len();
        match self.pos {
            None if len > 0 => Some(0),
            None => None,
            Some(p) if p + 1 < len => Some(p + 1),
            Some(_) if self.repeat != RepeatMode::Off && len > 0 => Some(0),
            Some(_) => None,
        }
    }

    /// Moves to the next track. `None` at the end of the queue.
    pub fn next(&mut self, auto: bool) -> Option<QueueItem> {
        if auto && self.repeat == RepeatMode::One {
            return self.current().cloned();
        }
        if let Some(m) = self.manual.pop_front() {
            self.playing_manual = Some(m.clone());
            return Some(m);
        }
        self.playing_manual = None;
        let p = self.next_pos()?;
        self.pos = Some(p);
        self.current().cloned()
    }

    /// Moves to the previous context track (from a hand-queued one: back to the context track).
    pub fn prev(&mut self) -> Option<QueueItem> {
        if self.playing_manual.take().is_some() {
            return self.current().cloned();
        }
        let len = self.order.len();
        let p = match self.pos? {
            0 if self.repeat != RepeatMode::Off => len.checked_sub(1)?,
            0 => 0,
            p => p - 1,
        };
        self.pos = Some(p);
        self.current().cloned()
    }

    /// Plays the `index`-th entry of [`QueueSnapshot::upcoming`]: skips everything before it.
    pub fn jump(&mut self, index: usize) -> Option<QueueItem> {
        if index < self.manual.len() {
            self.manual.drain(..index);
            return self.next(false);
        }
        let skip = index - self.manual.len();
        self.manual.clear();
        for _ in 0..skip {
            self.next(false)?;
        }
        self.next(false)
    }

    pub fn add(&mut self, item: QueueItem) {
        self.manual.push_back(item);
    }

    /// Moves to `uri` in the context if it is there.
    pub fn select(&mut self, uri: &str) -> bool {
        let Some(p) = self.order.iter().position(|&i| self.context[i].uri == uri) else {
            return false;
        };
        self.playing_manual = None;
        self.pos = Some(p);
        true
    }

    /// Drag and drop in the queue panel: moves entry `from` of [`QueueSnapshot::upcoming`] to
    /// `to`. Hand-queued tracks and the context are reordered each within its own part; a drop
    /// across the border lands at the border.
    pub fn move_upcoming(&mut self, from: usize, to: usize) {
        let m = self.manual.len();
        if from < m {
            let to = to.min(m - 1);
            if let Some(item) = self.manual.remove(from) {
                self.manual.insert(to, item);
            }
            return;
        }
        let start = self.pos.map_or(0, |p| p + 1);
        let (from, to) = (start + from - m, start + to.max(m) - m);
        if from >= self.order.len() {
            return;
        }
        let to = to.min(self.order.len() - 1);
        let entry = self.order.remove(from);
        self.order.insert(to, entry);
    }

    pub fn clear_manual(&mut self) {
        self.manual.clear();
    }

    pub fn set_shuffle(&mut self, on: bool) {
        self.set_shuffle_with(on, &mut rand::rng());
    }

    fn set_shuffle_with(&mut self, on: bool, rng: &mut impl Rng) {
        if on == self.shuffle {
            return;
        }
        self.shuffle = on;
        if on {
            self.reshuffle(rng);
        } else {
            let current = self.pos.map(|p| self.order[p]);
            self.order = (0..self.context.len()).collect();
            self.pos = current;
        }
    }

    #[must_use]
    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    pub fn set_repeat(&mut self, repeat: RepeatMode) {
        self.repeat = repeat;
    }

    #[must_use]
    pub fn repeat(&self) -> RepeatMode {
        self.repeat
    }

    #[must_use]
    pub fn snapshot(&self, limit: usize) -> QueueSnapshot {
        let mut upcoming: Vec<QueueItem> = self.manual.iter().take(limit).cloned().collect();
        let manual = upcoming.len();
        let start = self.pos.map_or(0, |p| p + 1);
        upcoming.extend(
            self.order
                .iter()
                .skip(start)
                .take(limit.saturating_sub(manual))
                .map(|&i| self.context[i].clone()),
        );
        QueueSnapshot {
            context_name: self.context_name.clone(),
            upcoming,
            manual,
            shuffle: self.shuffle,
            repeat: self.repeat,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    #[test]
    fn move_reorders_within_each_part() {
        let mut q = Queue::default();
        q.set_context(items(5), 0, String::new());
        q.set_shuffle(false);
        q.add(QueueItem {
            uri: "m1".into(),
            ..QueueItem::default()
        });
        q.add(QueueItem {
            uri: "m2".into(),
            ..QueueItem::default()
        });
        let uris = |q: &Queue| -> Vec<String> {
            q.snapshot(10).upcoming.into_iter().map(|i| i.uri).collect()
        };
        q.move_upcoming(1, 0);
        assert_eq!(uris(&q)[..2], ["m2".to_owned(), "m1".to_owned()]);
        // context: 1 2 3 4 after the current 0; move "4" to the front of the context
        q.move_upcoming(5, 0);
        assert_eq!(
            uris(&q),
            [
                "m2",
                "m1",
                "spotify:track:4",
                "spotify:track:1",
                "spotify:track:2",
                "spotify:track:3"
            ]
        );
        q.move_upcoming(99, 99); // out of range: nothing happens
        assert_eq!(uris(&q).len(), 6);
    }

    fn items(n: usize) -> Vec<QueueItem> {
        (0..n)
            .map(|i| QueueItem {
                uri: format!("spotify:track:{i}"),
                ..QueueItem::default()
            })
            .collect()
    }

    fn uri(i: Option<QueueItem>) -> String {
        i.map(|i| i.uri.trim_start_matches("spotify:track:").to_owned())
            .unwrap_or_else(|| "-".into())
    }

    #[test]
    fn plays_through_and_stops_or_repeats() {
        let mut q = Queue::default();
        q.set_context(items(3), 1, "P".into());
        assert_eq!(uri(q.current().cloned()), "1");
        assert_eq!(uri(q.next(true)), "2");
        assert_eq!(uri(q.next(true)), "-");
        q.set_repeat(RepeatMode::All);
        assert_eq!(uri(q.next(true)), "0");
        assert_eq!(uri(q.prev()), "2");
        q.set_repeat(RepeatMode::One);
        assert_eq!(uri(q.next(true)), "2");
        assert_eq!(uri(q.next(false)), "0", "manual skip ignores repeat-one");
    }

    #[test]
    fn hand_queued_tracks_play_first_and_keep_context_position() {
        let mut q = Queue::default();
        q.set_context(items(3), 0, "P".into());
        q.add(QueueItem {
            uri: "spotify:track:x".into(),
            ..QueueItem::default()
        });
        let snap = q.snapshot(10);
        assert_eq!(snap.manual, 1);
        assert_eq!(snap.upcoming.len(), 3);
        assert_eq!(uri(q.peek_next(true).cloned()), "x");
        assert_eq!(uri(q.next(true)), "x");
        assert_eq!(uri(q.next(true)), "1");
        q.add(QueueItem {
            uri: "spotify:track:y".into(),
            ..QueueItem::default()
        });
        assert_eq!(uri(q.next(false)), "y");
        assert_eq!(uri(q.prev()), "1");
    }

    #[test]
    fn shuffle_keeps_current_and_visits_everything_once() {
        let mut rng = StdRng::seed_from_u64(7);
        let mut q = Queue::default();
        q.set_context(items(20), 5, "P".into());
        q.set_shuffle_with(true, &mut rng);
        assert_eq!(uri(q.current().cloned()), "5");
        let mut seen = vec![uri(q.current().cloned())];
        while let Some(t) = q.next(true) {
            seen.push(uri(Some(t)));
        }
        let mut sorted: Vec<usize> = seen.iter().map(|s| s.parse().unwrap()).collect();
        assert_ne!(sorted, (0..20).collect::<Vec<_>>(), "order is shuffled");
        sorted.sort_unstable();
        assert_eq!(sorted, (0..20).collect::<Vec<_>>());
        // Off again: continue in album order after the current track.
        q.set_shuffle_with(false, &mut rng);
        let cur: usize = uri(q.current().cloned()).parse().unwrap();
        assert_eq!(q.snapshot(50).upcoming.len(), 19 - cur);
    }

    #[test]
    fn select_moves_within_context() {
        let mut q = Queue::default();
        q.set_context(items(4), 0, "P".into());
        assert!(q.select("spotify:track:2"));
        assert_eq!(uri(q.next(true)), "3");
        assert!(!q.select("spotify:track:9"));
    }

    #[test]
    fn jump_skips_ahead() {
        let mut q = Queue::default();
        q.set_context(items(5), 0, "P".into());
        q.add(QueueItem {
            uri: "spotify:track:x".into(),
            ..QueueItem::default()
        });
        // upcoming: x, 1, 2, 3, 4 → index 3 is track 3
        assert_eq!(uri(q.jump(3)), "3");
        assert_eq!(q.snapshot(10).manual, 0);
        assert_eq!(uri(q.next(true)), "4");
    }
}

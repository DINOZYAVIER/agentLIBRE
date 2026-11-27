//! The live speed of a generation (AGL-012; AGL-139, D6): the pieces of
//! its last second, as the REPL's dock and the chat's row count them.

use std::collections::VecDeque;
use std::time::Instant;

/// The times of the recent pieces of a generation, within the last second.
#[derive(Clone, Debug, Default)]
pub struct Window {
    pieces: VecDeque<Instant>,
}

impl Window {
    /// A piece arrived at `now`: the window keeps it, and of the others the
    /// pieces of the last second.
    pub fn piece(&mut self, now: Instant) {
        self.pieces.push_back(now);
        while self.pieces.len() > 1
            && self.pieces.front().is_some_and(|first| now.duration_since(*first).as_secs_f64() > 1.0)
        {
            self.pieces.pop_front();
        }
    }

    /// Pieces per second: the spans between the pieces over their time,
    /// none while it holds fewer than two pieces or no time passed between
    /// them.
    pub fn speed(&self) -> Option<f64> {
        let (first, last) = (self.pieces.front()?, self.pieces.back()?);
        if self.pieces.len() < 2 {
            return None;
        }
        let span = last.duration_since(*first).as_secs_f64();
        (span > 0.0).then(|| (self.pieces.len() - 1) as f64 / span)
    }

    /// The generation ended: its speed goes with it.
    pub fn clear(&mut self) {
        self.pieces.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_live_speed_needs_two_pieces_apart() {
        let t0 = Instant::now();
        let mut window = Window::default();
        assert_eq!(window.speed(), None);
        window.piece(t0);
        assert_eq!(window.speed(), None);
        window.piece(t0 + Duration::from_millis(500));
        // One span of half a second: two a second.
        assert!((window.speed().unwrap() - 2.0).abs() < 0.01);
        window.clear();
        assert_eq!(window.speed(), None);
    }

    #[test]
    fn the_window_keeps_the_pieces_of_the_last_second() {
        let t0 = Instant::now();
        let mut window = Window::default();
        for ms in [0, 400, 1200] {
            window.piece(t0 + Duration::from_millis(ms));
        }
        // The first is gone: one span over 0.8 s.
        assert!((window.speed().unwrap() - 1.25).abs() < 0.01);
    }
}

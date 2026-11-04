//! What the dock says while the server reads a prompt (AGL-046, D19): the
//! server's figures when they are there, and between them an estimate that
//! keeps moving. An estimate is marked with `~` and goes nowhere else: the
//! rows and the session file hold the server's figures only.

use std::time::{Duration, Instant};

use super::dock::grouped;
use crate::backend::PromptProgress;
use crate::i18n::{Lang, repl as texts};

/// For this long after a figure of the server, the dock shows it as it is.
const FRESH: Duration = Duration::from_millis(300);

/// The fewest tokens read that make a speed: a read of a few tokens after a
/// cached prefix is mostly the server's fixed cost (18 tokens in 304 ms, on
/// a server that reads 700 tokens a second).
pub const MIN_TOKENS_OF_A_SPEED: usize = 64;

/// The reading of one prompt.
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    started: Instant,
    /// The real speed of an earlier reading of the session.
    earlier_speed: Option<f64>,
    /// The tokens to read: the prompt without its cached part.
    to_read: Option<usize>,
    /// The last figures of the server and when they came.
    read: usize,
    speed: Option<f64>,
    anchor: Instant,
}

impl Reading {
    pub fn new(now: Instant, earlier_speed: Option<f64>) -> Reading {
        Reading {
            started: now,
            earlier_speed,
            to_read: None,
            read: 0,
            speed: None,
            anchor: now,
        }
    }

    /// Takes a figure of the server. A cached part does not count: the timed
    /// progress is `(processed - cache) / (total - cache)`.
    pub fn progress(&mut self, progress: PromptProgress, now: Instant) {
        let read = progress.processed.saturating_sub(progress.cache);
        self.to_read = Some(progress.total.saturating_sub(progress.cache));
        self.read = read;
        self.anchor = now;
        if read >= MIN_TOKENS_OF_A_SPEED && progress.time_ms > 0 {
            self.speed = Some(read as f64 * 1000.0 / progress.time_ms as f64);
        }
    }

    /// What follows the name of the phase in the dock at `now`.
    pub fn label(&self, now: Instant, lang: Lang) -> String {
        let mut parts = Vec::new();
        if let Some(to_read) = self.to_read.filter(|n| *n > 0) {
            let since = now.saturating_duration_since(self.anchor);
            let moving = self.speed.or(self.earlier_speed);
            let estimate = moving.filter(|_| self.speed.is_none() || since >= FRESH);
            parts.push(match estimate {
                // It stays below the end until the server says it is there.
                Some(speed) => {
                    let grown = self.read + (speed * since.as_secs_f64()) as usize;
                    let shown = grown.min(to_read.saturating_sub(1)).max(self.read);
                    format!("~{} / {}", grouped(shown), grouped(to_read))
                }
                None => format!("{} / {}", grouped(self.read), grouped(to_read)),
            });
            match (self.speed, self.earlier_speed) {
                (Some(speed), _) => parts.push(texts::speed(speed, lang)),
                (None, Some(speed)) => parts.push(format!("~{}", texts::speed(speed, lang))),
                (None, None) => {}
            }
        }
        let elapsed = now.saturating_duration_since(self.started);
        parts.push(if elapsed.as_secs() < 60 {
            format!("{:.1}s", elapsed.as_secs_f64())
        } else {
            texts::duration(elapsed.as_secs())
        });
        parts.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NBSP: char = '\u{a0}';

    fn progress(total: usize, cache: usize, processed: usize, time_ms: u64) -> PromptProgress {
        PromptProgress {
            total,
            cache,
            processed,
            time_ms,
        }
    }

    fn at(label: String) -> String {
        label.replace(NBSP, " ")
    }

    #[test]
    fn before_the_server_says_anything_only_the_time_moves() {
        let t0 = Instant::now();
        let reading = Reading::new(t0, Some(700.0));
        assert_eq!(
            reading.label(t0 + Duration::from_millis(400), Lang::En),
            "0.4s"
        );
    }

    #[test]
    fn without_a_speed_the_count_stays_and_the_time_moves() {
        let t0 = Instant::now();
        let mut reading = Reading::new(t0, None);
        reading.progress(progress(50_000, 49_000, 49_000, 0), t0);
        assert_eq!(
            at(reading.label(t0 + Duration::from_millis(400), Lang::En)),
            "0 / 1 000 · 0.4s"
        );
    }

    #[test]
    fn an_earlier_speed_gives_an_estimate_marked_as_one() {
        let t0 = Instant::now();
        let mut reading = Reading::new(t0, Some(700.0));
        reading.progress(progress(1000, 0, 0, 0), t0);
        assert_eq!(
            at(reading.label(t0 + Duration::from_millis(400), Lang::En)),
            "~280 / 1 000 · ~700.0 tok/s · 0.4s"
        );
    }

    #[test]
    fn a_figure_of_the_server_replaces_the_estimate_and_then_it_moves_on() {
        let t0 = Instant::now();
        let mut reading = Reading::new(t0, Some(100.0));
        let now = t0 + Duration::from_millis(500);
        // The cached part counts for neither the count nor the speed.
        reading.progress(progress(50_000, 49_000, 49_500, 500), now);
        assert_eq!(
            at(reading.label(now, Lang::En)),
            "500 / 1 000 · 1000.0 tok/s · 0.5s"
        );
        assert_eq!(
            at(reading.label(now + Duration::from_millis(400), Lang::En)),
            "~900 / 1 000 · 1000.0 tok/s · 0.9s"
        );
    }

    #[test]
    fn a_short_read_makes_no_speed_and_the_earlier_one_stays() {
        let t0 = Instant::now();
        let mut reading = Reading::new(t0, Some(700.0));
        reading.progress(progress(1591, 1573, 1587, 191), t0);
        let label = at(reading.label(t0, Lang::En));
        assert_eq!(label, "~14 / 18 · ~700.0 tok/s · 0.0s");
    }

    #[test]
    fn the_estimate_stays_below_the_end_and_never_goes_back() {
        let t0 = Instant::now();
        let mut reading = Reading::new(t0, None);
        reading.progress(progress(1000, 0, 500, 500), t0);
        let label = at(reading.label(t0 + Duration::from_secs(50), Lang::En));
        assert!(label.starts_with("~999 / 1 000"), "{label}");
    }

    #[test]
    fn a_prompt_that_is_all_cached_shows_the_time() {
        let t0 = Instant::now();
        let mut reading = Reading::new(t0, Some(700.0));
        reading.progress(progress(1000, 1000, 1000, 0), t0);
        assert_eq!(reading.label(t0, Lang::En), "0.0s");
    }

    #[test]
    fn a_long_reading_shows_minutes() {
        let t0 = Instant::now();
        let reading = Reading::new(t0, None);
        assert_eq!(
            reading.label(t0 + Duration::from_secs(75), Lang::En),
            "1m 15s"
        );
    }
}

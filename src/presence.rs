//! The bird-presence decision, and the settled weight taken during a visit.
//!
//! This used to sit inline in `main`, which meant the one piece of arithmetic
//! that decides whether a visit gets recorded *at all* was the part of the
//! firmware that could not be tested on the host. It needs nothing but one
//! reading and the persisted baseline, so it belongs here instead (see the
//! crate docs on why pure computation lives in the library half).
//!
//! ## What a visit looks like from here
//!
//! The load cell is polled out of deep sleep, so the firmware sees a visit as a
//! sequence of isolated readings rather than a curve. [`decide`] turns one such
//! reading into an edge, and `main` reacts:
//!
//! * [`Decision::Arrived`] — stay awake and watch the visit through with a
//!   [`Window`], so the published weight is a settled median rather than
//!   whatever single conversion happened to land first.
//! * [`Decision::Departed`] — the visit ended; publish and resume idle polling.
//! * [`Decision::Quiet`] — nothing there; absorb the reading as creep.
//! * [`Decision::Unexplained`] — something is on the scale that is neither.
//!   See [`drift_band`] for why this case exists at all.

use core::fmt::Write;

/// Exponential-decay shift for empty-house baseline drift tracking: each quiet
/// cycle nudges the baseline by `delta >> BASELINE_DRIFT_SHIFT`, which absorbs
/// slow thermal and mechanical creep without chasing a real load.
pub const BASELINE_DRIFT_SHIFT: u32 = 4;

/// How much narrower than the presence threshold the drift band is.
const DRIFT_BAND_DIVISOR: i32 = 4;

/// Half-width of the band around the baseline inside which a reading counts as
/// creep.
///
/// This band is the whole point of [`Decision::Unexplained`]. The drift used to
/// run on *any* sub-threshold reading, which quietly ate light visitors: a bird
/// landing with a load below the presence threshold — a small species, or one
/// perched half on the rim — was pulled into the baseline at `delta/16` per
/// cycle, so within roughly 16 cycles the scale read "empty" again. Worse, when
/// it left, the delta went *negative*, which is also sub-threshold, so no
/// departure edge fired either and the baseline crept back. The visit left no
/// trace anywhere.
///
/// Creep is slow and small; a bird is a step. Only the small case may move the
/// baseline, and anything in between is reported rather than absorbed.
pub fn drift_band(threshold_ticks: i32) -> i32 {
    (threshold_ticks / DRIFT_BAND_DIVISOR).max(1)
}

/// What one load-cell reading means for the presence state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The load just crossed the presence threshold: a visit begins.
    Arrived { delta: i32 },
    /// Above the threshold, and the previous cycle already knew. Reached when a
    /// load outlasts the awake window `main` gives one visit — a bird that
    /// settles in, or snow.
    Staying { delta: i32 },
    /// Below the threshold while flagged present: the visit ended.
    Departed { delta: i32 },
    /// Empty and quiet. `baseline` is the drifted value to store.
    Quiet { baseline: i32 },
    /// Empty of birds, but carrying a load too large to be creep and too small
    /// to be a visit. The baseline is deliberately left where it is.
    Unexplained { delta: i32 },
}

/// Classify one reading against the persisted baseline.
///
/// `threshold_ticks` comes from [`crate::config::Config::threshold_ticks`], so
/// the gram threshold Home Assistant sets is what decides a visit.
pub fn decide(raw: i32, baseline: i32, was_present: bool, threshold_ticks: i32) -> Decision {
    // Saturating rather than plain arithmetic: the baseline is read back from
    // RTC RAM, and a corrupted word must not panic the presence decision.
    let delta = raw.saturating_sub(baseline);

    if delta >= threshold_ticks {
        if was_present {
            Decision::Staying { delta }
        } else {
            Decision::Arrived { delta }
        }
    } else if was_present {
        Decision::Departed { delta }
    } else if delta.saturating_abs() <= drift_band(threshold_ticks) {
        Decision::Quiet {
            baseline: baseline.saturating_add(delta >> BASELINE_DRIFT_SHIFT),
        }
    } else {
        Decision::Unexplained { delta }
    }
}

/// Number of most-recent readings the settled weight is taken over.
pub const WINDOW: usize = 32;

/// A fixed ring of the most recent readings, reduced to a median.
///
/// Most-recent rather than first-N on purpose: a bird that has just landed is
/// still moving and the cell is still ringing, so the *earliest* samples of a
/// visit are the least trustworthy ones. Keeping the tail means the published
/// weight describes the bird standing still, and for a long visit it describes
/// the moment just before it left.
///
/// The median, not the mean, because a bird hopping once puts a single wild
/// sample in the window and the mean would carry it into the published value.
pub struct Window {
    buf: [i32; WINDOW],
    len: usize,
    next: usize,
}

impl Default for Window {
    fn default() -> Self {
        Self::new()
    }
}

impl Window {
    pub const fn new() -> Self {
        Self {
            buf: [0; WINDOW],
            len: 0,
            next: 0,
        }
    }

    /// Add a reading, evicting the oldest once the ring is full.
    pub fn push(&mut self, raw: i32) {
        self.buf[self.next] = raw;
        self.next = (self.next + 1) % WINDOW;
        if self.len < WINDOW {
            self.len += 1;
        }
    }

    /// How many readings the window holds.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The median of the held readings, or `None` if none were pushed. An even
    /// count takes the midpoint of the two central samples.
    pub fn median(&self) -> Option<i32> {
        if self.len == 0 {
            return None;
        }
        // Order within the ring is irrelevant to a median, so the live slice can
        // be copied out and sorted regardless of where `next` currently points.
        let mut sorted = [0i32; WINDOW];
        sorted[..self.len].copy_from_slice(&self.buf[..self.len]);
        let sorted = &mut sorted[..self.len];
        sorted.sort_unstable();

        let mid = self.len / 2;
        Some(if self.len % 2 == 1 {
            sorted[mid]
        } else {
            // Midpoint written as a difference so two large samples cannot
            // overflow on the way to their average.
            sorted[mid - 1] + (sorted[mid] - sorted[mid - 1]) / 2
        })
    }
}

/// Format a duration in milliseconds as seconds with one decimal, in the same
/// float-free style as the other published values.
pub fn write_secs(buf: &mut heapless::String<16>, millis: u64) {
    let tenths = (millis + 50) / 100;
    let _ = write!(buf, "{}.{}", tenths / 10, tenths % 10);
}

#[cfg(test)]
mod tests {
    use super::*;

    const THRESHOLD: i32 = 4200; // 10 g at the default scale factor

    #[test]
    fn crossing_the_threshold_from_empty_is_an_arrival() {
        assert_eq!(
            decide(1000 + THRESHOLD, 1000, false, THRESHOLD),
            Decision::Arrived { delta: THRESHOLD }
        );
    }

    #[test]
    fn the_same_load_is_only_an_arrival_once() {
        let raw = 1000 + THRESHOLD;
        assert!(matches!(
            decide(raw, 1000, false, THRESHOLD),
            Decision::Arrived { .. }
        ));
        assert!(matches!(
            decide(raw, 1000, true, THRESHOLD),
            Decision::Staying { .. }
        ));
    }

    #[test]
    fn losing_the_load_while_present_is_a_departure() {
        assert!(matches!(
            decide(1000, 1000, true, THRESHOLD),
            Decision::Departed { .. }
        ));
    }

    #[test]
    fn a_reading_inside_the_band_drifts_the_baseline_toward_it() {
        let band = drift_band(THRESHOLD);
        match decide(1000 + band, 1000, false, THRESHOLD) {
            Decision::Quiet { baseline } => {
                assert_eq!(baseline, 1000 + (band >> BASELINE_DRIFT_SHIFT));
                assert!(baseline > 1000, "drift must move toward the reading");
            }
            other => panic!("expected Quiet, got {other:?}"),
        }
    }

    #[test]
    fn drift_also_works_downward() {
        match decide(1000 - drift_band(THRESHOLD), 1000, false, THRESHOLD) {
            Decision::Quiet { baseline } => assert!(baseline < 1000),
            other => panic!("expected Quiet, got {other:?}"),
        }
    }

    #[test]
    fn a_load_between_the_band_and_the_threshold_is_unexplained() {
        // Just outside the creep band, and just short of a visit: the case the
        // old code silently absorbed.
        for delta in [drift_band(THRESHOLD) + 1, THRESHOLD - 1] {
            assert_eq!(
                decide(1000 + delta, 1000, false, THRESHOLD),
                Decision::Unexplained { delta },
                "delta {delta} must not be treated as creep"
            );
        }
    }

    #[test]
    fn a_light_visitor_is_never_absorbed_into_the_baseline() {
        // The regression this guards is a *silent* one: under the old
        // unconditional drift, a steady sub-threshold load was pulled into the
        // baseline within ~16 cycles and the visit vanished without a trace,
        // in the logs or on the broker.
        let mut baseline = 1000;
        let raw = baseline + THRESHOLD - 1; // a bird just under the threshold

        for _ in 0..100 {
            match decide(raw, baseline, false, THRESHOLD) {
                Decision::Quiet { baseline: drifted } => baseline = drifted,
                Decision::Unexplained { .. } => {}
                other => panic!("unexpected {other:?}"),
            }
        }

        assert_eq!(
            baseline, 1000,
            "a sub-threshold load must leave the baseline untouched"
        );
    }

    #[test]
    fn the_drift_band_never_collapses_to_zero() {
        // A tiny threshold must not make every reading "creep" by making the
        // band round down to nothing.
        for threshold in [1, 2, 3, DRIFT_BAND_DIVISOR] {
            assert!(drift_band(threshold) >= 1);
        }
    }

    #[test]
    fn the_band_stays_below_the_threshold() {
        for threshold in [100, 4200, 100_000] {
            assert!(drift_band(threshold) < threshold);
        }
    }

    #[test]
    fn a_corrupt_baseline_cannot_panic_the_decision() {
        // RTC RAM is not checksummed, so `decide` must survive whatever it
        // reads back rather than overflowing on the subtraction.
        let _ = decide(i32::MAX, i32::MIN, false, THRESHOLD);
        let _ = decide(i32::MIN, i32::MAX, true, THRESHOLD);
        let _ = decide(i32::MIN, i32::MAX, false, THRESHOLD);
    }

    #[test]
    fn an_empty_window_has_no_median() {
        assert_eq!(Window::new().median(), None);
        assert!(Window::new().is_empty());
    }

    #[test]
    fn an_odd_window_takes_the_middle_sample() {
        let mut w = Window::new();
        for v in [30, 10, 20] {
            w.push(v);
        }
        assert_eq!(w.len(), 3);
        assert_eq!(w.median(), Some(20));
    }

    #[test]
    fn an_even_window_takes_the_midpoint() {
        let mut w = Window::new();
        for v in [10, 20, 30, 40] {
            w.push(v);
        }
        assert_eq!(w.median(), Some(25));
    }

    #[test]
    fn the_median_ignores_a_single_wild_sample() {
        // A bird hopping once: the mean would carry the spike into the
        // published weight, the median does not.
        let mut w = Window::new();
        for _ in 0..8 {
            w.push(4000);
        }
        w.push(999_999);
        assert_eq!(w.median(), Some(4000));
    }

    #[test]
    fn a_full_window_keeps_the_most_recent_samples() {
        let mut w = Window::new();
        // The landing transient, then a settled bird.
        for _ in 0..WINDOW {
            w.push(9000);
        }
        for _ in 0..WINDOW {
            w.push(4000);
        }
        assert_eq!(w.len(), WINDOW);
        assert_eq!(
            w.median(),
            Some(4000),
            "the window must have forgotten the landing"
        );
    }

    #[test]
    fn seconds_are_written_with_one_decimal() {
        for (millis, expected) in [
            (0, "0.0"),
            (49, "0.0"),
            (50, "0.1"),
            (1_000, "1.0"),
            (1_249, "1.2"),
            (1_250, "1.3"),
            (12_340, "12.3"),
            (600_000, "600.0"),
        ] {
            let mut buf = heapless::String::new();
            write_secs(&mut buf, millis);
            assert_eq!(buf.as_str(), expected, "{millis} ms");
        }
    }
}

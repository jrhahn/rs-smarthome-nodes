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

/// Shortest interval between two presence-driven publishes, in seconds.
///
/// The scale is the one sensor here that can ask for airtime on its own, and on
/// the night of 2026-09-09 it asked for all of it: an uncalibrated cell kept
/// crossing its own threshold, the node published on every arrival, and eleven
/// hours of that at ~230 radio sessions an hour flattened a 2000 mAh pack. The
/// broker log is the record -- 3 to 6 connections an hour before, 230 an hour
/// after.
///
/// One a minute, not the ten a minute that first suggested itself: ten a minute
/// is 600 an hour, two and a half times the rate that did the damage, and about
/// 72 mAh an hour once the radio is paid for. At one a minute a node that
/// flaps without pause still lives past ten days, and a real visit is never
/// reported more than a minute late.
pub const MIN_PUBLISH_GAP_SECS: u32 = 60;

/// After this long continuously loaded, a load stops counting as a visitor.
///
/// The rate limit above bounds *flapping*; this bounds *sticking*, which is the
/// other way the same night could have gone. A bird does not sit on a feeder
/// for ten minutes, so a load that does is snow, a twig, a pan resting against
/// the enclosure, or a tare baseline taken while the beam was being handled --
/// and none of those should hold the node on its expensive cadence.
///
/// What happens at the bound is *absorption*, not disbelief: `main` moves the
/// baseline to the current reading. Clearing the presence flag alone left the
/// load above the threshold, so the next round read `Arrived` and the node
/// oscillated. [`drift_band`] refuses a step this large because it cannot tell
/// a step from a visitor; ten minutes is the evidence it lacks.
pub const STUCK_AFTER_SECS: u32 = 600;

// Eleven hours is what the flapping night cost. Whatever this constant becomes,
// it has to stay a small fraction of that, and long enough that a bird may
// legitimately linger. Checked at compile time rather than in a test: it is a
// claim about a constant, so the right failure is a build that stops, and
// `assert!` over a constant inside a test is a value clippy folds away anyway.
const _: () = assert!(STUCK_AFTER_SECS <= 3600, "not a bound on a night");
const _: () = assert!(STUCK_AFTER_SECS >= 120, "a bird may legitimately linger");

/// How long the baseline may stay frozen in [`Decision::Unexplained`] before
/// `main` adopts the current reading instead.
///
/// The freezing is deliberate and stays: a delta too large for creep and too
/// small for a visit is exactly the case where guessing is wrong, and absorbing
/// it would quietly eat a light bird. What was missing is a way *out*. Without
/// one the state is absorbing rather than transient -- every later round starts
/// from the same stale baseline, so it stays outside the band, so it freezes
/// again, for ever. The node then cannot see any visitor at all until someone
/// tares it by hand.
///
/// That is not hypothetical. On 2026-09-24 the terrace node sat in it for
/// **thirty-two hours** and counted nothing. The trigger was switching
/// `temp_coeff` on: the baseline had spent the afternoon tracking a thermal
/// ramp of 38 g, the correction removed that ramp in a single round, and the
/// baseline was left 38 g away from a reading it could never rejoin. Anything
/// that shifts the raw scale does this -- a new calibration, a remount, snow
/// sliding off -- so the recovery has to be general rather than a special case
/// for one setting.
///
/// The bound is the same argument as [`STUCK_AFTER_SECS`] and deliberately the
/// same length: ten minutes of an unchanging in-between reading is evidence
/// that it is the new empty state, not a visitor. A bird heavy enough to count
/// never lands here anyway -- it is over the threshold, so it reads `Arrived`.
/// Only something under the trigger weight can sit in this band, and that is
/// something the node has already decided not to count.
pub const UNEXPLAINED_ADOPT_AFTER_SECS: u32 = STUCK_AFTER_SECS;


/// Whole rounds of `round_secs` needed to cover `secs`, at least one.
///
/// Rounded up: the loop only wakes on its own cadence, so a budget between two
/// multiples would be spent early.
pub const fn rounds_for(secs: u32, round_secs: u32) -> u32 {
    let round = if round_secs == 0 { 1 } else { round_secs };
    let rounds = secs.div_ceil(round);
    if rounds == 0 {
        1
    } else {
        rounds
    }
}

/// Whether a presence publish may spend airtime now.
///
/// `rounds_since_publish` is what [`crate::state::idle_wakes`] already counts:
/// wakes since the last publish of any kind, reset by every publish. So the
/// limiter needs no clock and no second counter -- it reuses the one the
/// heartbeat already keeps.
pub const fn may_publish(rounds_since_publish: u32, gap_rounds: u32) -> bool {
    rounds_since_publish >= gap_rounds
}

/// Shortest load that is counted as a visit, in milliseconds.
///
/// The counter used to increment on the rising edge, before anyone knew how
/// long the load would stay. That counts a bird, and it also counts every
/// bounce: on 2026-09-12 the terrace reported 193 arrivals in ten hours, and
/// the durations published alongside them included 0.0, 0.1 and 0.3 seconds --
/// touches, not meals. The weights in the same window were credible (7 to 18 g,
/// which is a blue tit to a great tit), so the cell was not imagining loads;
/// the edge simply is not evidence of a visit yet.
///
/// One second is chosen against [`crate::VISIT_SETTLE`] rather than against
/// birds: samples in the first 400 ms are thrown away as ringing, so a visit
/// shorter than that has *no* settled reading behind it and its published
/// weight is whichever conversion tripped the threshold. At one second there
/// are always at least 600 ms of settled samples, which makes "counted" and
/// "has a real weight" the same set.
///
/// This does not make the count a count of *birds*. One bird that hops off and
/// back on is still two, and the entity is named for what it measures.
pub const MIN_COUNTED_VISIT_MILLIS: u64 = 1_000;

/// Whether a finished visit lasted long enough to be counted.
///
/// Takes the duration [`crate::watch_visit`] measured, which is the time from
/// the rising edge to the last sample still above the threshold -- so a load
/// that vanished immediately arrives here as a handful of milliseconds.
pub const fn counts_as_visit(millis: u64) -> bool {
    millis >= MIN_COUNTED_VISIT_MILLIS
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

/// How many readings a re-zero is taken over.
///
/// A tare used to be one number. That is fine on a bench and wrong on a feeder:
/// the press arrives retained, the node acts on it whenever it next wakes, and
/// nothing stops a bird being on the perch at that moment. One sample makes
/// that bird the new zero, permanently and silently.
///
/// Sixteen at the cell's ~10 SPS is about 1.6 s of sampling, and the median of
/// them survives anything that occupies a minority of that window -- a landing,
/// a hop, a gust. It does not survive a bird that sits still through the whole
/// window, and nothing measured only at tare time could: the node has no way to
/// tell that weight from the feeder's own. That case is the human's to avoid,
/// which is why the count is small enough to stay inside "I am looking at it".
pub const TARE_SAMPLES: usize = 16;

/// Largest spread, in raw ticks, a re-zero will accept across its samples.
///
/// The median handles a *minority* of disturbed samples. This catches the case
/// it cannot: readings that disagree wildly throughout, meaning something was
/// moving the whole time and no single number describes the empty feeder. Then
/// refusing is right -- a tare is not urgent, and a wrong zero is expensive to
/// notice, since every later weight is quietly shifted by it.
///
/// Sized off the measured resting scatter: `docs/commissioning.md` recorded
/// about ±70 counts on a settled chain 2026-09-04, so 2000 is far outside
/// normal noise while still well under a bird.
pub const TARE_MAX_SPREAD: i32 = 2000;

/// Whether a set of tare samples is consistent enough to re-zero from.
pub fn tare_spread_ok(samples: &[i32]) -> bool {
    let Some(&first) = samples.first() else {
        return false;
    };
    let mut lo = first;
    let mut hi = first;
    for &v in samples {
        lo = lo.min(v);
        hi = hi.max(v);
    }
    hi.saturating_sub(lo) <= TARE_MAX_SPREAD
}

/// Format a duration in milliseconds as seconds with one decimal, in the same
/// float-free style as the other published values.
pub fn write_secs(buf: &mut heapless::String<16>, millis: u64) {
    let tenths = (millis + 50) / 100;
    let _ = write!(buf, "{}.{}", tenths / 10, tenths % 10);
}

#[cfg(test)]
mod tare_tests {
    use super::*;

    /// A median over the window is the whole point: one bird landing mid-tare
    /// must not become the new zero.
    #[test]
    fn a_bird_arriving_partway_through_does_not_move_the_zero() {
        let mut w = Window::new();
        // Eleven readings of an empty feeder, then five with a 20 g bird on it.
        for _ in 0..11 {
            w.push(1000);
        }
        for _ in 0..5 {
            w.push(1000 + 8400);
        }
        assert_eq!(w.median(), Some(1000));
    }

    /// The guard the median cannot give: if nothing held still, refuse.
    #[test]
    fn readings_that_never_settle_are_refused() {
        let steady: Vec<i32> = (0..TARE_SAMPLES as i32).map(|i| 1000 + i * 10).collect();
        assert!(tare_spread_ok(&steady), "resting scatter must be accepted");

        let mut restless = steady.clone();
        restless[7] = 1000 + TARE_MAX_SPREAD + 1;
        assert!(!tare_spread_ok(&restless));
    }

    /// Refusing on no samples at all, rather than inventing a zero.
    #[test]
    fn no_samples_is_not_a_valid_tare() {
        assert!(!tare_spread_ok(&[]));
        assert_eq!(Window::new().median(), None);
    }

    /// The spread is a range, so it must not overflow on extreme readings.
    #[test]
    fn an_extreme_pair_does_not_overflow_the_spread() {
        assert!(!tare_spread_ok(&[i32::MIN, i32::MAX]));
    }

    /// A majority of disturbed samples is beyond what a median can repair --
    /// stated as a test so the limit is written down rather than assumed.
    #[test]
    fn a_bird_that_sits_through_the_whole_window_is_not_caught() {
        let mut w = Window::new();
        for _ in 0..TARE_SAMPLES {
            w.push(1000 + 8400);
        }
        assert_eq!(w.median(), Some(9400));
        let all: Vec<i32> = core::iter::repeat(9400).take(TARE_SAMPLES).collect();
        assert!(
            tare_spread_ok(&all),
            "a still bird looks exactly like a still feeder"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THRESHOLD: i32 = 4200; // 10 g at the default scale factor

    #[test]
    fn a_touch_shorter_than_a_second_is_not_counted() {
        // The durations the terrace actually published on 2026-09-12 in the
        // hour it reported 67 arrivals. The short ones are what this rejects.
        for millis in [0, 100, 300, 600, 700, 999] {
            assert!(!counts_as_visit(millis), "{millis} ms counted");
        }
    }

    #[test]
    fn a_visit_of_a_second_or_more_is_counted() {
        for millis in [1_000, 1_300, 2_100, 3_500, 9_300, 60_000] {
            assert!(counts_as_visit(millis), "{millis} ms not counted");
        }
    }

    #[test]
    fn nothing_inside_the_settling_window_is_counted() {
        // Below `VISIT_SETTLE` (400 ms) a visit has no settled samples at all
        // and its weight is whichever conversion tripped the threshold. A
        // counted visit must never be one of those.
        assert!(!counts_as_visit(399));
        assert!(!counts_as_visit(400));
    }

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
    fn a_stranded_baseline_never_frees_itself() {
        // The failure the recovery exists for, reproduced: a step that shifts
        // the raw scale leaves the baseline outside the band, and `decide`
        // alone can never bring the two back together. Every later round starts
        // from the same stale baseline and reaches the same verdict.
        let baseline = 1000;
        let step = drift_band(THRESHOLD) + 1; // just too big for creep
        let raw = baseline - step; // and negative, so never a visit either

        for round in 0..1000 {
            match decide(raw, baseline, false, THRESHOLD) {
                Decision::Unexplained { .. } => {}
                other => panic!("round {round} escaped on its own: {other:?}"),
            }
        }
    }

    #[test]
    fn a_countable_visitor_is_never_in_the_unexplained_band() {
        // What makes adopting the reading safe. The recovery absorbs whatever
        // is on the cell, so it must be impossible for a bird the node would
        // have counted to be sitting there: at or above the threshold every
        // delta reads `Arrived`, never `Unexplained`.
        for threshold in [1, 100, THRESHOLD, 46_570] {
            for over in [0, 1, 500, 100_000] {
                let delta = threshold + over;
                assert!(
                    matches!(
                        decide(1000 + delta, 1000, false, threshold),
                        Decision::Arrived { .. }
                    ),
                    "threshold {threshold}, delta {delta} must be a visit, not unexplained"
                );
            }
        }
    }

    #[test]
    fn the_recovery_bound_is_counted_in_whole_idle_rounds() {
        // Ten minutes at the terrace's 5 s idle cadence, and the rounding is
        // up: a budget between two multiples would otherwise be spent early.
        assert_eq!(rounds_for(UNEXPLAINED_ADOPT_AFTER_SECS, 5), 120);
        assert_eq!(rounds_for(UNEXPLAINED_ADOPT_AFTER_SECS, 7), 86);
        // A zero cadence must not divide by zero; one round is the floor.
        assert_eq!(rounds_for(UNEXPLAINED_ADOPT_AFTER_SECS, 0), 600);
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

    // --- Rate limiting ------------------------------------------------------

    #[test]
    fn the_gap_is_rounded_up_to_whole_rounds() {
        // 60 s at a 2 s idle round is 30 wakes; at 7 s it is 9, not 8, because
        // eight would let the publish through at 56 s.
        assert_eq!(rounds_for(60, 2), 30);
        assert_eq!(rounds_for(60, 7), 9);
        assert_eq!(rounds_for(60, 60), 1);
        assert_eq!(rounds_for(60, 600), 1);
    }

    #[test]
    fn a_zero_round_does_not_divide_by_zero() {
        assert_eq!(rounds_for(60, 0), 60);
    }

    #[test]
    fn the_limiter_lets_a_first_visit_straight_through() {
        // The heartbeat has just fired, so the counter is high: a bird landing
        // now must be reported at once, which is the whole point of the node.
        assert!(may_publish(300, rounds_for(MIN_PUBLISH_GAP_SECS, 2)));
    }

    #[test]
    fn the_limiter_holds_back_a_flapping_scale() {
        let gap = rounds_for(MIN_PUBLISH_GAP_SECS, 2);
        assert!(!may_publish(0, gap), "immediately after a publish");
        assert!(!may_publish(gap - 1, gap), "one round short");
        assert!(may_publish(gap, gap), "exactly at the gap");
    }

    /// The number is a battery budget, so assert the budget rather than the
    /// number: whatever the constant becomes, the worst case has to stay
    /// survivable for more than a week.
    #[test]
    fn the_worst_case_rate_still_leaves_over_a_week() {
        let per_hour = 3600 / MIN_PUBLISH_GAP_SECS;
        // ~0.12 mAh per radio session, measured against a 2000 mAh pack.
        let mah_per_hour = per_hour * 12 / 100;
        let hours = 2000 / mah_per_hour.max(1);
        assert!(
            hours > 24 * 7,
            "{per_hour}/h would flatten the pack in {hours} h"
        );
    }
}

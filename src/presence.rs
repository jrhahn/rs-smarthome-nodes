//! The bird-presence decision.
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
//! * [`Decision::Arrived`] — a visit begins.
//! * [`Decision::Departed`] — the visit ended; publish and resume idle polling.
//! * [`Decision::Quiet`] — nothing there; absorb the reading as creep.
//! * [`Decision::Unexplained`] — something is on the scale that is neither.
//!   See [`drift_band`] for why this case exists at all.

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
}

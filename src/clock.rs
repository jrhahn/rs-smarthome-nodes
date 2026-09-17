//! Turning "the time is now" into "that reading was taken then".
//!
//! The C3 has no battery-backed clock, so a node learns the time from
//! [`crate::ntp`] and forgets it when power goes. That sync happens once per
//! publish round, at the only moment it can: the radio is up, which is also the
//! only moment the node costs anything to run.
//!
//! Two things follow, and this module is both of them.
//!
//! **The answer has to be checked before it is believed.** A server that is
//! itself lost, a stray datagram, a counter that wrapped -- each yields a
//! number, and a wrong timestamp is worse than none at all, because it sorts
//! into the history and stays there. [`is_plausible`] is the bound: outside it,
//! the node simply publishes unstamped and the archiver falls back to the time
//! of receipt, which is what every reading did before any of this existed.
//!
//! **The sync is not when the reading was taken.** Samples are collected before
//! the radio comes up -- deliberately, a battery node should not hold an
//! association open through an SCD41 conversion -- and on a node duty-cycling
//! an SDS011 fan that gap is tens of seconds, which is a whole rollup bucket.
//! So each sample carries the monotonic instant it was taken, and [`stamp`]
//! walks the freshly synced wall clock back by its age.
//!
//! Note what is deliberately *not* here: nothing is kept across deep sleep. The
//! RTC domain could hold a clock -- `Rtc::set_current_time` writes a boot time
//! into registers that survive a wake -- and an earlier draft did that. It was
//! dropped because the fallback is better than the thing it falls back to: a
//! node whose sync failed would be choosing between a clock that has been
//! drifting on the C3's internal RC oscillator (whole seconds per hour, and
//! temperature-dependent) and letting the archiver stamp the reading on
//! arrival, which is accurate to the network hop. The drifted clock only wins
//! when the archiver is *also* down, and it buys a worse timestamp everywhere
//! else. It would also have meant trusting the same RTC domain whose
//! `wohnzimmer` incident is written up in [`crate::state`].

/// Lower edge of the plausible window: 2025-01-01T00:00:00Z.
///
/// Comfortably before this fleet existed, and comfortably above every value a
/// confused source produces -- a zero, a small count of seconds since boot, or
/// an NTP era mistake landing in 1900.
pub const SANE_FROM_MS: u64 = 1_735_689_600_000;

/// Upper edge: 2100-01-01T00:00:00Z. Nothing here will still be running, and a
/// garbage value reads as "no clock" rather than as a date in the year 500 000.
pub const SANE_UNTIL_MS: u64 = 4_102_444_800_000;

/// Could this be a real moment, as opposed to a confused one?
pub fn is_plausible(millis: u64) -> bool {
    (SANE_FROM_MS..SANE_UNTIL_MS).contains(&millis)
}

/// When a reading of age `age_ms` was taken, given that it is `now_ms` now.
///
/// Saturating rather than wrapping: the two clocks are independent -- one from
/// the network, one from the chip's monotonic timer -- so nothing structurally
/// prevents an age larger than the wall clock. That cannot happen with a
/// plausible `now_ms` and a round that lasts seconds, and if it ever did, an
/// epoch timestamp is visibly wrong where a wrapped one would look like the
/// year 584 million.
pub fn stamp(now_ms: u64, age_ms: u64) -> u64 {
    now_ms.saturating_sub(age_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_source_that_does_not_know_the_time_is_not_believed() {
        // What a never-set clock and a failed parse produce.
        assert!(!is_plausible(0));
        assert!(!is_plausible(1));
        assert!(!is_plausible(30_000));
        // And what garbage looks like.
        assert!(!is_plausible(u64::MAX));
        assert!(!is_plausible(u64::from(u32::MAX)));
    }

    #[test]
    fn a_time_this_fleet_could_be_reporting_is_a_clock() {
        // 2026-09-16T20:00:00Z, the day this was written.
        assert!(is_plausible(1_789_675_200_000));
    }

    #[test]
    fn the_window_is_half_open_at_both_ends() {
        assert!(is_plausible(SANE_FROM_MS));
        assert!(!is_plausible(SANE_FROM_MS - 1));
        assert!(!is_plausible(SANE_UNTIL_MS));
        assert!(is_plausible(SANE_UNTIL_MS - 1));
    }

    #[test]
    fn seconds_are_not_mistaken_for_milliseconds() {
        // The unit slip this bound exists to catch: a Unix *second* count used
        // where milliseconds are meant lands in 1970, far below the window.
        assert!(!is_plausible(1_789_675_200));
    }

    #[test]
    fn a_reading_is_stamped_when_it_was_taken_not_when_it_was_sent() {
        let now = 1_789_675_200_000;
        // Fresh off the sensor.
        assert_eq!(stamp(now, 0), now);
        // Taken 40 s ago, while the SDS011's fan was spinning up.
        assert_eq!(stamp(now, 40_000), now - 40_000);
    }

    #[test]
    fn an_impossible_age_does_not_wrap_into_the_far_future() {
        assert_eq!(stamp(1_000, 5_000), 0);
        assert_eq!(stamp(0, u64::MAX), 0);
    }
}

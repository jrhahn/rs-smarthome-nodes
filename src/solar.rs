//! When it is dark at the feeder, without trigonometry.
//!
//! The night cadence in [`crate::config`] needs one fact: is it dark enough
//! that no bird will land. Computing sunrise and sunset properly means solar
//! declination and an hour angle, so `sin`, `cos` and `acos` -- none of which
//! `core` has, and adding `libm` to a firmware for a decision an hour wide is
//! the expensive kind of correct.
//!
//! This node does not move. Sunrise and sunset at one fixed place trace a
//! smooth curve through the year, so twenty-four points off that curve and a
//! straight line between them is the whole method. Measured against every day
//! of 2025 at 49.87 N, 8.65 E, the worst interpolation error is **two
//! minutes**.
//!
//! The date comes the same way. A real day-of-year needs the civil calendar and
//! its leap rules; days since the epoch modulo the tropical year is within
//! **one day** of it across 2026-2040, which is another two minutes of sunset.
//! Four minutes total, against a window whose edges are deliberately blunt.

/// Sunrise and sunset in minutes past midnight **UTC**, at twenty-four points
/// spaced evenly through the year, starting at 1 January.
///
/// Taken from Open-Meteo for 49.87 N, 8.65 E in 2025 rather than derived, so
/// the numbers carry refraction and the standard -0.833 degree horizon without
/// this file having to model either. They shift by seconds between years, which
/// is four orders of magnitude below what the caller asks.
const TABLE: [(u16, u16); 24] = [
    ( 443,  935), ( 436,  954), ( 419,  978), ( 393, 1006),
    ( 364, 1031), ( 332, 1055), ( 300, 1079), ( 268, 1102),
    ( 238, 1127), ( 216, 1148), ( 201, 1166), ( 196, 1176),
    ( 202, 1177), ( 217, 1165), ( 236, 1145), ( 258, 1119),
    ( 280, 1088), ( 304, 1054), ( 327, 1021), ( 350,  990),
    ( 375,  962), ( 399,  940), ( 423,  926), ( 438,  924),
];

/// Minutes in a day, and the modulus every time-of-day here is taken in.
const DAY_MIN: u32 = 24 * 60;

/// A tropical year in ten-thousandths of a day.
const TROPICAL_E4: u64 = 3_652_425;

/// Day of the year, 0..365, from epoch milliseconds.
pub const fn day_of_year(millis: u64) -> u32 {
    let days = millis / 86_400_000;
    let years = days * 10_000 / TROPICAL_E4;
    (days - years * TROPICAL_E4 / 10_000) as u32
}

/// Minutes past midnight UTC.
pub const fn minute_of_day(millis: u64) -> u32 {
    ((millis / 60_000) % DAY_MIN as u64) as u32
}

/// Sunrise and sunset for `doy`, in minutes past midnight UTC.
///
/// Wraps at the year end on purpose: the last table point and the first are
/// fifteen days apart across New Year like any other pair, and treating them as
/// neighbours is what keeps the curve continuous there.
pub const fn sun(doy: u32) -> (u32, u32) {
    let n = TABLE.len() as u32;
    // Position along the table in sixteenths, so the interpolation stays in
    // integers without throwing away the fraction that matters.
    let pos = doy * n * 16 / 366;
    let i = (pos / 16) % n;
    let j = (i + 1) % n;
    let f = pos % 16;
    let (r0, s0) = TABLE[i as usize];
    let (r1, s1) = TABLE[j as usize];
    (lerp(r0 as u32, r1 as u32, f), lerp(s0 as u32, s1 as u32, f))
}

/// Linear step from `a` to `b`, `f` sixteenths of the way.
const fn lerp(a: u32, b: u32, f: u32) -> u32 {
    if b >= a {
        a + (b - a) * f / 16
    } else {
        a - (a - b) * f / 16
    }
}

/// Whether `minute` (UTC) falls in the dark stretch of day `doy`, with
/// `margin_min` of daylight kept clear at each end.
///
/// The margin is not politeness. Birds feed into dusk and start again before
/// full light, and the two errors do not cost the same: opening the window too
/// early loses visits that can never be recovered, while closing it too late
/// costs a few minutes of polling. So it is applied outward at both ends, and
/// the window is always the *inside* of the night.
pub const fn is_night(minute: u32, doy: u32, margin_min: u32) -> bool {
    let (sunrise, sunset) = sun(doy);
    // Clamped, so an absurd margin cannot invert the window into "always
    // night", which would quietly stop the node counting anything.
    let margin = if margin_min > 180 { 180 } else { margin_min };
    let from = (sunset + margin) % DAY_MIN;
    let to = (sunrise + DAY_MIN - margin) % DAY_MIN;
    let m = minute % DAY_MIN;
    if from == to {
        false
    } else if from < to {
        m >= from && m < to
    } else {
        m >= from || m < to
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sunrise and sunset in UTC minutes on four days of 2025, off the same
    /// source the table came from. Held here as a *separate* set of days from
    /// the table's own support points, so a broken interpolation cannot pass by
    /// reproducing the numbers it was built from.
    const REFERENCE: [(u32, u32, u32); 4] = [
        (354, 440, 926),  // winter solstice
        (78, 328, 1058),  // vernal equinox
        (171, 196, 1177), // summer solstice
        (265, 313, 1041), // autumnal equinox
    ];

    #[test]
    fn the_table_reproduces_the_solstices_and_equinoxes() {
        for (doy, sunrise, sunset) in REFERENCE {
            let (r, s) = sun(doy);
            assert!(
                r.abs_diff(sunrise) <= 5,
                "day {doy}: sunrise {r} against {sunrise}"
            );
            assert!(
                s.abs_diff(sunset) <= 5,
                "day {doy}: sunset {s} against {sunset}"
            );
        }
    }

    #[test]
    fn the_curve_is_continuous_across_new_year() {
        // The seam the wrapping index exists for. Neighbouring days must not
        // jump: the last table point and the first are fifteen days apart like
        // any other pair.
        let (r_dec, s_dec) = sun(364);
        let (r_jan, s_jan) = sun(0);
        assert!(r_dec.abs_diff(r_jan) < 10, "sunrise {r_dec} -> {r_jan}");
        assert!(s_dec.abs_diff(s_jan) < 10, "sunset {s_dec} -> {s_jan}");
    }

    #[test]
    fn every_day_of_the_year_gives_a_sane_pair() {
        for doy in 0..366 {
            let (r, s) = sun(doy);
            assert!(r < DAY_MIN && s < DAY_MIN, "day {doy}: {r}, {s}");
            assert!(s > r, "day {doy}: sunset {s} must follow sunrise {r}");
            // Darmstadt's shortest and longest days, with room to spare.
            let daylight = s - r;
            assert!((7 * 60..=17 * 60).contains(&daylight), "day {doy}: {daylight} min");
        }
    }

    #[test]
    fn the_day_of_year_tracks_the_calendar_without_one() {
        // 2026-09-26T00:00Z is day 268 counting from zero, 2026-01-01 is day
        // 0. Both land within the one day the approximation is allowed.
        assert!(day_of_year(1_790_380_800_000).abs_diff(268) <= 1);
        assert!(day_of_year(1_767_225_600_000).abs_diff(0) <= 1);
    }

    #[test]
    fn midnight_is_night_and_noon_is_not() {
        for (doy, _, _) in REFERENCE {
            assert!(is_night(0, doy, 30), "day {doy} at midnight");
            assert!(!is_night(12 * 60, doy, 30), "day {doy} at noon");
        }
    }

    #[test]
    fn the_margin_keeps_the_window_inside_the_dark() {
        // Taken from `sun` rather than written out, so this tests the window
        // arithmetic and not the table's fourth digit.
        let doy = 265;
        let (sunrise, sunset) = sun(doy);
        let margin = 30;
        assert!(!is_night(sunset, doy, margin), "sunset itself is not night");
        assert!(
            !is_night(sunset + margin - 1, doy, margin),
            "a minute short of the margin"
        );
        assert!(is_night(sunset + margin, doy, margin), "one minute past it");
        assert!(
            is_night(sunrise - margin - 1, doy, margin),
            "still dark before dawn"
        );
        assert!(
            !is_night(sunrise - margin, doy, margin),
            "the margin before sunrise"
        );
    }

    #[test]
    fn a_wider_margin_only_ever_shrinks_the_window() {
        // Monotonic, which is what makes the slider behave: no setting may turn
        // a daylight minute into night.
        for doy in (0..366).step_by(7) {
            for minute in (0..DAY_MIN).step_by(11) {
                if is_night(minute, doy, 60) {
                    assert!(
                        is_night(minute, doy, 30),
                        "day {doy}, minute {minute}: a wider margin added night"
                    );
                }
            }
        }
    }

    #[test]
    fn an_absurd_margin_cannot_make_it_always_night() {
        // The clamp. Midsummer night here is barely five hours, so a margin of
        // hours would otherwise invert the comparison and stop the node
        // counting anything at all.
        for margin in [180, 500, u32::MAX] {
            assert!(!is_night(12 * 60, 171, margin), "margin {margin} at noon");
        }
    }
}

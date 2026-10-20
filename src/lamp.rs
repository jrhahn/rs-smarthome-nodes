//! How bright the `solarleuchte` is, at any moment of any evening.
//!
//! Pure arithmetic: no HAL, no floats, no clock of its own. Everything here is
//! a function of the minute, the day of the year and the cell voltage, which
//! is what makes it testable on the host and what keeps the firmware's job
//! down to reading those three things and writing a duty.
//!
//! The duty is a product of three independent factors, and each exists for its
//! own reason:
//!
//! ```text
//! duty = BREATH[step] × MAX_DUTY_PCT × twilight × charge
//! ```
//!
//! - [`BREATH`] is the shape — a gamma-corrected half-cosine, so the fade
//!   looks linear to the eye and has no corners.
//! - [`MAX_DUTY_PCT`] is the hard ceiling, compiled in, because the string
//!   draws 640 mA flat out and would empty the cell in one evening.
//! - [`twilight_permille`] decides when the evening starts and ends, and
//!   ramps rather than switches at both edges.
//! - [`charge_permille`] pulls everything back when the cell is low, which is
//!   what makes the lamp dim through a dark week instead of dying in it.
//!
//! The reasoning behind the numbers is in
//! [`docs/solarleuchte.md`](../docs/solarleuchte.md).

use crate::battery;
use crate::solar;

/// **Gamma-corrected half-cosine, one full breath, 12-bit duty.**
///
/// Two corrections live in this one table, and both are needed.
///
/// The *cosine* is why the breath has no corners: a triangle ramp visibly
/// kinks at the top and bottom, where this flattens into the turn.
///
/// The *gamma* is why it looks linear at all. The eye responds roughly to the
/// 1/2.2 power of emitted light, so feeding a linear duty ramp to an LED makes
/// it leap away from black and then crawl once it is bright. Raising the
/// intended perceived brightness to 2.2 before it becomes a duty cancels that.
///
/// 256 entries, because the largest step between neighbours is then **1.23 %**
/// of perceived brightness -- under the ~2 % that is noticeable -- so no
/// interpolation is needed. Generated with:
///
/// ```python
/// round((((1 - math.cos(2*math.pi*i/256)) / 2) ** 2.2) * 4095)
/// ```
///
/// Verified on hardware 2026-10-04, first through a PCA9685 and then through
/// LEDC: the curve never depended on what drove the LEDs.
pub const BREATH: [u16; 256] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 2, 2, 3, 4, 5, 6, 8, 10, 12, 15, 18, 21, 25, 29, 34,
    40, 46, 52, 60, 68, 77, 87, 97, 109, 122, 135, 150, 165, 182, 200, 219, 240, 261, 284, 308,
    334, 361, 389, 419, 450, 483, 517, 553, 590, 629, 669, 710, 753, 798, 844, 891, 940, 990, 1042,
    1095, 1149, 1204, 1261, 1319, 1378, 1438, 1499, 1561, 1624, 1688, 1753, 1818, 1884, 1950, 2017,
    2084, 2152, 2220, 2288, 2356, 2424, 2492, 2559, 2626, 2693, 2760, 2825, 2890, 2955, 3018, 3080,
    3142, 3202, 3261, 3318, 3374, 3428, 3481, 3533, 3582, 3629, 3675, 3718, 3760, 3799, 3836, 3871,
    3904, 3934, 3961, 3986, 4009, 4029, 4046, 4061, 4073, 4083, 4090, 4094, 4095, 4094, 4090, 4083,
    4073, 4061, 4046, 4029, 4009, 3986, 3961, 3934, 3904, 3871, 3836, 3799, 3760, 3718, 3675, 3629,
    3582, 3533, 3481, 3428, 3374, 3318, 3261, 3202, 3142, 3080, 3018, 2955, 2890, 2825, 2760, 2693,
    2626, 2559, 2492, 2424, 2356, 2288, 2220, 2152, 2084, 2017, 1950, 1884, 1818, 1753, 1688, 1624,
    1561, 1499, 1438, 1378, 1319, 1261, 1204, 1149, 1095, 1042, 990, 940, 891, 844, 798, 753, 710,
    669, 629, 590, 553, 517, 483, 450, 419, 389, 361, 334, 308, 284, 261, 240, 219, 200, 182, 165,
    150, 135, 122, 109, 97, 87, 77, 68, 60, 52, 46, 40, 34, 29, 25, 21, 18, 15, 12, 10, 8, 6, 5, 4,
    3, 2, 2, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// Full scale of the 12-bit LEDC duty.
pub const FULL_SCALE: u32 = 4095;

/// **Hard ceiling on the duty, as a percentage. Compiled in, not configurable.**
///
/// The string measured **640 mA at 4.06 V across 25 LEDs** (2026-10-04), i.e.
/// 25.6 mA each and ~29 mA at a full 4.2 V. Flat out that is 1660 mAh an
/// evening from a 2000 mAh cell -- the lamp would empty it in just over one
/// night and leave the panel 1.1x of margin.
///
/// 70 % is where the cell can still afford a bright evening: it is 85 % of
/// perceived brightness, because energy falls with the duty while brightness
/// falls only with `duty^(1/2.2)`.
///
/// **It is a constant rather than a config key on purpose.** The ceiling that
/// [`charge_permille`] applies belongs on MQTT so the trade can be made in the
/// garden, but a config able to reach 100 % would run the string at a
/// DC-equivalent 29 mA for months against a 20 mA rating. MQTT may lower the
/// brightness; it may not raise it past this line.
pub const MAX_DUTY_PCT: u32 = 70;

/// [`MAX_DUTY_PCT`] of [`FULL_SCALE`], as the duty actually written.
///
/// Named rather than recomputed, because [`duty`] clamps to it explicitly: the
/// rounding there can land a single count above the percentage, and a ceiling
/// that is enforced in one visible place is worth more than one that happens
/// to fall out of the arithmetic.
pub const MAX_DUTY: u32 = FULL_SCALE * MAX_DUTY_PCT / 100;

/// Minutes after sunset before the lamp lights at all, by default.
///
/// Home Assistant can move it (`config/lamp_start`, see [`Schedule`]).
///
/// The cheapest saving available, because it costs nothing anyone can see: in
/// civil twilight the sky still outshines the lamp, so the first half hour of
/// burning is spent on nobody. Every minute here is a minute of budget.
pub const START_AFTER_SUNSET_MIN: u32 = 45;

/// Length of the ramp from first light to full brightness.
///
/// Roughly the length of civil twilight at 49.87 N, so the lamp comes up as
/// the sky goes down rather than switching on into it.
pub const RAMP_MIN: u32 = 40;

/// When the evening ends by default, in **minutes past midnight UTC**.
///
/// Home Assistant can move it (`config/lamp_off`, see [`Schedule`]).
///
/// UTC and not local time, because [`solar`] deliberately has no civil
/// calendar and therefore no EU daylight-saving rule. 20:30 UTC is 21:30 CET
/// in winter and 22:30 CEST in summer — and summer is the half of the year
/// where the lamp burns under an hour anyway and the panel returns five times
/// what it needs, so the hour costs nothing. Implementing DST would cost more
/// code than the behaviour is worth.
///
/// Until 2026-10 this was 22:00 UTC. An hour and a half less a night is the
/// cheapest reserve there is for the weeks the panel does not keep up.
///
/// One property worth keeping: the lit window never crosses midnight UTC, so
/// every comparison in this module is ordinary arithmetic on one day's
/// minutes. A cutoff is a time of day, `0..1440`, so it cannot.
///
/// The summer solstice's window opens at 20:22 UTC, so at this default a June
/// evening is eight minutes of ramp and then nothing: deliberate, see above.
pub const CUTOFF_MINUTE_UTC: u32 = 20 * 60 + 30;

/// Length of the fade-out before [`CUTOFF_MINUTE_UTC`].
///
/// An abrupt cut at the end reads as a failure; ten minutes of breathing out
/// reads as intent.
pub const FADE_OUT_MIN: u32 = 10;

/// State of charge below which the lamp stays dark, in percent.
///
/// Not a protection — the BMS is that, and `battery.rs` warns from 3.0 V. This
/// is a choice: a lamp that spends the last of the cell on a dim glow has
/// nothing left to start the next evening with, and the cell ages faster for
/// it.
pub const DARK_BELOW_PCT: u32 = 20;

/// State of charge from which the lamp is allowed its full [`MAX_DUTY_PCT`].
pub const FULL_FROM_PCT: u32 = 90;

/// The two ends of the evening that Home Assistant can move.
///
/// Start is relative to sunset and not a clock time, because that is what
/// follows the season: a fixed 17:00 is dark in December and broad daylight
/// in June. The end is a clock time, because "off at half past ten" is what a
/// person means by it.
///
/// Neither is persisted, like the light's switch and slider: the retained
/// `config/` topics are re-delivered on every publish, and a lamp that cannot
/// reach the broker falls back to [`Schedule::DEFAULT`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Schedule {
    /// Minutes after sunset before the lamp lights.
    pub start_after_sunset_min: u32,
    /// When the evening ends, minutes past midnight UTC, `0..1440`.
    pub cutoff_minute_utc: u32,
}

impl Schedule {
    pub const DEFAULT: Schedule = Schedule {
        start_after_sunset_min: START_AFTER_SUNSET_MIN,
        cutoff_minute_utc: CUTOFF_MINUTE_UTC,
    };
}

/// Tonight's lit window as minutes past midnight UTC, `[start, cutoff)`.
///
/// One place that decides it, because two would drift: `twilight_permille`
/// shapes the brightness inside the window and the firmware's sleep decides
/// when to be awake for it, and a lamp awake outside its own window is a node
/// burning a fifth of its cell for nothing.
pub fn window(doy: u32, schedule: Schedule) -> (u32, u32) {
    let (_sunrise, sunset) = solar::sun(doy);
    (
        sunset + schedule.start_after_sunset_min,
        schedule.cutoff_minute_utc,
    )
}

/// Whether `minute_utc` falls inside [`window`].
pub fn is_evening(minute_utc: u32, doy: u32, schedule: Schedule) -> bool {
    let (start, cutoff) = window(doy, schedule);
    minute_utc >= start && minute_utc < cutoff
}

/// How far into the evening the lamp has got, in per mille of full brightness.
///
/// Zero before the schedule's start and from its cutoff on; a [`RAMP_MIN`]
/// climb at the start, a [`FADE_OUT_MIN`] fall at the end, and flat in
/// between. Where the two overlap on a short evening, the dimmer one wins.
pub fn twilight_permille(minute_utc: u32, doy: u32, schedule: Schedule) -> u32 {
    let (start, cutoff) = window(doy, schedule);
    let full = start + RAMP_MIN;
    let fade = cutoff.saturating_sub(FADE_OUT_MIN);

    // A cutoff at or before the start: an empty window, nothing to light.
    if start >= cutoff || minute_utc < start || minute_utc >= cutoff {
        return 0;
    }
    let ramp = if minute_utc < full {
        (minute_utc - start) * 1000 / RAMP_MIN
    } else {
        1000
    };
    let tail = if minute_utc >= fade {
        (cutoff - minute_utc) * 1000 / FADE_OUT_MIN
    } else {
        1000
    };
    // `min`, because a short window puts the fade inside the ramp: taking the
    // ramp alone there would cut from mid-climb straight to dark.
    ramp.min(tail)
}

/// What the cell can afford tonight, in per mille of full brightness.
///
/// Linear in state of charge between [`DARK_BELOW_PCT`] and
/// [`FULL_FROM_PCT`]. Deliberately the whole of the weather model: the panel's
/// aspect, its shading and the last week of cloud all show up in the cell, so
/// reading the cell is strictly better than predicting any of them — and it
/// degrades gracefully, dimming through a dark week instead of going out in
/// the middle of it.
pub fn charge_permille(cell_mv: u32) -> u32 {
    let soc = battery::percent(cell_mv);
    if soc < DARK_BELOW_PCT {
        return 0;
    }
    if soc >= FULL_FROM_PCT {
        return 1000;
    }
    (soc - DARK_BELOW_PCT) * 1000 / (FULL_FROM_PCT - DARK_BELOW_PCT)
}

/// Everything about tonight that is *not* the breath, in per mille.
///
/// Split out from [`duty`] because the two change on completely different
/// timescales and, since 2026-10-05, in different tasks. This moves when the
/// sky moves, when the cell moves or when someone touches Home Assistant —
/// seconds at the fastest. [`shaped`] moves every 97 ms and must not wait for
/// an ADC conversion or an MQTT round trip to do it.
pub fn gate_permille(
    minute_utc: u32,
    doy: u32,
    cell_mv: u32,
    enabled: bool,
    brightness: u8,
    schedule: Schedule,
) -> u32 {
    if !enabled {
        return 0;
    }
    twilight_permille(minute_utc, doy, schedule) * charge_permille(cell_mv) / 1000
        * brightness as u32
        / 255
}

/// The breath at `step`, scaled by a gate from [`gate_permille`].
///
/// Pure arithmetic on two numbers already in hand, so the task that drives the
/// PWM can run on a fixed cadence and nothing else.
pub fn shaped(step: usize, gate_permille: u32) -> u32 {
    if gate_permille == 0 {
        return 0;
    }
    let shape = BREATH[step % BREATH.len()] as u32;
    // Largest intermediate is 4095 × 70 × 1000 ≈ 2.9e8, well inside u32.
    ((shape * MAX_DUTY_PCT * gate_permille + 50_000) / 100_000).min(MAX_DUTY)
}

/// The duty to write, 0..=[`MAX_DUTY`].
///
/// `step` walks [`BREATH`] and wraps on its own, so a caller only has to count
/// up. `enabled` and `brightness` are Home Assistant's, straight off
/// [`crate::config::Config`]; everything else is measured or computed.
///
/// **`brightness` scales the ceiling, it does not replace it.** 255 means "as
/// bright as [`MAX_DUTY_PCT`] allows", which is why that one is a constant and
/// this one is a slider.
///
/// Rounds rather than truncates, which costs a handful of extra zeros at the
/// dark end and buys back everything else.
pub fn duty(
    step: usize,
    minute_utc: u32,
    doy: u32,
    cell_mv: u32,
    enabled: bool,
    brightness: u8,
    schedule: Schedule,
) -> u32 {
    shaped(
        step,
        gate_permille(minute_utc, doy, cell_mv, enabled, brightness, schedule),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Schedule = Schedule::DEFAULT;

    /// Winter solstice: sunset 926 UTC per `solar`'s own reference set, so the
    /// lamp starts at 971 and is at full from 1011 until the fade at 1220.
    const WINTER_DOY: u32 = 354;

    /// A cell comfortably above [`FULL_FROM_PCT`], so brightness tests are not
    /// silently scaled by the charge factor.
    const FULL_CELL_MV: u32 = 4150;

    #[test]
    fn the_lamp_is_dark_until_the_margin_has_passed() {
        let (_, sunset) = solar::sun(WINTER_DOY);
        assert_eq!(twilight_permille(sunset, WINTER_DOY, S), 0);
        assert_eq!(
            twilight_permille(sunset + START_AFTER_SUNSET_MIN - 1, WINTER_DOY, S),
            0
        );
        assert_eq!(
            twilight_permille(sunset + START_AFTER_SUNSET_MIN, WINTER_DOY, S),
            0
        );
        assert!(twilight_permille(sunset + START_AFTER_SUNSET_MIN + 1, WINTER_DOY, S) > 0);
    }

    #[test]
    fn the_ramp_climbs_to_full_and_stays_there() {
        let (_, sunset) = solar::sun(WINTER_DOY);
        let start = sunset + START_AFTER_SUNSET_MIN;
        assert_eq!(twilight_permille(start + RAMP_MIN / 2, WINTER_DOY, S), 500);
        assert_eq!(twilight_permille(start + RAMP_MIN, WINTER_DOY, S), 1000);
        assert_eq!(
            twilight_permille(start + RAMP_MIN + 60, WINTER_DOY, S),
            1000
        );
    }

    #[test]
    fn the_evening_fades_out_rather_than_stopping() {
        let fade = CUTOFF_MINUTE_UTC - FADE_OUT_MIN;
        assert_eq!(twilight_permille(fade, WINTER_DOY, S), 1000);
        assert_eq!(
            twilight_permille(fade + FADE_OUT_MIN / 2, WINTER_DOY, S),
            500
        );
        assert_eq!(twilight_permille(CUTOFF_MINUTE_UTC, WINTER_DOY, S), 0);
        assert_eq!(twilight_permille(CUTOFF_MINUTE_UTC + 30, WINTER_DOY, S), 0);
    }

    #[test]
    fn a_summer_evening_is_short_but_never_wraps_midnight() {
        // Summer solstice: sunset 1177 UTC, so the window is 1222..1230 — late,
        // short, and still inside the same day, which every comparison here
        // depends on.
        let doy = 171;
        let (_, sunset) = solar::sun(doy);
        assert!(sunset + START_AFTER_SUNSET_MIN < CUTOFF_MINUTE_UTC);
        assert_eq!(twilight_permille(sunset + 10, doy, S), 0);
        assert!(twilight_permille(CUTOFF_MINUTE_UTC - 5, doy, S) > 0);
    }

    #[test]
    fn a_low_cell_darkens_the_lamp_entirely() {
        assert_eq!(charge_permille(3000), 0);
        assert!(battery::percent(3000) < DARK_BELOW_PCT);
    }

    #[test]
    fn charge_scales_between_the_two_thresholds_and_clamps_above() {
        assert_eq!(charge_permille(4200), 1000);
        assert_eq!(charge_permille(4150), 1000);
        let mid = charge_permille(3800);
        assert!(
            (1..1000).contains(&mid),
            "expected a partial ceiling, got {mid}"
        );
    }

    #[test]
    fn charge_never_falls_as_the_cell_rises() {
        let mut last = 0;
        for mv in (3000..=4200).step_by(10) {
            let p = charge_permille(mv);
            assert!(p >= last, "charge_permille fell at {mv} mV: {last} -> {p}");
            last = p;
        }
    }

    #[test]
    fn the_duty_never_exceeds_the_compiled_ceiling() {
        let cap = MAX_DUTY;
        for step in 0..BREATH.len() {
            for minute in (0..1440).step_by(7) {
                for doy in [1, 90, 171, 265, 354] {
                    let d = duty(step, minute, doy, 4200, true, 255, S);
                    assert!(d <= cap, "duty {d} over cap {cap} at step {step}");
                }
            }
        }
    }

    #[test]
    fn the_brightest_moment_of_the_evening_reaches_the_ceiling() {
        let (_, sunset) = solar::sun(WINTER_DOY);
        let minute = sunset + START_AFTER_SUNSET_MIN + RAMP_MIN + 30;
        let peak = (0..BREATH.len())
            .map(|s| duty(s, minute, WINTER_DOY, FULL_CELL_MV, true, 255, S))
            .max()
            .unwrap();
        assert_eq!(peak, MAX_DUTY);
    }

    /// The minute of the winter evening where the twilight factor is 1.
    fn winter_peak_minute() -> u32 {
        let (_, sunset) = solar::sun(WINTER_DOY);
        sunset + START_AFTER_SUNSET_MIN + RAMP_MIN + 30
    }

    #[test]
    fn home_assistant_can_switch_the_lamp_off_entirely() {
        let minute = winter_peak_minute();
        for step in 0..BREATH.len() {
            assert_eq!(
                duty(step, minute, WINTER_DOY, FULL_CELL_MV, false, 255, S),
                0
            );
        }
    }

    #[test]
    fn the_brightness_slider_scales_the_ceiling_but_cannot_lift_it() {
        let minute = winter_peak_minute();
        let peak = |bri| {
            (0..BREATH.len())
                .map(|s| duty(s, minute, WINTER_DOY, FULL_CELL_MV, true, bri, S))
                .max()
                .unwrap()
        };
        assert_eq!(peak(255), MAX_DUTY);
        assert!(peak(128) < peak(255));
        assert!(peak(128) > peak(32));
        assert_eq!(peak(0), 0);
    }

    /// The split that keeps the PWM task free of the ADC and the radio: the
    /// two halves recombined must be the whole.
    #[test]
    fn the_gate_and_the_shape_compose_back_into_the_duty() {
        let minute = winter_peak_minute();
        for step in [0, 7, 64, 128, 200, 255] {
            for (enabled, bri, mv) in [
                (true, 255, FULL_CELL_MV),
                (true, 128, 3800),
                (true, 32, FULL_CELL_MV),
                (false, 255, FULL_CELL_MV),
            ] {
                let gate = gate_permille(minute, WINTER_DOY, mv, enabled, bri, S);
                assert_eq!(
                    shaped(step, gate),
                    duty(step, minute, WINTER_DOY, mv, enabled, bri, S)
                );
            }
        }
    }

    /// Never asleep while lit, which is the direction that can break something.
    ///
    /// Not an equality: at the very first minute the ramp is still at zero and
    /// the window is already open, which is right — the lamp has to be awake to
    /// start rising. The converse is what matters, and it is asserted both
    /// ways round: nothing outside the window is ever lit.
    #[test]
    fn the_lamp_is_never_lit_outside_its_own_window() {
        for doy in [1, 60, 171, 265, 354, 365] {
            let (start, cutoff) = window(doy, S);
            for minute in 0..1440 {
                let lit = twilight_permille(minute, doy, S) > 0;
                if lit {
                    assert!(
                        is_evening(minute, doy, S),
                        "day {doy} minute {minute}: lit outside the window"
                    );
                }
                if !is_evening(minute, doy, S) {
                    assert_eq!(
                        twilight_permille(minute, doy, S),
                        0,
                        "day {doy} minute {minute}: brightness outside the window"
                    );
                }
            }
            // And the window is the span it claims to be, not a point.
            assert!(cutoff > start, "day {doy}: empty window {start}..{cutoff}");
            assert_eq!(twilight_permille(start, doy, S), 0);
            assert!(twilight_permille(start + 1, doy, S) > 0);
            assert!(twilight_permille(cutoff - 1, doy, S) > 0);
            assert_eq!(twilight_permille(cutoff, doy, S), 0);
        }
    }

    #[test]
    fn outside_the_window_every_step_is_dark() {
        for step in 0..BREATH.len() {
            assert_eq!(
                duty(step, 12 * 60, WINTER_DOY, FULL_CELL_MV, true, 255, S),
                0
            );
        }
    }

    #[test]
    fn home_assistant_moves_both_ends_of_the_evening() {
        let (_, sunset) = solar::sun(WINTER_DOY);
        let late = Schedule {
            start_after_sunset_min: 90,
            cutoff_minute_utc: 22 * 60,
        };
        assert_eq!(window(WINTER_DOY, late), (sunset + 90, 22 * 60));
        assert!(!is_evening(sunset + 60, WINTER_DOY, late));
        assert!(is_evening(sunset + 60, WINTER_DOY, S));
        assert!(is_evening(21 * 60 + 30, WINTER_DOY, late));
        assert!(!is_evening(21 * 60 + 30, WINTER_DOY, S));
    }

    #[test]
    fn a_cutoff_before_the_start_lights_nothing() {
        let (_, sunset) = solar::sun(WINTER_DOY);
        let empty = Schedule {
            start_after_sunset_min: 45,
            cutoff_minute_utc: sunset,
        };
        for minute in 0..1440 {
            assert!(!is_evening(minute, WINTER_DOY, empty));
            assert_eq!(twilight_permille(minute, WINTER_DOY, empty), 0);
        }
    }

    #[test]
    fn a_short_evening_never_jumps() {
        // Fade inside the ramp: no step between neighbouring minutes may be
        // larger than the steeper of the two slopes.
        let max_step = 1000 / FADE_OUT_MIN.min(RAMP_MIN);
        let (_, sunset) = solar::sun(171);
        for len in 1..=60 {
            let s = Schedule {
                start_after_sunset_min: 45,
                cutoff_minute_utc: sunset + 45 + len,
            };
            let mut last = 0;
            for minute in sunset..sunset + 120 {
                let p = twilight_permille(minute, 171, s);
                assert!(
                    p.abs_diff(last) <= max_step,
                    "len {len} minute {minute}: {last} -> {p}"
                );
                last = p;
            }
        }
    }
}

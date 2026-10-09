//! Discovery metadata for the HX711 load cell.
//!
//! The load cell is not a plain [`Sensor`](super::Sensor): its reading feeds the
//! tare baseline and the bird-presence edge detection in `main`, and the raw ->
//! grams conversion needs the runtime calibration from [`crate::config`]. So the
//! HX711 keeps its own path and only contributes its Home Assistant descriptor
//! here, so the discovery publisher (#16) can treat it like any other reading.

use core::fmt::Write as _;

use heapless::String;

use super::EntityDescriptor;

pub const DESCRIPTORS: &[EntityDescriptor] = &[
    EntityDescriptor {
        key: "weight",
        name: "Gewicht",
        unit: "g",
        device_class: "weight",
        state_class: "measurement",
    },
    // How long the load stayed on the cell. Only a visit produces one, so this
    // entity is stale between birds by design — it is the length of the *last*
    // visit, not a live value. `main` watches a visit through while awake
    // (see `crate::presence`), which is what makes the number better than the
    // deep-sleep interval it used to be quantised to.
    // The count, kept in RTC RAM and incremented at the arrival rather than at
    // the publish -- see `crate::state::count_visit` for why deriving it in
    // Home Assistant would undercount. `total_increasing` is what makes Home
    // Assistant keep it forever: statistics store a sum for that state class
    // and never purge it, while a `measurement` keeps only hourly mean/min/max
    // and the individual visits are gone with the 10-day raw history.
    //
    // Named for the birds rather than for the mechanism: the panel's Zuhause
    // screen has no device_class to go on here, so it picks the counter out by
    // name -- `vogel` or `vögel`, compared in lower case -- and reads `heute`
    // or `gesamt` from the rest (see `trmnl/README.md` in home-server).
    //
    // The umlaut is why that rule accepts two spellings. It used to test for
    // `Vogel` exactly, this name has been `Vögel gesamt` since the counter
    // existed, and Liquid's `contains` compares bytes: the panel showed no
    // count at all, silently, because a filter that keeps nothing looks like a
    // sensor that sends nothing. Renaming this entity is still free, but the
    // panel is the thing to check when it happens.
    EntityDescriptor {
        key: "visits",
        name: "Vögel gesamt",
        unit: "",
        device_class: "",
        state_class: "total_increasing",
    },
    EntityDescriptor {
        key: "visit",
        name: "Besuchsdauer",
        unit: "s",
        device_class: "duration",
        state_class: "measurement",
    },
];

/// Distinguishes a real visit count in RTC RAM from uninitialised memory.
///
/// A `#[ram(unstable(rtc_fast, persistent))]` word is never zeroed by the startup code,
/// so one that has just been *added to the firmware* comes up holding whatever
/// was in that slot. A cold-boot check does not cover it: a reflash preserves
/// RTC RAM (see docs/flashing.md), so the first boot on new firmware is not a cold
/// boot and the counter came up at 2 345 324 652 on the terrace node.
///
/// The count is therefore stored twice -- the value and this magic XORed with
/// it. Two independent words of leftover memory agreeing by chance is a 1 in
/// 2^32 event, and the pair repairs itself: an inconsistent one reads as zero,
/// and the next write makes it consistent again.
pub const VISITS_MAGIC: u32 = 0x5669_7369; // "Visi"

/// The companion word to store beside `count`.
pub const fn visits_check(count: u32) -> u32 {
    count ^ VISITS_MAGIC
}

/// What a stored pair means: the count, or zero if the two disagree.
pub const fn visits_from_pair(count: u32, check: u32) -> u32 {
    if check == visits_check(count) {
        count
    } else {
        0
    }
}

/// Format the visit counter for MQTT.
///
/// A bare integer. Home Assistant needs no unit to treat a `total_increasing`
/// number as a count, and giving it one ("Besuche") would put that word on
/// every axis and tooltip for nothing.
pub fn write_visits(buf: &mut String<16>, count: u32) {
    let _ = write!(buf, "{}", count);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_counter_is_total_increasing_so_statistics_keep_it() {
        let d = DESCRIPTORS
            .iter()
            .find(|d| d.key == "visits")
            .expect("the visit counter is announced");
        assert_eq!(d.state_class, "total_increasing");
        assert!(d.name.contains("Vögel"), "the panel matches on the name");
    }

    #[test]
    fn a_consistent_pair_reads_back_as_itself() {
        for n in [0u32, 1, 75, 4_294_967_295] {
            assert_eq!(visits_from_pair(n, visits_check(n)), n);
        }
    }

    #[test]
    fn leftover_rtc_memory_reads_as_zero() {
        // The value the terrace node actually came up with after the reflash
        // that introduced the counter, and a few other shapes of garbage.
        for (value, check) in [
            (2_345_324_652u32, 0u32),
            (2_345_324_652, 2_345_324_652),
            (0, 0xDEAD_BEEF),
            (0xFFFF_FFFF, 0xFFFF_FFFF),
            (12, 13),
        ] {
            assert_eq!(
                visits_from_pair(value, check),
                0,
                "value {value:#x} check {check:#x} must not be trusted"
            );
        }
    }

    #[test]
    fn the_magic_does_not_make_zero_look_valid_by_accident() {
        // A pair of zeroed words is the one shape leftover memory takes often,
        // so it must be rejected rather than read as "no visits yet".
        assert_ne!(visits_check(0), 0);
        assert_eq!(visits_from_pair(0, 0), 0);
    }

    #[test]
    fn a_count_formats_as_a_bare_integer() {
        for (n, expected) in [(0u32, "0"), (1, "1"), (75, "75"), (u32::MAX, "4294967295")] {
            let mut buf = String::new();
            write_visits(&mut buf, n);
            assert_eq!(buf.as_str(), expected);
        }
    }
}

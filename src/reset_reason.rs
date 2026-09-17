//! Why the node last restarted, published so the archive can answer it.
//!
//! The outdoor node has twice gone silent mid-cadence with a healthy cell and
//! come back only after power was removed entirely — see
//! [`docs/commissioning.md`]. Nothing in the telemetry could distinguish the
//! candidates, because the evidence destroys itself: the reset-cause register
//! describes *this* boot, so by the time anyone has pulled the cable to revive
//! the node, it reports the cable.
//!
//! Publishing it closes that. The three cases separate cleanly in the archive:
//!
//! * a **watchdog** cause appearing regularly means the node hangs often and
//!   usually recovers on its own, and the silences are the times it did not;
//! * a **brownout** cause means the supply collapses under radio load, which
//!   for a cell reading 3.7 V would mean an aged cell rather than a flat one;
//! * **nothing at all** across a silence means a true hang, with the watchdog
//!   not firing either — the case that needs a power cycle, and the one we have
//!   no explanation for yet.
//!
//! Reported as the raw `SocResetReason` discriminant rather than a name: the
//! MQTT archiver stores values as doubles, so a string would not survive the
//! trip. The codes are the hardware's own, listed below.

use core::fmt::Write as _;

use heapless::String;

use crate::sensors::EntityDescriptor;

/// Home Assistant discovery metadata. Neither entity has a unit or a device
/// class — like the visit count, they are bare numbers, and Home Assistant
/// validates `dev_cla` against its own list, so an absent key is how to say
/// "none" without the entity being rejected.
///
/// `reset_count` is `total_increasing` rather than `measurement`: it only ever
/// climbs, and saying so lets Home Assistant draw it as the step function it is
/// and survive the reset to zero that a power cycle brings.
pub const DESCRIPTORS: &[EntityDescriptor] = &[
    EntityDescriptor {
        key: "reset_reason",
        name: "Reset-Grund",
        unit: "",
        device_class: "",
        state_class: "measurement",
    },
    EntityDescriptor {
        key: "reset_count",
        name: "Unerwartete Resets",
        unit: "",
        device_class: "",
        state_class: "total_increasing",
    },
];

/// The codes worth recognising when reading the archive back. The full list is
/// `esp_hal`'s `SocResetReason`; these are the ones this node can plausibly
/// produce, and the reason the number is worth publishing at all.
///
/// | code | meaning |
/// | --- | --- |
/// | 0x01 | power on — the cell was disconnected, or the protection board cut |
/// | 0x03 | software reset of the digital core |
/// | 0x05 | deep-sleep wake — routine, never latched |
/// | 0x07 | main watchdog 0 — the app hung and was rebooted |
/// | 0x09 | RTC watchdog |
/// | 0x0F | **brownout** — the supply collapsed |
/// | 0x10 | RTC watchdog, core and RTC |
/// | 0x12 | super watchdog |
/// | 0x15 | USB UART reset — a host attached, e.g. `espflash` |
/// | 0x16 | USB JTAG reset |
pub const POWER_ON: u32 = 0x01;

/// Deep-sleep wake (`SocResetReason::CoreDeepSleep`). The expected cause on a
/// battery node, hundreds of times between publishes, and the one worth
/// filtering out; everything else is a report.
pub const DEEP_SLEEP: u32 = 0x05;

/// Marks the counter's RTC-RAM words as ours. `"REST"` in ASCII.
///
/// Without it the counter is nonsense, and this module shipped that way for one
/// evening: the first publish after flashing carried
/// `reset_count 3319124735`. RTC fast RAM is **not** zeroed when power is
/// removed — `state.rs` opens with that warning and the story behind it — so a
/// fresh region holds whatever was there before, and incrementing it produces a
/// large believable-looking number rather than an obvious error.
///
/// Guarding with a tag is the same shape `discovery_tag` uses: a value that
/// stays correct when the region is garbage, instead of a flag that assumes it
/// is not.
pub const EPOCH_TAG: u32 = 0x5245_5354;

/// Fold this boot's reset cause into what RTC RAM already holds, returning the
/// `(tag, count, latched)` to store back.
///
/// Pure so it can be tested on the host: `state.rs` needs the HAL for the
/// memory it lives in, but the decision does not.
pub const fn latch(tag: u32, count: u32, latched: u32, code: u32) -> (u32, u32, u32) {
    if tag != EPOCH_TAG {
        // Not ours. Whatever the words held is not a history, and this boot is
        // the first one that can be counted.
        return match code {
            DEEP_SLEEP => (EPOCH_TAG, 0, 0),
            _ => (EPOCH_TAG, 1, code),
        };
    }
    match code {
        DEEP_SLEEP => (tag, count, latched),
        _ => (tag, count.saturating_add(1), code),
    }
}

/// This boot's reset cause, as the raw discriminant. `None` when the hardware
/// reports something `esp_hal` does not recognise, which is reported as zero
/// rather than guessed at.
#[cfg(feature = "hal")]
pub fn code() -> u32 {
    esp_hal::reset::reset_reason().map_or(0, |reason| reason as u32)
}

/// Format a code for publishing: a bare integer, since it is an enumeration
/// rather than a measurement.
pub fn write_code(out: &mut String<16>, code: u32) {
    let _ = write!(out, "{code}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_untagged_region_is_treated_as_empty_rather_than_counted_on() {
        // The bug this guards: RTC RAM is not zeroed on power-up, so the count
        // started from garbage and the first publish read 3319124735.
        let garbage = 0xC5D3_1FFF;
        let (tag, count, latched) = latch(garbage, garbage, garbage, 0x0F);
        assert_eq!(tag, EPOCH_TAG);
        assert_eq!(count, 1, "the first countable boot, not garbage + 1");
        assert_eq!(latched, 0x0F);
    }

    #[test]
    fn an_untagged_region_woken_from_sleep_starts_at_zero() {
        let (tag, count, latched) = latch(0xDEAD_BEEF, 7, 9, DEEP_SLEEP);
        assert_eq!((tag, count, latched), (EPOCH_TAG, 0, 0));
    }

    #[test]
    fn deep_sleep_wakes_are_not_counted() {
        let before = (EPOCH_TAG, 4, 0x07);
        let after = latch(before.0, before.1, before.2, DEEP_SLEEP);
        assert_eq!(after, before, "the steady state must not move the counter");
    }

    #[test]
    fn anything_else_counts_and_replaces_the_latched_cause() {
        let (_, count, latched) = latch(EPOCH_TAG, 4, 0x07, 0x0F);
        assert_eq!((count, latched), (5, 0x0F));
    }

    #[test]
    fn a_code_is_published_as_a_bare_integer() {
        let mut s = String::<16>::new();
        write_code(&mut s, 0x0F);
        assert_eq!(s.as_str(), "15", "decimal, not hex — the archiver stores doubles");
    }

    #[test]
    fn the_two_entities_carry_no_unit_or_device_class() {
        // Home Assistant validates `dev_cla` against its own list and rejects an
        // empty string outright; the payload builder omits empty members, and
        // these two rely on that.
        for d in DESCRIPTORS {
            assert!(d.unit.is_empty(), "{} should have no unit", d.key);
            assert!(d.device_class.is_empty(), "{} should have no device class", d.key);
        }
    }
}

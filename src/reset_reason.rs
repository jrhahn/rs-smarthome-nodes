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

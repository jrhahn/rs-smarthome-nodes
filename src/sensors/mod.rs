//! Sensor abstraction for the configurable base platform.
//!
//! Scaffolding for the multi-sensor platform (epic #11, abstraction #12). The
//! goal is one firmware where each node enables the sensors it physically has
//! (force cell, DS18B20, SHT31-D, SCD41, SDS011) and the shared Wi-Fi / MQTT /
//! sleep machinery iterates over whatever is enabled.
//!
//! [`Sensor`] is deliberately **HAL-agnostic**: it knows nothing about esp-hal
//! peripherals, only how to take a measurement and describe its readings for
//! Home Assistant discovery (#16). The concrete drivers own their bus handle
//! (I²C / UART / 1-Wire / bit-bang) and are wired in per node.
//!
//! Drivers are generic over the `embedded-hal-async` / `embedded-io-async` bus
//! traits rather than over esp-hal types, so they can be exercised against any
//! implementation. `platform.rs` supplies the concrete ESP32-C3 buses (a shared
//! I²C bus for SHT31-D + SCD41, a UART for the SDS011) and decides which
//! drivers exist on this node (see `node.rs`).
#![allow(dead_code)]
// The trait uses `async fn`; we don't need `Send` futures on this single-core,
// single-executor target, so silence the auto-bound lint.
#![allow(async_fn_in_trait)]

use core::fmt::Write as _;

use heapless::{String, Vec};

#[cfg(all(test, feature = "drivers"))]
pub mod mock;
pub mod scale;
pub mod scd41;
pub mod sds011;
pub mod sht31;

/// Largest number of readings any one sensor emits per measurement. The widest
/// is a humidity-compensated SDS011: PM2.5 and PM10, each both corrected and
/// raw = 4 (see [`sds011::DESCRIPTORS_COMPENSATED`]). Sized generously.
pub const MAX_READINGS: usize = 6;

/// One published quantity, already formatted as a float-free ASCII value ready
/// for MQTT (matching the existing on-device formatting style).
pub struct Reading {
    /// Topic suffix / discovery key, e.g. `"co2"`, `"temperature"`, `"pm25"`.
    pub key: &'static str,
    /// Pre-formatted value, e.g. `"26.3"`, `"812"`.
    pub value: String<16>,
}

/// Static, per-reading metadata for Home Assistant MQTT auto-discovery (#16).
/// One entry per key a sensor can emit.
pub struct EntityDescriptor {
    /// Must match the [`Reading::key`] the sensor emits.
    pub key: &'static str,
    /// Human-friendly entity name suffix, e.g. `"CO₂"`.
    pub name: &'static str,
    /// MQTT `unit_of_measurement`, e.g. `"ppm"`, `"°C"`, `"µg/m³"`.
    pub unit: &'static str,
    /// HA `device_class`, e.g. `"carbon_dioxide"`, `"temperature"`, `"pm25"`.
    pub device_class: &'static str,
    /// HA `state_class`, almost always `"measurement"`.
    pub state_class: &'static str,
}

/// A pluggable sensor. Implementors own their bus handle and emit one or more
/// [`Reading`]s per [`Sensor::measure`].
pub trait Sensor {
    /// Short sensor-kind name for logs, e.g. `"SCD41"`.
    fn kind(&self) -> &'static str;

    /// Discovery metadata for every reading this sensor can emit. Order is
    /// irrelevant; keys must line up with [`Reading::key`].
    fn descriptors(&self) -> &'static [EntityDescriptor];

    /// Take one measurement. Returns the readings, or an **empty** `Vec` if the
    /// sensor did not respond — a missing/faulty sensor is logged by the caller
    /// and simply omitted from the publish, never fatal (same contract as the
    /// existing DS18B20 path).
    async fn measure(&mut self) -> Vec<Reading, MAX_READINGS>;

    /// Why the last [`Self::measure`] came back empty, in terms someone holding
    /// the board can act on.
    ///
    /// "Not responding" covers a wide spread of causes — a sensor that never
    /// acknowledged anything, one that acknowledges commands but refuses to
    /// start, and one that answers with corrupt data all look identical from
    /// outside, and they have completely different fixes. A driver that can
    /// tell them apart says so here; the default keeps the coarse message for
    /// the ones that cannot.
    fn fault(&self) -> Option<&'static str> {
        None
    }
}

// --- Shared helpers ---------------------------------------------------------

/// Sensirion CRC-8 (poly 0x31, init 0xFF, no reflection / final xor). Used by
/// both I²C sensors (SHT31-D, SCD41) to validate each 2-byte word.
pub const fn crc8_sensirion(data: &[u8]) -> u8 {
    let mut crc: u8 = 0xFF;
    let mut i = 0;
    while i < data.len() {
        crc ^= data[i];
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x31
            } else {
                crc << 1
            };
            bit += 1;
        }
        i += 1;
    }
    crc
}

/// Decode one Sensirion data word: `MSB LSB CRC`. Returns `None` when the CRC
/// does not match, so a noisy bus is dropped rather than published.
pub fn crc_word(chunk: &[u8]) -> Option<u16> {
    let [msb, lsb, crc] = chunk else { return None };
    if crc8_sensirion(&[*msb, *lsb]) != *crc {
        return None;
    }
    Some(u16::from_be_bytes([*msb, *lsb]))
}

/// Format a value given in **tenths** as a one-decimal ASCII string
/// (`-4` -> `"-0.4"`, `263` -> `"26.3"`), float-free. Mirrors the fixed-point
/// formatting used in `config::write_grams` / `ds18b20::write_temp_c`.
pub fn write_tenths(buf: &mut String<16>, tenths: i32) {
    if tenths < 0 {
        let _ = buf.push('-');
    }
    let mag = tenths.unsigned_abs();
    let _ = write!(buf, "{}.{}", mag / 10, mag % 10);
}

/// Format a plain integer value (e.g. CO₂ ppm, PM µg/m³ if whole).
pub fn write_int(buf: &mut String<16>, value: i32) {
    let _ = write!(buf, "{}", value);
}

// Pin the CRC against the datasheet check vector at compile time (same tactic
// as the existing sensor drivers, since the host test harness can't link this
// crate). Sensirion datasheets give CRC(0xBEEF) == 0x92.
const _: () = assert!(crc8_sensirion(&[0xBE, 0xEF]) == 0x92);

// --- Bus diagnostics ---------------------------------------------------------

/// Lowest and highest 7-bit addresses that can belong to a device. Everything
/// below `0x08` and above `0x77` is reserved by the I²C specification.
pub const SCAN_FIRST: u8 = 0x08;
pub const SCAN_LAST: u8 = 0x77;
/// Most devices worth reporting from one sweep. Two is the realistic number
/// here; the headroom is for a bus that turns out to have more on it than
/// anyone expected, which is exactly the case worth seeing.
pub const MAX_FOUND: usize = 8;

/// Sweep the bus and return every address that answers.
///
/// This exists to settle the one question the targeted probes cannot: a missing
/// sensor and a dead bus both read as "not responding". If the sweep finds
/// *something*, the wiring and the pull-ups are fine and the device is simply
/// not where it was expected; if it finds nothing, the bus itself is the
/// problem.
///
/// The probe is a zero-length write, which puts the address on the bus and
/// looks at the acknowledgement without transferring data — the same thing an
/// `i2cdetect` does, and side-effect-free on any device that is not there.
///
/// Worth ~110 transactions, so callers run it only when something is already
/// wrong.
#[cfg(feature = "drivers")]
pub async fn scan_bus<I: embedded_hal_async::i2c::I2c>(bus: &mut I) -> Vec<u8, MAX_FOUND> {
    let mut found = Vec::new();
    for addr in SCAN_FIRST..=SCAN_LAST {
        if bus.write(addr, &[]).await.is_ok() && found.push(addr).is_err() {
            // More devices than we can report says everything the caller needs
            // to know already.
            break;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensirion_crc_matches_the_datasheet_vector() {
        assert_eq!(crc8_sensirion(&[0xBE, 0xEF]), 0x92);
        assert_eq!(crc8_sensirion(&[0x00, 0x00]), 0x81);
        assert_eq!(crc8_sensirion(&[0xFF, 0xFF]), 0xAC);
    }

    #[test]
    fn a_word_decodes_only_with_its_own_crc() {
        assert_eq!(crc_word(&[0xBE, 0xEF, 0x92]), Some(0xBEEF));
        // Every single-bit error in the word or its CRC must be caught — this
        // is all that stands between bus noise and a published reading.
        for byte in 0..3 {
            for bit in 0..8 {
                let mut chunk = [0xBE, 0xEF, 0x92];
                chunk[byte] ^= 1 << bit;
                assert!(
                    crc_word(&chunk).is_none(),
                    "bit {bit} of byte {byte} accepted"
                );
            }
        }
    }

    #[test]
    fn a_short_chunk_is_rejected_rather_than_panicking() {
        // A truncated I²C read must not index out of bounds.
        assert!(crc_word(&[]).is_none());
        assert!(crc_word(&[0xBE]).is_none());
        assert!(crc_word(&[0xBE, 0xEF]).is_none());
        assert!(crc_word(&[0xBE, 0xEF, 0x92, 0x00]).is_none());
    }

    #[test]
    fn tenths_format_as_one_decimal() {
        for (tenths, expected) in [
            (0, "0.0"),
            (5, "0.5"),
            (263, "26.3"),
            (-4, "-0.4"),
            (-263, "-26.3"),
            (1000, "100.0"),
        ] {
            let mut buf = String::new();
            write_tenths(&mut buf, tenths);
            assert_eq!(buf.as_str(), expected);
        }
    }

    #[test]
    fn a_negative_value_below_one_keeps_its_sign() {
        // `-0.4` would print as `0.4` if the sign were taken from the integer
        // part alone, which is zero.
        let mut buf = String::new();
        write_tenths(&mut buf, -4);
        assert!(buf.starts_with('-'));
    }

    #[test]
    fn integers_format_without_a_decimal_point() {
        for (value, expected) in [(0, "0"), (812, "812"), (-5, "-5")] {
            let mut buf = String::new();
            write_int(&mut buf, value);
            assert_eq!(buf.as_str(), expected);
        }
    }

    #[test]
    fn no_descriptor_set_exceeds_what_a_measurement_can_carry() {
        // A sensor emitting more readings than `MAX_READINGS` would silently
        // drop the tail when pushed into the heapless Vec.
        for descriptors in [
            scale::DESCRIPTORS,
            sht31::DESCRIPTORS,
            scd41::DESCRIPTORS,
            sds011::DESCRIPTORS,
            sds011::DESCRIPTORS_COMPENSATED,
            crate::ds18b20::DESCRIPTORS,
            crate::battery::DESCRIPTORS,
        ] {
            assert!(descriptors.len() <= MAX_READINGS);
            assert!(!descriptors.is_empty());
        }
    }

    #[test]
    fn descriptor_keys_are_unique_within_a_sensor() {
        for descriptors in [
            sht31::DESCRIPTORS,
            scd41::DESCRIPTORS,
            sds011::DESCRIPTORS,
            sds011::DESCRIPTORS_COMPENSATED,
        ] {
            for (i, a) in descriptors.iter().enumerate() {
                for b in &descriptors[i + 1..] {
                    assert_ne!(a.key, b.key);
                }
            }
        }
    }

    #[test]
    fn every_descriptor_is_complete() {
        // An empty device_class or unit produces a valid-looking but useless
        // Home Assistant entity.
        for descriptors in [
            scale::DESCRIPTORS,
            sht31::DESCRIPTORS,
            scd41::DESCRIPTORS,
            sds011::DESCRIPTORS,
            sds011::DESCRIPTORS_COMPENSATED,
            crate::ds18b20::DESCRIPTORS,
            crate::battery::DESCRIPTORS,
        ] {
            for d in descriptors {
                assert!(!d.key.is_empty() && !d.name.is_empty());
                assert!(!d.unit.is_empty() && !d.device_class.is_empty());
                assert_eq!(d.state_class, "measurement");
                // Keys end up in MQTT topics. Underscore is allowed *within* a
                // key — a slot prefix already puts one there (`scd41_`), so the
                // topics contain them either way — but never at an end, which
                // would produce a doubled or trailing separator.
                assert!(d
                    .key
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'));
                assert!(!d.key.starts_with('_') && !d.key.ends_with('_'));
            }
        }
    }

    // --- Bus scan -----------------------------------------------------------

    #[cfg(feature = "drivers")]
    #[test]
    fn the_scan_reports_every_address_that_answers() {
        use super::mock::{block_on, FakeI2c};

        // The realistic case: both I²C sensors of the outdoor node.
        let mut bus = FakeI2c::with_devices([sht31::ADDR, scd41::ADDR]);
        let found = block_on(scan_bus(&mut bus));
        assert_eq!(found.as_slice(), &[sht31::ADDR, scd41::ADDR]);
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn a_dead_bus_answers_nowhere() {
        // Nothing answering anywhere is the signal that the bus itself is the
        // problem — wiring, pull-ups or power — rather than one absent sensor.
        use super::mock::{block_on, FakeI2c};

        let mut bus = FakeI2c::empty();
        assert!(block_on(scan_bus(&mut bus)).is_empty());
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn a_device_at_an_unexpected_address_is_still_found() {
        // The case that makes the scan worth its ~110 transactions: the bus is
        // healthy and the breakout is simply somewhere else.
        use super::mock::{block_on, FakeI2c};

        let mut bus = FakeI2c::with_devices([0x76]); // e.g. a BME280
        assert_eq!(block_on(scan_bus(&mut bus)).as_slice(), &[0x76]);
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn the_scan_stays_inside_the_addressable_range() {
        // Addressing a reserved range is not merely useless: 0x00 is the
        // general-call address, which every device on the bus listens to.
        use super::mock::{block_on, FakeI2c};

        let mut bus = FakeI2c::empty();
        let _ = block_on(scan_bus(&mut bus));
        let addressed = bus.addressed();
        assert_eq!(addressed.len(), (SCAN_LAST - SCAN_FIRST + 1) as usize);
        assert!(addressed
            .iter()
            .all(|a| (SCAN_FIRST..=SCAN_LAST).contains(a)));
        assert_eq!(addressed.first(), Some(&SCAN_FIRST));
        assert_eq!(addressed.last(), Some(&SCAN_LAST));
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn a_crowded_bus_does_not_overrun_the_report() {
        // A bus where everything answers means SDA is stuck low, which is worth
        // surviving rather than panicking on.
        use super::mock::{block_on, FakeI2c};

        let mut bus = FakeI2c::with_devices(SCAN_FIRST..=SCAN_LAST);
        let found = block_on(scan_bus(&mut bus));
        assert_eq!(found.len(), MAX_FOUND);
    }
}

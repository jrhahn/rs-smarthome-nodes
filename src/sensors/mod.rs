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

/// Largest number of readings any one sensor emits per measurement (SCD41 emits
/// CO₂ + temperature + humidity = 3). Sized generously.
pub const MAX_READINGS: usize = 4;

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
            crate::ds18b20::DESCRIPTORS,
        ] {
            assert!(descriptors.len() <= MAX_READINGS);
            assert!(!descriptors.is_empty());
        }
    }

    #[test]
    fn descriptor_keys_are_unique_within_a_sensor() {
        for descriptors in [sht31::DESCRIPTORS, scd41::DESCRIPTORS, sds011::DESCRIPTORS] {
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
            crate::ds18b20::DESCRIPTORS,
        ] {
            for d in descriptors {
                assert!(!d.key.is_empty() && !d.name.is_empty());
                assert!(!d.unit.is_empty() && !d.device_class.is_empty());
                assert_eq!(d.state_class, "measurement");
                // Keys end up in MQTT topics.
                assert!(d
                    .key
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
            }
        }
    }
}

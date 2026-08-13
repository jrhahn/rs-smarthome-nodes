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
//! Status: type + trait + datasheet constants are in place and compile-checked;
//! the actual bus I/O in each driver's `measure()` is a `todo!()` to fill in
//! once the hardware is on the bench. `main.rs` does **not** use this yet — the
//! working bird-scale path is untouched.
#![allow(dead_code)]
// The trait uses `async fn`; we don't need `Send` futures on this single-core,
// single-executor target, so silence the auto-bound lint.
#![allow(async_fn_in_trait)]

use core::fmt::Write as _;

use heapless::{String, Vec};

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

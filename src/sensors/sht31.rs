//! SHT31-D temperature + humidity driver (I²C). Scaffolding for #13.
//!
//! Bus: I²C, address **0x44** (ADDR pin low) or 0x45 (high).
//! Single-shot, high repeatability, clock-stretching **disabled**: write command
//! `0x2400`, wait ~15 ms, then read 6 bytes:
//!   `T_MSB T_LSB CRC   RH_MSB RH_LSB CRC`
//! each 16-bit word guarded by [`crc8_sensirion`].
//!
//! Conversions (fixed-point, one decimal — see [`write_tenths`]):
//!   T[°C]  = -45 + 175 · raw / 65535
//!   RH[%]  = 100 · raw / 65535

use heapless::{String, Vec};

use super::{write_tenths, EntityDescriptor, Reading, Sensor, MAX_READINGS};

/// I²C address with ADDR tied low (the common breakout default).
pub const ADDR: u8 = 0x44;
/// Measure, single-shot, high repeatability, clock-stretch off.
pub const CMD_SINGLE_HIGH: u16 = 0x2400;
/// Datasheet conversion time for high repeatability (ms); wait before reading.
pub const CONVERSION_MS: u64 = 15;

const DESCRIPTORS: &[EntityDescriptor] = &[
    EntityDescriptor {
        key: "temperature",
        name: "Temperatur",
        unit: "°C",
        device_class: "temperature",
        state_class: "measurement",
    },
    EntityDescriptor {
        key: "humidity",
        name: "Luftfeuchte",
        unit: "%",
        device_class: "humidity",
        state_class: "measurement",
    },
];

/// Raw 16-bit sample -> temperature in tenths of °C.
pub const fn temp_tenths(raw: u16) -> i32 {
    (1750 * raw as i32) / 65535 - 450
}

/// Raw 16-bit sample -> relative humidity in tenths of %.
pub const fn rh_tenths(raw: u16) -> i32 {
    (1000 * raw as i32) / 65535
}

/// SHT31-D on an I²C bus. TODO(#13): hold the bus handle (`I2c<'_, ...>`) here;
/// zero-field placeholder for now so the abstraction compiles.
pub struct Sht31;

impl Sht31 {
    pub fn new() -> Self {
        Self
    }

    fn push(readings: &mut Vec<Reading, MAX_READINGS>, key: &'static str, tenths: i32) {
        let mut value = String::new();
        write_tenths(&mut value, tenths);
        let _ = readings.push(Reading { key, value });
    }
}

impl Default for Sht31 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sensor for Sht31 {
    fn kind(&self) -> &'static str {
        "SHT31-D"
    }

    fn descriptors(&self) -> &'static [EntityDescriptor] {
        DESCRIPTORS
    }

    async fn measure(&mut self) -> Vec<Reading, MAX_READINGS> {
        // TODO(#13): write CMD_SINGLE_HIGH, Timer::after(CONVERSION_MS), read 6
        // bytes, verify both CRCs (crc8_sensirion over each word), then:
        //   Self::push(&mut out, "temperature", temp_tenths(t_raw));
        //   Self::push(&mut out, "humidity",    rh_tenths(rh_raw));
        // On any bus/CRC error or absent sensor, return an empty Vec.
        todo!("SHT31-D I²C read (#13)")
    }
}

// Compile-time anchor of the conversion at the datasheet endpoints.
const _: () = {
    assert!(temp_tenths(0) == -450); // 0x0000 -> -45.0 °C
    assert!(temp_tenths(65535) == 1300); // 0xFFFF -> 130.0 °C
    assert!(rh_tenths(0) == 0);
    assert!(rh_tenths(65535) == 1000); // -> 100.0 %
};

//! SHT31-D temperature + humidity driver (I²C, #13).
//!
//! Bus: I²C, address **0x44** (ADDR pin low) or 0x45 (high).
//! Single-shot, high repeatability, clock-stretching **disabled**: write command
//! `0x2400`, wait ~15 ms, then read 6 bytes:
//!   `T_MSB T_LSB CRC   RH_MSB RH_LSB CRC`
//! each 16-bit word guarded by [`crc8_sensirion`](super::crc8_sensirion).
//!
//! Conversions (fixed-point, one decimal — see [`write_tenths`]):
//!   T[°C]  = -45 + 175 · raw / 65535
//!   RH[%]  = 100 · raw / 65535
//!
//! Clock stretching is left off deliberately: with it enabled the sensor holds
//! SCL for the whole conversion, which would block the shared bus (and the
//! SCD41 sitting on it) for 15 ms inside one transaction.

use embassy_time::{Duration, Timer};
use embedded_hal_async::i2c::I2c as I2cBus;
use heapless::{String, Vec};

use super::{crc_word, write_tenths, EntityDescriptor, Reading, Sensor, MAX_READINGS};

/// I²C address with ADDR tied low (the common breakout default).
pub const ADDR: u8 = 0x44;
/// I²C address with ADDR tied high.
pub const ADDR_ALT: u8 = 0x45;
/// Measure, single-shot, high repeatability, clock-stretch off.
pub const CMD_SINGLE_HIGH: u16 = 0x2400;
/// Soft reset; clears a sensor left in a weird state by a hot restart.
pub const CMD_SOFT_RESET: u16 = 0x30A2;
/// Datasheet conversion time for high repeatability (ms); wait before reading.
pub const CONVERSION_MS: u64 = 15;

pub const DESCRIPTORS: &[EntityDescriptor] = &[
    EntityDescriptor {
        key: "temperature",
        name: "Temperatur",
        unit: "°C",
        device_class: "temperature",
        state_class: "measurement",
    },
    EntityDescriptor {
        key: "humidity",
        name: "Feuchte",
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

/// SHT31-D on an I²C bus. Generic over the bus so the driver stays HAL-agnostic;
/// `platform.rs` hands it a shared-bus handle it can own.
pub struct Sht31<I2C> {
    i2c: I2C,
    addr: u8,
}

impl<I2C: I2cBus> Sht31<I2C> {
    /// Driver at the default address (0x44, ADDR low).
    pub fn new(i2c: I2C) -> Self {
        Self { i2c, addr: ADDR }
    }

    /// Driver at an explicit address, for a board that strapped ADDR high.
    pub fn with_address(i2c: I2C, addr: u8) -> Self {
        Self { i2c, addr }
    }

    /// One single-shot conversion -> `(temperature_raw, humidity_raw)`, or
    /// `None` if the sensor did not ACK or a word failed its CRC.
    async fn sample(&mut self) -> Option<(u16, u16)> {
        self.i2c
            .write(self.addr, &CMD_SINGLE_HIGH.to_be_bytes())
            .await
            .ok()?;
        Timer::after(Duration::from_millis(CONVERSION_MS)).await;

        let mut buf = [0u8; 6];
        self.i2c.read(self.addr, &mut buf).await.ok()?;
        Some((crc_word(&buf[0..3])?, crc_word(&buf[3..6])?))
    }

    fn push(readings: &mut Vec<Reading, MAX_READINGS>, key: &'static str, tenths: i32) {
        let mut value = String::new();
        write_tenths(&mut value, tenths);
        let _ = readings.push(Reading { key, value });
    }
}

impl<I2C: I2cBus> Sensor for Sht31<I2C> {
    fn kind(&self) -> &'static str {
        "SHT31-D"
    }

    fn descriptors(&self) -> &'static [EntityDescriptor] {
        DESCRIPTORS
    }

    async fn measure(&mut self) -> Vec<Reading, MAX_READINGS> {
        let mut out = Vec::new();
        // A missing or mis-wired sensor simply contributes nothing; the caller
        // logs it and publishes whatever else answered.
        if let Some((t_raw, rh_raw)) = self.sample().await {
            Self::push(&mut out, "temperature", temp_tenths(t_raw));
            Self::push(&mut out, "humidity", rh_tenths(rh_raw));
        }
        out
    }
}

// Compile-time anchor of the conversion at the datasheet endpoints.
const _: () = {
    assert!(temp_tenths(0) == -450); // 0x0000 -> -45.0 °C
    assert!(temp_tenths(65535) == 1300); // 0xFFFF -> 130.0 °C
    assert!(rh_tenths(0) == 0);
    assert!(rh_tenths(65535) == 1000); // -> 100.0 %
};

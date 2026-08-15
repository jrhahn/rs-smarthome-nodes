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

#[cfg(feature = "hal")]
use embassy_time::{Duration, Timer};
#[cfg(feature = "hal")]
use embedded_hal_async::i2c::I2c as I2cBus;
#[cfg(feature = "hal")]
use heapless::{String, Vec};

use super::EntityDescriptor;
#[cfg(feature = "hal")]
use super::{crc_word, write_tenths, Reading, Sensor, MAX_READINGS};

/// I²C address with ADDR tied low (the common breakout default).
pub const ADDR: u8 = 0x44;
/// I²C address with ADDR tied high.
pub const ADDR_ALT: u8 = 0x45;
/// Measure, single-shot, high repeatability, clock-stretch off.
pub const CMD_SINGLE_HIGH: u16 = 0x2400;
/// Soft reset; clears a sensor left in a weird state by a hot restart.
pub const CMD_SOFT_RESET: u16 = 0x30A2;
/// Read the status register. Side-effect-free, so it doubles as the "are you
/// there?" probe when working out which address the breakout is strapped to.
pub const CMD_READ_STATUS: u16 = 0xF32D;
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
#[cfg(feature = "hal")]
pub struct Sht31<I2C> {
    i2c: I2C,
    addr: u8,
}

#[cfg(feature = "hal")]
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

#[cfg(feature = "hal")]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions_hit_the_datasheet_endpoints() {
        assert_eq!(temp_tenths(0), -450); // -45.0 °C
        assert_eq!(temp_tenths(65535), 1300); // +130.0 °C
        assert_eq!(rh_tenths(0), 0);
        assert_eq!(rh_tenths(65535), 1000); // 100.0 %
    }

    #[test]
    fn conversions_are_monotonic_across_the_range() {
        // Integer maths with a division; a lost fraction would show up as a
        // step backwards somewhere in the middle.
        let mut previous = temp_tenths(0);
        for raw in (0..=65535u32).step_by(97) {
            let value = temp_tenths(raw as u16);
            assert!(value >= previous, "temperature dipped at raw {raw}");
            previous = value;
        }
    }

    #[test]
    fn room_temperature_lands_where_it_should() {
        // ~21.4 °C and ~50 % RH, the values a bench check should produce.
        assert_eq!(temp_tenths(24_900), 214);
        assert_eq!(rh_tenths(32_768), 500);
    }

    #[test]
    fn the_probe_command_is_side_effect_free() {
        // `platform`'s bus scan writes this to work out which address the
        // breakout is strapped to, so it must not start a conversion.
        assert_eq!(CMD_READ_STATUS, 0xF32D);
        assert_ne!(CMD_READ_STATUS, CMD_SINGLE_HIGH);
        assert_ne!(ADDR, ADDR_ALT);
    }
}

//! SCD41 CO₂ + temperature + humidity driver (I²C, #14).
//!
//! Bus: I²C, address **0x62**. Command words are big-endian; data words are
//! `MSB LSB CRC` guarded by [`crc8_sensirion`](super::crc8_sensirion).
//!
//! Two run modes:
//!   * **periodic** (mains): `start_periodic_measurement` (0x21B1), then read
//!     `read_measurement` (0xEC05) every ≥5 s. The first sample after the start
//!     command is only ready after ~5 s, so the first `measure()` on a cold node
//!     returns nothing and the next one has data.
//!   * **single-shot** (battery): `measure_single_shot` (0x219D), wait ~5 s,
//!     then `read_measurement`. Note: automatic self-calibration (ASC) assumes
//!     the sensor periodically sees fresh (~400 ppm) air.
//!
//! `read_measurement` returns 9 bytes: CO₂(2+CRC), T(2+CRC), RH(2+CRC).
//!
//! Conversions:
//!   CO₂[ppm] = raw
//!   T[°C]    = -45 + 175 · raw / 65535
//!   RH[%]    = 100 · raw / 65535

use embassy_time::{Duration, Timer};
use embedded_hal_async::i2c::I2c as I2cBus;
use heapless::{String, Vec};

use super::{crc_word, write_int, write_tenths, EntityDescriptor, Reading, Sensor, MAX_READINGS};

pub const ADDR: u8 = 0x62;
pub const CMD_START_PERIODIC: u16 = 0x21B1;
pub const CMD_START_LOW_POWER_PERIODIC: u16 = 0x21AC;
pub const CMD_READ_MEASUREMENT: u16 = 0xEC05;
pub const CMD_MEASURE_SINGLE_SHOT: u16 = 0x219D;
pub const CMD_STOP_PERIODIC: u16 = 0x3F86;
pub const CMD_GET_DATA_READY: u16 = 0xE4B8;
/// Single-shot conversion time (ms) before `read_measurement`.
pub const SINGLE_SHOT_MS: u64 = 5000;
/// Datasheet execution time for the short commands (ms).
pub const CMD_DELAY_MS: u64 = 2;

pub const DESCRIPTORS: &[EntityDescriptor] = &[
    EntityDescriptor {
        key: "co2",
        name: "CO₂",
        unit: "ppm",
        device_class: "carbon_dioxide",
        state_class: "measurement",
    },
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

/// CO₂ raw word is already ppm.
pub const fn co2_ppm(raw: u16) -> i32 {
    raw as i32
}

/// Raw 16-bit sample -> temperature in tenths of °C (same transfer fn as SHT31).
pub const fn temp_tenths(raw: u16) -> i32 {
    (1750 * raw as i32) / 65535 - 450
}

/// Raw 16-bit sample -> relative humidity in tenths of %.
pub const fn rh_tenths(raw: u16) -> i32 {
    (1000 * raw as i32) / 65535
}

/// A data-ready reply carries the status in the low 11 bits; all-zero means
/// "no measurement pending".
pub const fn data_ready(status: u16) -> bool {
    status & 0x07FF != 0
}

/// Whether this node drives the SCD41 in single-shot mode (battery) or
/// continuous periodic mode (mains). Ties into the power-profile work (#17).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Periodic,
    SingleShot,
}

/// SCD41 on an I²C bus, generic over the bus (see [`super::sht31::Sht31`]).
pub struct Scd41<I2C> {
    i2c: I2C,
    pub mode: Mode,
    /// Periodic mode only: whether `start_periodic_measurement` has been sent
    /// this boot. A mains node stays powered, so this happens once.
    started: bool,
}

impl<I2C: I2cBus> Scd41<I2C> {
    pub fn new(i2c: I2C, mode: Mode) -> Self {
        Self {
            i2c,
            mode,
            started: false,
        }
    }

    /// Send a bare command word and give the sensor its execution time.
    async fn command(&mut self, cmd: u16) -> Option<()> {
        self.i2c.write(ADDR, &cmd.to_be_bytes()).await.ok()?;
        Timer::after(Duration::from_millis(CMD_DELAY_MS)).await;
        Some(())
    }

    /// Stop a periodic measurement (needed before most config commands, and to
    /// leave the sensor idle). Takes 500 ms per the datasheet.
    pub async fn stop_periodic(&mut self) {
        if self.command(CMD_STOP_PERIODIC).await.is_some() {
            Timer::after(Duration::from_millis(500)).await;
        }
        self.started = false;
    }

    /// `get_data_ready_status` — cheap poll so periodic mode never reads a
    /// stale/unfinished measurement.
    async fn ready(&mut self) -> Option<bool> {
        self.command(CMD_GET_DATA_READY).await?;
        let mut buf = [0u8; 3];
        self.i2c.read(ADDR, &mut buf).await.ok()?;
        Some(data_ready(crc_word(&buf)?))
    }

    /// `read_measurement` -> `(co2_raw, t_raw, rh_raw)`.
    async fn read_measurement(&mut self) -> Option<(u16, u16, u16)> {
        self.command(CMD_READ_MEASUREMENT).await?;
        let mut buf = [0u8; 9];
        self.i2c.read(ADDR, &mut buf).await.ok()?;
        Some((
            crc_word(&buf[0..3])?,
            crc_word(&buf[3..6])?,
            crc_word(&buf[6..9])?,
        ))
    }

    /// One sample according to the configured mode, or `None` when the sensor is
    /// absent, still busy, or answered with a bad CRC.
    async fn sample(&mut self) -> Option<(u16, u16, u16)> {
        match self.mode {
            Mode::Periodic => {
                if !self.started {
                    self.command(CMD_START_PERIODIC).await?;
                    self.started = true;
                    // The first conversion needs ~5 s; report nothing this round
                    // rather than blocking the publish path.
                    return None;
                }
                if !self.ready().await? {
                    return None;
                }
                self.read_measurement().await
            }
            Mode::SingleShot => {
                self.command(CMD_MEASURE_SINGLE_SHOT).await?;
                Timer::after(Duration::from_millis(SINGLE_SHOT_MS)).await;
                self.read_measurement().await
            }
        }
    }

    fn push_tenths(readings: &mut Vec<Reading, MAX_READINGS>, key: &'static str, tenths: i32) {
        let mut value = String::new();
        write_tenths(&mut value, tenths);
        let _ = readings.push(Reading { key, value });
    }

    fn push_int(readings: &mut Vec<Reading, MAX_READINGS>, key: &'static str, v: i32) {
        let mut value = String::new();
        write_int(&mut value, v);
        let _ = readings.push(Reading { key, value });
    }
}

impl<I2C: I2cBus> Sensor for Scd41<I2C> {
    fn kind(&self) -> &'static str {
        "SCD41"
    }

    fn descriptors(&self) -> &'static [EntityDescriptor] {
        DESCRIPTORS
    }

    async fn measure(&mut self) -> Vec<Reading, MAX_READINGS> {
        let mut out = Vec::new();
        let Some((co2_raw, t_raw, rh_raw)) = self.sample().await else {
            return out;
        };
        // 0 ppm is the sensor's "no valid measurement yet" answer, not air.
        if co2_raw == 0 {
            return out;
        }
        Self::push_int(&mut out, "co2", co2_ppm(co2_raw));
        Self::push_tenths(&mut out, "temperature", temp_tenths(t_raw));
        Self::push_tenths(&mut out, "humidity", rh_tenths(rh_raw));
        out
    }
}

const _: () = {
    assert!(co2_ppm(812) == 812);
    assert!(temp_tenths(0) == -450);
    assert!(rh_tenths(65535) == 1000);
    assert!(!data_ready(0x8000)); // only the reserved high bits set -> not ready
    assert!(data_ready(0x8001));
};

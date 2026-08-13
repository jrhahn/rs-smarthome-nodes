//! SCD41 CO₂ + temperature + humidity driver (I²C). Scaffolding for #14.
//!
//! Bus: I²C, address **0x62**. Command words are big-endian; data words are
//! `MSB LSB CRC` guarded by [`crc8_sensirion`].
//!
//! Two run modes:
//!   * **periodic** (mains): `start_periodic_measurement` (0x21B1), then read
//!     `read_measurement` (0xEC05) every ≥5 s.
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

use heapless::{String, Vec};

use super::{write_int, write_tenths, EntityDescriptor, Reading, Sensor, MAX_READINGS};

pub const ADDR: u8 = 0x62;
pub const CMD_START_PERIODIC: u16 = 0x21B1;
pub const CMD_START_LOW_POWER_PERIODIC: u16 = 0x21AC;
pub const CMD_READ_MEASUREMENT: u16 = 0xEC05;
pub const CMD_MEASURE_SINGLE_SHOT: u16 = 0x219D;
pub const CMD_STOP_PERIODIC: u16 = 0x3F86;
/// Single-shot conversion time (ms) before `read_measurement`.
pub const SINGLE_SHOT_MS: u64 = 5000;

const DESCRIPTORS: &[EntityDescriptor] = &[
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
        name: "Luftfeuchte",
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

/// Whether this node drives the SCD41 in low-power/single-shot mode (battery) or
/// continuous periodic mode (mains). Ties into the power-profile work (#17).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Periodic,
    SingleShot,
}

/// SCD41 on an I²C bus. TODO(#14): hold the bus handle; zero-field placeholder.
pub struct Scd41 {
    pub mode: Mode,
}

impl Scd41 {
    pub fn new(mode: Mode) -> Self {
        Self { mode }
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

impl Sensor for Scd41 {
    fn kind(&self) -> &'static str {
        "SCD41"
    }

    fn descriptors(&self) -> &'static [EntityDescriptor] {
        DESCRIPTORS
    }

    async fn measure(&mut self) -> Vec<Reading, MAX_READINGS> {
        // TODO(#14): per `self.mode`, either kick a single shot + wait
        // SINGLE_SHOT_MS, or read the running periodic measurement; parse the
        // 9-byte reply (3× CRC-checked words), then:
        //   Self::push_int(&mut out, "co2", co2_ppm(co2_raw));
        //   Self::push_tenths(&mut out, "temperature", temp_tenths(t_raw));
        //   Self::push_tenths(&mut out, "humidity",    rh_tenths(rh_raw));
        // Empty Vec on absence / CRC error.
        todo!("SCD41 I²C read (#14)")
    }
}

const _: () = {
    assert!(co2_ppm(812) == 812);
    assert!(temp_tenths(0) == -450);
    assert!(rh_tenths(65535) == 1000);
};

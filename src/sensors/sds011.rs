//! SDS011 particulate-matter driver (UART). Scaffolding for #15.
//!
//! Bus: UART **9600 8N1**, 3.3 V TTL logic (the sensor itself is powered from
//! 5 V; its TX is already 3.3 V-safe for the ESP32-C3 RX).
//!
//! Measurement frame (10 bytes), reported once per second in active mode:
//!   `AA C0 PM25_L PM25_H PM10_L PM10_H ID1 ID2 CHK AB`
//! where `CHK = (sum of bytes[2..=7]) & 0xFF` and:
//!   PM2.5[µg/m³] = (PM25_H·256 + PM25_L) / 10
//!   PM10 [µg/m³] = (PM10_H·256 + PM10_L) / 10
//!
//! **Fan duty-cycling (important):** the laser + fan are rated ~8000 h, so we
//! must not run them continuously. Command frame `AA B4 06 01 <mode> ... AB`
//! sets sleep/work; the cycle is: wake fan -> wait ~15–30 s for the airflow to
//! stabilise -> read a frame -> put the sensor back to sleep. The sensor must
//! also be kept dry (condensation ruins both reading and hardware).

use heapless::{String, Vec};

use super::{write_tenths, EntityDescriptor, Reading, Sensor, MAX_READINGS};

/// Frame markers.
pub const HEAD: u8 = 0xAA;
pub const TAIL: u8 = 0xAB;
/// Command byte for a measurement-data reply.
pub const CMD_DATA: u8 = 0xC0;
pub const FRAME_LEN: usize = 10;
/// Seconds to run the fan before trusting a reading.
pub const WARMUP_SECS: u64 = 20;

const DESCRIPTORS: &[EntityDescriptor] = &[
    EntityDescriptor {
        key: "pm25",
        name: "PM2.5",
        unit: "µg/m³",
        device_class: "pm25",
        state_class: "measurement",
    },
    EntityDescriptor {
        key: "pm10",
        name: "PM10",
        unit: "µg/m³",
        device_class: "pm10",
        state_class: "measurement",
    },
];

/// Little-endian PM word -> µg/m³ in **tenths** (raw is already tenths of µg/m³).
pub const fn pm_tenths(lo: u8, hi: u8) -> i32 {
    (hi as i32) * 256 + lo as i32
}

/// Validate a 10-byte frame: markers, command byte, and checksum.
pub fn frame_ok(f: &[u8; FRAME_LEN]) -> bool {
    if f[0] != HEAD || f[1] != CMD_DATA || f[FRAME_LEN - 1] != TAIL {
        return false;
    }
    let sum = f[2]
        .wrapping_add(f[3])
        .wrapping_add(f[4])
        .wrapping_add(f[5])
        .wrapping_add(f[6])
        .wrapping_add(f[7]);
    sum == f[8]
}

/// SDS011 on a UART. TODO(#15): hold the UART handle; zero-field placeholder.
pub struct Sds011;

impl Sds011 {
    pub fn new() -> Self {
        Self
    }

    fn push(readings: &mut Vec<Reading, MAX_READINGS>, key: &'static str, tenths: i32) {
        let mut value = String::new();
        write_tenths(&mut value, tenths);
        let _ = readings.push(Reading { key, value });
    }
}

impl Default for Sds011 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sensor for Sds011 {
    fn kind(&self) -> &'static str {
        "SDS011"
    }

    fn descriptors(&self) -> &'static [EntityDescriptor] {
        DESCRIPTORS
    }

    async fn measure(&mut self) -> Vec<Reading, MAX_READINGS> {
        // TODO(#15): wake fan, Timer::after(WARMUP_SECS), read until a valid
        // frame (`frame_ok`), then:
        //   Self::push(&mut out, "pm25", pm_tenths(f[2], f[3]));
        //   Self::push(&mut out, "pm10", pm_tenths(f[4], f[5]));
        // finally put the sensor back to sleep. Empty Vec on timeout.
        todo!("SDS011 UART read + fan duty-cycle (#15)")
    }
}

const _: () = {
    // 0x00F5 = 245 -> 24.5 µg/m³ (245 tenths)
    assert!(pm_tenths(0xF5, 0x00) == 245);
    assert!(pm_tenths(0x00, 0x01) == 256);
};

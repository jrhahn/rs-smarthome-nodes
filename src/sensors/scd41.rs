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

#[cfg(feature = "drivers")]
use embassy_time::{Duration, Timer};
#[cfg(feature = "drivers")]
use embedded_hal_async::i2c::I2c as I2cBus;
#[cfg(feature = "drivers")]
use heapless::{String, Vec};

use super::EntityDescriptor;
#[cfg(feature = "drivers")]
use super::{crc_word, write_int, write_tenths, Reading, Sensor, MAX_READINGS};

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
#[cfg(feature = "drivers")]
pub struct Scd41<I2C> {
    i2c: I2C,
    pub mode: Mode,
    /// Periodic mode only: whether `start_periodic_measurement` has been sent
    /// this boot. A mains node stays powered, so this happens once.
    started: bool,
}

#[cfg(feature = "drivers")]
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

#[cfg(feature = "drivers")]
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

#[cfg(test)]
mod tests {
    use super::*;
    // `super::*` brings in heapless's `Vec`/`String` once the drivers are
    // compiled; the tests want the growable ones.
    #[allow(unused_imports)]
    use std::{string::String, vec::Vec};

    #[test]
    fn co2_is_reported_raw() {
        assert_eq!(co2_ppm(0), 0);
        assert_eq!(co2_ppm(812), 812);
        assert_eq!(co2_ppm(40_000), 40_000);
    }

    #[test]
    fn temperature_and_humidity_share_the_sht31_transfer_function() {
        assert_eq!(temp_tenths(0), crate::sensors::sht31::temp_tenths(0));
        assert_eq!(rh_tenths(65535), crate::sensors::sht31::rh_tenths(65535));
        assert_eq!(temp_tenths(65535), 1300);
        assert_eq!(rh_tenths(0), 0);
    }

    #[test]
    fn data_ready_looks_only_at_the_low_bits() {
        // The reply's high bits are reserved and set on a healthy sensor;
        // reading them as "ready" would return the previous measurement for ever.
        assert!(!data_ready(0x0000));
        assert!(!data_ready(0x8000));
        assert!(!data_ready(0xF800));
        assert!(data_ready(0x0001));
        assert!(data_ready(0x8001));
        assert!(data_ready(0x07FF));
    }

    #[test]
    fn the_commands_are_distinct() {
        // A transposed digit here would silently start the wrong mode.
        let commands = [
            CMD_START_PERIODIC,
            CMD_START_LOW_POWER_PERIODIC,
            CMD_READ_MEASUREMENT,
            CMD_MEASURE_SINGLE_SHOT,
            CMD_STOP_PERIODIC,
            CMD_GET_DATA_READY,
        ];
        for (i, a) in commands.iter().enumerate() {
            for b in &commands[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    // --- Driver, against a fake bus -----------------------------------------

    /// One Sensirion data word: value, big-endian, plus its CRC.
    #[cfg(feature = "drivers")]
    fn word(value: u16) -> Vec<u8> {
        let bytes = value.to_be_bytes();
        let mut out = bytes.to_vec();
        out.push(super::super::crc8_sensirion(&bytes));
        out
    }

    /// A `read_measurement` reply: CO₂, temperature, humidity.
    #[cfg(feature = "drivers")]
    fn measurement(co2: u16, t: u16, rh: u16) -> Vec<u8> {
        let mut out = word(co2);
        out.extend(word(t));
        out.extend(word(rh));
        out
    }

    /// A `get_data_ready_status` reply. The high bits are reserved and set on a
    /// healthy sensor, so "ready" lives in the low 11.
    #[cfg(feature = "drivers")]
    fn ready(ready: bool) -> Vec<u8> {
        word(if ready { 0x8001 } else { 0x8000 })
    }

    #[cfg(feature = "drivers")]
    fn readings(sensor: &mut Scd41<super::super::mock::FakeI2c>) -> Vec<(&'static str, String)> {
        use super::super::mock::block_on;
        use super::super::Sensor;
        block_on(sensor.measure())
            .iter()
            .map(|r| (r.key, r.value.to_string()))
            .collect()
    }

    #[cfg(feature = "drivers")]
    fn sent(sensor: &Scd41<super::super::mock::FakeI2c>, cmd: u16) -> usize {
        sensor
            .i2c
            .writes()
            .into_iter()
            .filter(|w| *w == cmd.to_be_bytes())
            .count()
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn periodic_mode_starts_the_measurement_and_reports_nothing_yet() {
        use super::super::mock::FakeI2c;

        // The first conversion needs ~5 s. Waiting for it would block the
        // publish path, so the round reports nothing and the next one has data.
        let mut sensor = Scd41::new(FakeI2c::new(ADDR, []), Mode::Periodic);
        assert!(readings(&mut sensor).is_empty());
        assert_eq!(sent(&sensor, CMD_START_PERIODIC), 1);
        assert_eq!(sent(&sensor, CMD_READ_MEASUREMENT), 0);
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn periodic_mode_is_started_only_once() {
        use super::super::mock::FakeI2c;

        // Re-sending `start_periodic` would restart the measurement cycle, so a
        // mains node polling every 60 s would never get a reading at all.
        let replies = vec![
            ready(true),
            measurement(812, 24_900, 32_768),
            ready(true),
            measurement(820, 24_900, 32_768),
        ];
        let mut sensor = Scd41::new(FakeI2c::new(ADDR, replies), Mode::Periodic);
        for _ in 0..3 {
            let _ = readings(&mut sensor);
        }
        assert_eq!(sent(&sensor, CMD_START_PERIODIC), 1);
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn periodic_mode_waits_for_data_ready_before_reading() {
        use super::super::mock::FakeI2c;

        // Reading before the sensor says it is ready returns the *previous*
        // measurement, which on a fresh start is not a measurement at all.
        let mut sensor = Scd41::new(FakeI2c::new(ADDR, vec![ready(false)]), Mode::Periodic);
        let _ = readings(&mut sensor); // start
        assert!(readings(&mut sensor).is_empty());
        assert_eq!(sent(&sensor, CMD_GET_DATA_READY), 1);
        assert_eq!(sent(&sensor, CMD_READ_MEASUREMENT), 0);
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn a_ready_measurement_becomes_three_readings() {
        use super::super::mock::FakeI2c;

        let replies = vec![ready(true), measurement(812, 24_900, 32_768)];
        let mut sensor = Scd41::new(FakeI2c::new(ADDR, replies), Mode::Periodic);
        let _ = readings(&mut sensor); // start

        assert_eq!(
            readings(&mut sensor),
            vec![
                ("co2", "812".to_string()),
                ("temperature", "21.4".to_string()),
                ("humidity", "50.0".to_string()),
            ]
        );
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn zero_ppm_is_the_sensors_no_measurement_answer_not_air() {
        use super::super::mock::FakeI2c;

        // 0 ppm is not a reading of clean air — it is the sensor saying it has
        // nothing. Publishing it would draw a plausible-looking line at zero.
        let replies = vec![ready(true), measurement(0, 24_900, 32_768)];
        let mut sensor = Scd41::new(FakeI2c::new(ADDR, replies), Mode::Periodic);
        let _ = readings(&mut sensor); // start
        assert!(readings(&mut sensor).is_empty());
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn a_corrupt_word_is_dropped_rather_than_published() {
        use super::super::mock::FakeI2c;

        // Bus noise anywhere in the nine bytes, including the CRCs themselves.
        for corrupt in 0..9 {
            let mut reply = measurement(812, 24_900, 32_768);
            reply[corrupt] ^= 0x01;
            let mut sensor =
                Scd41::new(FakeI2c::new(ADDR, vec![ready(true), reply]), Mode::Periodic);
            let _ = readings(&mut sensor); // start
            assert!(
                readings(&mut sensor).is_empty(),
                "a measurement with byte {corrupt} corrupted was published"
            );
        }
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn a_corrupt_data_ready_reply_is_not_read_as_ready() {
        use super::super::mock::FakeI2c;

        let mut reply = ready(true);
        reply[2] ^= 0x01; // CRC no longer matches
        let mut sensor = Scd41::new(FakeI2c::new(ADDR, vec![reply]), Mode::Periodic);
        let _ = readings(&mut sensor); // start
        assert!(readings(&mut sensor).is_empty());
        assert_eq!(sent(&sensor, CMD_READ_MEASUREMENT), 0);
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn an_absent_sensor_contributes_nothing() {
        use super::super::mock::FakeI2c;

        // Silent, not fatal: the node publishes whatever else answered.
        let mut sensor = Scd41::new(FakeI2c::empty(), Mode::Periodic);
        assert!(readings(&mut sensor).is_empty());
        let mut sensor = Scd41::new(FakeI2c::empty(), Mode::SingleShot);
        assert!(readings(&mut sensor).is_empty());
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn single_shot_mode_measures_on_demand_without_polling() {
        use super::super::mock::FakeI2c;

        // The battery node's mode: ask for one conversion, wait it out, read.
        // No `start_periodic` (there is no continuous cycle to keep alive) and
        // no data-ready poll (the wait already covers it).
        //
        // This test really does take the datasheet's ~5 s conversion time — it
        // is a fixed sensor timing, not a policy knob like the SDS011 warm-up,
        // so there is nothing honest to shorten.
        let replies = vec![measurement(812, 24_900, 32_768)];
        let mut sensor = Scd41::new(FakeI2c::new(ADDR, replies), Mode::SingleShot);

        assert_eq!(
            readings(&mut sensor),
            vec![
                ("co2", "812".to_string()),
                ("temperature", "21.4".to_string()),
                ("humidity", "50.0".to_string()),
            ]
        );
        assert_eq!(sent(&sensor, CMD_MEASURE_SINGLE_SHOT), 1);
        assert_eq!(sent(&sensor, CMD_START_PERIODIC), 0);
        assert_eq!(sent(&sensor, CMD_GET_DATA_READY), 0);
        assert_eq!(sent(&sensor, CMD_READ_MEASUREMENT), 1);
    }
}

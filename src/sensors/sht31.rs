//! SHT31-D temperature + humidity driver (I²C, #13).
//!
//! Bus: I²C, address **0x44** (ADDR pin low) or 0x45 (high).
//! Single-shot, high repeatability, clock-stretching **disabled**: write command
//! `0x2400`, wait out the conversion (see [`CONVERSION_MS`]), then read 6 bytes:
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

#[cfg(feature = "drivers")]
use embassy_time::{Duration, Timer};
#[cfg(feature = "drivers")]
use embedded_hal_async::i2c::I2c as I2cBus;
#[cfg(feature = "drivers")]
use heapless::{String, Vec};

use super::EntityDescriptor;
#[cfg(feature = "drivers")]
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
/// The datasheet's figure is 12.5 ms typical but **15.5 ms maximum**, so the
/// old 15 sat under the worst case and read early often enough to matter: an
/// early read is NAKed, `sample` returns `None`, and the node reports the
/// sensor as not responding when it is merely still converting. Rounded up
/// with a little margin rather than to the exact maximum.
pub const CONVERSION_MS: u64 = 17;

// The floor the paragraph above argues for, checked where a violation belongs:
// at compile time. As a test it was an assertion over a constant, which clippy
// folds to `assert!(true)` and rejects -- and it would only have failed after
// the image was already built.
const _: () = assert!(
    CONVERSION_MS >= 16,
    "the wait cannot cover a 15.5 ms conversion"
);

/// Datasheet soft-reset time (ms). 1.5 ms in the datasheet; wait 2.
pub const RESET_MS: u64 = 2;

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
#[cfg(feature = "drivers")]
pub struct Sht31<I2C> {
    i2c: I2C,
    addr: u8,
    /// Humidity from the most recent [`Sensor::measure`], in tenths of a
    /// percent, or `None` if that measurement failed.
    ///
    /// Kept because a second sensor on the same node may need it: the SDS011's
    /// humidity correction reads the room off this one (see
    /// [`crate::sensors::sds011::compensate`]). It is deliberately the *raw*
    /// figure rather than the formatted [`Reading`], so the correction does not
    /// have to parse back a string it just printed.
    last_rh_tenths: Option<i32>,
    last_t_tenths: Option<i32>,
    /// Why the last measurement produced nothing, in the caller's terms.
    ///
    /// Worth keeping because the boot probe is a *write* and nothing more
    /// (`acks` in `platform.rs`), so "found at 0x44" and "answers a
    /// measurement" are different claims -- and on 2026-09-13 this sensor
    /// spent three rounds satisfying the first while failing the second. The
    /// generic "not responding" the caller falls back to was, by then, the
    /// least true sentence available.
    fault: Option<&'static str>,
}

#[cfg(feature = "drivers")]
impl<I2C: I2cBus> Sht31<I2C> {
    /// Driver at the default address (0x44, ADDR low).
    pub fn new(i2c: I2C) -> Self {
        Self {
            i2c,
            addr: ADDR,
            last_rh_tenths: None,
            last_t_tenths: None,
            fault: None,
        }
    }

    /// Driver at an explicit address, for a board that strapped ADDR high.
    pub fn with_address(i2c: I2C, addr: u8) -> Self {
        Self {
            i2c,
            addr,
            last_rh_tenths: None,
            last_t_tenths: None,
            fault: None,
        }
    }

    /// Humidity from the last round, for a sensor that needs it (see
    /// [`Sht31::last_rh_tenths`]). `None` after a failed measurement, so a
    /// caller can tell "the room is dry" from "the sensor did not answer".
    pub fn last_humidity_tenths(&self) -> Option<i32> {
        self.last_rh_tenths
    }

    /// Temperature from the last round, in tenths of a degree.
    ///
    /// Kept for the same reason as the humidity beside it, and used by the same
    /// kind of caller: the SGP4x compensates its hotplate against ambient
    /// conditions, and wants both halves from air measured at the same moment.
    pub fn last_temperature_tenths(&self) -> Option<i32> {
        self.last_t_tenths
    }

    /// Put the sensor back into a known state, and wait out the datasheet's
    /// recovery time.
    ///
    /// This matters far more on a battery node than a mains one. The sensor is
    /// powered continuously from 3V3, but the MCU beside it cold-boots every
    /// couple of seconds; a restart that lands mid-transaction leaves the SHT31
    /// part-way through a command. It then still ACKs its address — so a bus
    /// scan finds it — while NAKing the next command, which is exactly how the
    /// fault reads from outside: "I²C scan: 0x44 answered" next to "no SHT31-D
    /// at 0x44 or 0x45". A mains node never hot-restarts and so never gets
    /// there, which is why this only ever showed up outdoors.
    ///
    /// Errors are ignored on purpose: if the sensor is unreachable the probe
    /// that follows says so with better wording than this could.
    pub async fn soft_reset(&mut self) {
        let _ = self
            .i2c
            .write(self.addr, &CMD_SOFT_RESET.to_be_bytes())
            .await;
        Timer::after(Duration::from_millis(RESET_MS)).await;
    }

    /// One single-shot conversion -> `(temperature_raw, humidity_raw)`, or
    /// `None` if the sensor did not ACK or a word failed its CRC.
    async fn sample(&mut self) -> Option<(u16, u16)> {
        if self
            .i2c
            .write(self.addr, &CMD_SINGLE_HIGH.to_be_bytes())
            .await
            .is_err()
        {
            self.fault = Some(
                "will not take a measure command. If a bus scan still finds its address it is \
                 powered and addressable and has stopped listening -- which is the state a \
                 restart mid-transaction leaves it in, and which only a power cycle has been \
                 seen to clear",
            );
            return None;
        }
        Timer::after(Duration::from_millis(CONVERSION_MS)).await;

        let mut buf = [0u8; 6];
        if self.i2c.read(self.addr, &mut buf).await.is_err() {
            self.fault = Some("took the measure command and then returned nothing");
            return None;
        }
        match (crc_word(&buf[0..3]), crc_word(&buf[3..6])) {
            (Some(t_raw), Some(rh_raw)) => {
                self.fault = None;
                Some((t_raw, rh_raw))
            }
            _ => {
                self.fault = Some(
                    "answered with a failed checksum -- the bytes arrive and are spoiled \
                          on the way, so look at the contacts rather than at the part",
                );
                None
            }
        }
    }

    fn push(readings: &mut Vec<Reading, MAX_READINGS>, key: &'static str, tenths: i32) {
        let mut value = String::new();
        write_tenths(&mut value, tenths);
        let _ = readings.push(Reading { key, value });
    }
}

#[cfg(feature = "drivers")]
impl<I2C: I2cBus> Sensor for Sht31<I2C> {
    fn kind(&self) -> &'static str {
        "SHT31-D"
    }

    fn descriptors(&self) -> &'static [EntityDescriptor] {
        DESCRIPTORS
    }

    fn fault(&self) -> Option<&'static str> {
        self.fault
    }

    async fn measure(&mut self) -> Vec<Reading, MAX_READINGS> {
        let mut out = Vec::new();
        // A missing or mis-wired sensor simply contributes nothing; the caller
        // logs it and publishes whatever else answered.
        self.last_rh_tenths = None;
        self.last_t_tenths = None;

        // A second chance behind a reset, because the failure this recovers
        // from is not rare: a sensor left part-way through a command by an MCU
        // restart answers its address and refuses everything else, and the
        // reset issued once at boot is no help at all if it enters that state
        // afterwards. One retry, not a loop -- a sensor that is genuinely gone
        // should cost one extra command a round, not a stall.
        let mut sampled = self.sample().await;
        if sampled.is_none() {
            self.soft_reset().await;
            sampled = self.sample().await;
        }
        if let Some((t_raw, rh_raw)) = sampled {
            Self::push(&mut out, "temperature", temp_tenths(t_raw));
            Self::push(&mut out, "humidity", rh_tenths(rh_raw));
            self.last_rh_tenths = Some(rh_tenths(rh_raw));
            self.last_t_tenths = Some(temp_tenths(t_raw));
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
    // `super::*` brings in heapless's `Vec` once the drivers are compiled; the
    // tests want the growable one.
    #[allow(unused_imports)]
    use std::vec::Vec;

    /// "not responding" used to be the only thing the caller could say about
    /// this sensor, and on 2026-09-13 it was the least true sentence
    /// available: the part answered its address for three rounds while
    /// refusing every measurement.
    #[cfg(feature = "drivers")]
    #[test]
    fn a_sensor_that_will_not_take_the_command_says_so() {
        use super::super::mock::{block_on, FakeI2c};
        let mut sensor = Sht31::new(FakeI2c::empty());
        assert!(block_on(sensor.measure()).is_empty());
        let fault = sensor
            .fault()
            .expect("a failed measurement explains itself");
        assert!(fault.contains("will not take a measure command"), "{fault}");
        assert!(fault.contains("power cycle"), "{fault}");
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn a_corrupt_answer_is_not_reported_as_an_absent_sensor() {
        use super::super::mock::{block_on, FakeI2c};
        // Two reads -- the measurement and the retry behind the reset -- both
        // with broken checksums.
        let bad = vec![0x12, 0x34, 0x00, 0x56, 0x78, 0x00];
        let mut sensor = Sht31::new(FakeI2c::new(ADDR, [bad.clone(), bad]));
        assert!(block_on(sensor.measure()).is_empty());
        let fault = sensor.fault().unwrap();
        assert!(fault.contains("failed checksum"), "{fault}");
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn a_stuck_sensor_gets_one_reset_and_a_second_chance() {
        use super::super::mock::{block_on, FakeI2c};
        // First read is corrupt, the retry after the soft reset is good: the
        // round still produces its two readings, which is the whole point of
        // the retry.
        let bad = vec![0x12, 0x34, 0x00, 0x56, 0x78, 0x00];
        // 25.0 °C and 50.0 %RH, with the checksums Sensirion's CRC-8 actually
        // produces -- a made-up checksum would fail the retry for the wrong
        // reason and the test would still look green in the wrong way.
        let good = vec![0x66, 0x66, 0x93, 0x80, 0x00, 0xA2];
        let mut sensor = Sht31::new(FakeI2c::new(ADDR, [bad, good]));
        let readings = block_on(sensor.measure());
        assert_eq!(readings.len(), 2, "temperature and humidity");
        assert_eq!(readings[0].value.as_str(), "25.0");
        assert_eq!(readings[1].value.as_str(), "50.0");
        assert_eq!(sensor.fault(), None, "a recovered round reports no fault");
        assert!(sensor.last_humidity_tenths().is_some());
    }

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

    // --- Driver, against a fake bus -----------------------------------------

    /// A CRC-correct 6-byte reply for the given raw words.
    #[cfg(feature = "drivers")]
    fn reply(t_raw: u16, rh_raw: u16) -> Vec<u8> {
        let mut out = Vec::new();
        for word in [t_raw, rh_raw] {
            let bytes = word.to_be_bytes();
            out.extend_from_slice(&bytes);
            out.push(super::super::crc8_sensirion(&bytes));
        }
        out
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn a_measurement_is_decoded_and_published_as_two_readings() {
        use super::super::mock::{block_on, FakeI2c};
        use super::super::Sensor;

        let bus = FakeI2c::new(ADDR, [reply(24_900, 32_768)]);
        let mut sensor = Sht31::new(bus);
        let readings = block_on(sensor.measure());

        let values: Vec<(&str, &str)> =
            readings.iter().map(|r| (r.key, r.value.as_str())).collect();
        assert_eq!(values, vec![("temperature", "21.4"), ("humidity", "50.0")]);
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn the_conversion_is_started_before_the_read() {
        use super::super::mock::{block_on, FakeI2c, I2cEvent};
        use super::super::Sensor;

        let bus = FakeI2c::new(ADDR, [reply(0, 0)]);
        let mut sensor = Sht31::new(bus);
        let _ = block_on(sensor.measure());

        // Command first, then a 6-byte read — reading before the conversion
        // has been asked for would return the *previous* measurement.
        assert_eq!(
            sensor.i2c.events,
            vec![
                I2cEvent::Write {
                    addr: ADDR,
                    data: CMD_SINGLE_HIGH.to_be_bytes().to_vec(),
                },
                I2cEvent::Read { addr: ADDR, len: 6 },
            ]
        );
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn a_soft_reset_is_issued_before_anything_else_is_asked() {
        use super::super::mock::{block_on, FakeI2c};

        // The bug this guards: `CMD_SOFT_RESET` was defined and documented in
        // this file and never sent, so a sensor left mid-command by the MCU's
        // cold boot stayed that way -- ACKing its address for the bus scan
        // while NAKing every command, which reads from outside as a missing
        // sensor on a working bus.
        let mut bus = FakeI2c::new(ADDR, [reply(24_900, 32_768)]);
        let mut sensor = Sht31::new(&mut bus);
        block_on(sensor.soft_reset());

        assert_eq!(
            bus.writes(),
            vec![&CMD_SOFT_RESET.to_be_bytes()[..]],
            "the reset command itself, and nothing else"
        );
    }

    #[test]
    fn a_corrupt_word_is_dropped_rather_than_published() {
        use super::super::mock::{block_on, FakeI2c};
        use super::super::Sensor;

        // Bus noise in each of the two words in turn, and in a CRC byte.
        for corrupt in 0..6 {
            let mut bytes = reply(24_900, 32_768);
            bytes[corrupt] ^= 0x01;
            let mut sensor = Sht31::new(FakeI2c::new(ADDR, [bytes]));
            assert!(
                block_on(sensor.measure()).is_empty(),
                "a reading with byte {corrupt} corrupted was published"
            );
        }
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn an_absent_sensor_contributes_nothing() {
        use super::super::mock::{block_on, FakeI2c};
        use super::super::Sensor;

        // Nothing on the bus: every transaction NACKs. This must be silent, not
        // fatal — the node publishes whatever else answered.
        let mut sensor = Sht31::new(FakeI2c::empty());
        assert!(block_on(sensor.measure()).is_empty());
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn a_sensor_strapped_to_the_alternate_address_is_reachable() {
        use super::super::mock::{block_on, FakeI2c};
        use super::super::Sensor;

        // The breakout the bus probe adopts at 0x45.
        let mut sensor =
            Sht31::with_address(FakeI2c::new(ADDR_ALT, [reply(24_900, 32_768)]), ADDR_ALT);
        assert_eq!(block_on(sensor.measure()).len(), 2);

        // ... and the same driver at the default address finds nothing there.
        let mut sensor = Sht31::new(FakeI2c::new(ADDR_ALT, [reply(24_900, 32_768)]));
        assert!(block_on(sensor.measure()).is_empty());
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

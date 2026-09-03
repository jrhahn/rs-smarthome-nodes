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
//!
//! ## Temperature offset
//!
//! The SCD41 measures on a die that heats itself, and cancels that with a
//! *temperature offset* (`set_temperature_offset`, 0x241D) — 4 °C out of the
//! box. The offset is not cosmetic: the humidity signal is compensated to the
//! offset-corrected temperature, so an offset that does not match the board's
//! real self-heating skews **both** outputs, temperature low and humidity high.
//! Seen head-to-head against an SHT31-D in the same room on 2026-08-26: 24.7 °C
//! / 70.9 % against 26.3 °C / 53.6 %.
//!
//! [`Scd41::set_temperature_offset`] takes hundredths of °C and writes the
//! register lazily, at the next sample. The value is *not* persisted to the
//! sensor's EEPROM (`persist_settings` has a finite write budget); it is
//! rewritten from the node's flash config on every boot.

#[cfg(feature = "drivers")]
use embassy_time::{Duration, Timer};
#[cfg(feature = "drivers")]
use embedded_hal_async::i2c::I2c as I2cBus;
#[cfg(feature = "drivers")]
use heapless::{String, Vec};

use super::EntityDescriptor;
#[cfg(feature = "drivers")]
use super::{crc8_sensirion, crc_word, write_int, write_tenths, Reading, Sensor, MAX_READINGS};

pub const ADDR: u8 = 0x62;
pub const CMD_START_PERIODIC: u16 = 0x21B1;
pub const CMD_START_LOW_POWER_PERIODIC: u16 = 0x21AC;
pub const CMD_READ_MEASUREMENT: u16 = 0xEC05;
pub const CMD_MEASURE_SINGLE_SHOT: u16 = 0x219D;
pub const CMD_STOP_PERIODIC: u16 = 0x3F86;
pub const CMD_GET_DATA_READY: u16 = 0xE4B8;
pub const CMD_SET_TEMPERATURE_OFFSET: u16 = 0x241D;
pub const CMD_GET_TEMPERATURE_OFFSET: u16 = 0x2318;
/// Returns three words forming a unique 48-bit serial, big-endian.
pub const CMD_GET_SERIAL_NUMBER: u16 = 0x3682;
/// End-of-line self test. One word: zero means no malfunction detected.
pub const CMD_PERFORM_SELF_TEST: u16 = 0x3639;
/// Datasheet execution time for the self test. It really is ten seconds.
pub const SELF_TEST_MS: u64 = 10_000;
/// Single-shot conversion time (ms) before `read_measurement`.
pub const SINGLE_SHOT_MS: u64 = 5000;
/// Datasheet execution time for the short commands (ms).
pub const CMD_DELAY_MS: u64 = 2;

/// The sensor's own power-on temperature offset, in hundredths of °C.
///
/// 4 °C is what Sensirion ships, chosen for a board that runs the sensor
/// *continuously* — that is the self-heating it cancels. Any node that samples
/// less hard, or that mounts the SCD41 where it can shed heat, needs less than
/// this, and every 0.01 °C too much shows up twice: the temperature reads low,
/// and the humidity — which the sensor compensates to that same temperature —
/// reads high.
pub const DEFAULT_OFFSET_CENTI: i32 = 400;

/// Largest offset we will program, in hundredths of °C. The register itself
/// spans the whole 0..175 °C transfer function, but an offset beyond ~20 °C is
/// a typo rather than a calibration, and it would silently wreck both signals.
pub const MAX_OFFSET_CENTI: i32 = 2000;

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

/// Temperature offset in hundredths of °C -> the raw word the offset register
/// takes. Same transfer function as the temperature signal, minus the -45 °C
/// zero point: `raw = offset / 175 · 65535`.
///
/// Out-of-range values are clamped rather than rejected: this sits behind a
/// Home Assistant slider, and refusing a bad value would leave the sensor on
/// whatever it had before with nothing to show for it.
pub const fn offset_raw(centi: i32) -> u16 {
    let clamped = if centi < 0 {
        0
    } else if centi > MAX_OFFSET_CENTI {
        MAX_OFFSET_CENTI
    } else {
        centi
    };
    // 2000 · 65535 stays well inside i32.
    ((clamped * 65535) / 17500) as u16
}

/// Inverse of [`offset_raw`], for reading the register back.
pub const fn offset_centi(raw: u16) -> i32 {
    (17500 * raw as i32) / 65535
}

/// A data-ready reply carries the status in the low 11 bits; all-zero means
/// "no measurement pending".
pub const fn data_ready(status: u16) -> bool {
    status & 0x07FF != 0
}

/// A self-test reply: zero means no malfunction detected. Note the inversion —
/// unlike [`data_ready`], here zero is the good news.
pub const fn self_test_passed(status: u16) -> bool {
    status == 0
}

/// Assemble the three `get_serial_number` words into the 48-bit id.
pub const fn serial_from_words(words: [u16; 3]) -> u64 {
    (words[0] as u64) << 32 | (words[1] as u64) << 16 | words[2] as u64
}

/// Where the last measurement attempt fell over.
///
/// The SCD41 fails in ways that look the same from the publish path — an empty
/// round — but mean very different things on the bench. It acknowledging its
/// address (which the boot probe checks) only proves the bus works; everything
/// below can still fail after that.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The sensor would not take the temperature-offset write. It only accepts
    /// config commands while idle, so this means the stop did not take.
    OffsetRejected,
    /// `start_periodic_measurement` was not acknowledged.
    StartRejected,
    /// Started, but `get_data_ready_status` never reported a sample. Expected
    /// once right after the start (the first conversion needs ~5 s); persisting
    /// past that is not.
    NeverReady,
    /// The data-ready poll itself did not answer, or answered with a bad CRC.
    ReadyUnreadable,
    /// `read_measurement` did not answer, or one of its three words failed CRC.
    MeasurementUnreadable,
    /// A well-formed reply reporting 0 ppm, which is the sensor's way of saying
    /// it has nothing valid yet.
    NoValidSample,
    /// Not a fault: the measurement was just started and the first conversion
    /// needs ~5 s. Expected exactly once per periodic start, and worth saying
    /// out loud so it is not mistaken for one of the failures above.
    Warming,
}

impl Fault {
    pub const fn describe(self) -> &'static str {
        match self {
            Fault::OffsetRejected => {
                "refused the temperature-offset write (it only accepts config commands while idle)"
            }
            Fault::StartRejected => "refused start_periodic_measurement",
            Fault::NeverReady => "started, but never reported a ready measurement",
            Fault::ReadyUnreadable => {
                "did not answer the data-ready poll, or answered with a bad CRC"
            }
            Fault::MeasurementUnreadable => {
                "did not answer read_measurement, or answered with a bad CRC"
            }
            Fault::NoValidSample => "reported 0 ppm, i.e. no valid measurement yet",
            Fault::Warming => "measurement just started; first conversion takes ~5 s",
        }
    }
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
    /// Temperature offset we want the sensor to use, in hundredths of °C.
    offset_centi: i32,
    /// The offset actually programmed into the sensor, once we have written one
    /// this boot. `None` forces a write on the next sample — which is also the
    /// state after a reboot, because we deliberately never `persist_settings`
    /// (that register is EEPROM with a finite write budget, and the value comes
    /// back from flash config on every boot anyway).
    written_offset: Option<i32>,
    /// Whether we *know* the sensor is idle, i.e. accepting config commands.
    /// False at boot on purpose: the SCD41 keeps its own 3V3 rail across an ESP
    /// reset, so "we just started" says nothing about what it is doing.
    known_idle: bool,
    /// Where the last empty round fell over, for the log.
    fault: Option<Fault>,
}

#[cfg(feature = "drivers")]
impl<I2C: I2cBus> Scd41<I2C> {
    pub fn new(i2c: I2C, mode: Mode) -> Self {
        Self {
            i2c,
            mode,
            started: false,
            offset_centi: DEFAULT_OFFSET_CENTI,
            written_offset: None,
            known_idle: false,
            fault: None,
        }
    }

    /// Record where a round failed and hand back `None`, so the call sites stay
    /// one line each.
    fn fail<T>(&mut self, fault: Fault) -> Option<T> {
        self.fault = Some(fault);
        None
    }

    /// Ask for a different temperature offset, in hundredths of °C.
    ///
    /// Cheap and I²C-free: it only records the wish. The register is written at
    /// the next [`Self::sample`], which is the one place that knows whether the
    /// sensor is in a state that accepts config commands.
    pub fn set_temperature_offset(&mut self, centi: i32) {
        if centi != self.offset_centi {
            self.offset_centi = centi;
            self.written_offset = None;
        }
    }

    /// The offset currently requested, in hundredths of °C.
    pub fn temperature_offset(&self) -> i32 {
        self.offset_centi
    }

    /// Send a bare command word and give the sensor its execution time.
    async fn command(&mut self, cmd: u16) -> Option<()> {
        self.i2c.write(ADDR, &cmd.to_be_bytes()).await.ok()?;
        Timer::after(Duration::from_millis(CMD_DELAY_MS)).await;
        Some(())
    }

    /// Send a command word followed by one argument word and its CRC.
    async fn write_word(&mut self, cmd: u16, arg: u16) -> Option<()> {
        let arg = arg.to_be_bytes();
        let frame = [
            cmd.to_be_bytes()[0],
            cmd.to_be_bytes()[1],
            arg[0],
            arg[1],
            crc8_sensirion(&arg),
        ];
        self.i2c.write(ADDR, &frame).await.ok()?;
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
        self.known_idle = true;
    }

    /// Stop the sensor unless we already know it is idle, so the 500 ms is paid
    /// once rather than by every caller that needs a quiet bus.
    async fn ensure_idle(&mut self) {
        if !self.known_idle {
            self.stop_periodic().await;
        }
    }

    /// Program the temperature offset if it is not already what we want.
    ///
    /// The offset register is only writable while the sensor is **idle**: in
    /// periodic mode the SCD41 NACKs everything except `read_measurement`,
    /// `get_data_ready_status` and `stop_periodic_measurement`. Clearing
    /// `started` (via [`Self::stop_periodic`]) makes the caller restart it.
    ///
    /// Costs 500 ms, but only on the rounds where the offset actually changed —
    /// and on a periodic node that is the boot round plus whenever someone moves
    /// the slider in Home Assistant.
    async fn sync_offset(&mut self) -> Option<()> {
        if self.written_offset == Some(self.offset_centi) {
            return Some(());
        }
        if self.mode == Mode::Periodic {
            self.ensure_idle().await;
        }
        if self
            .write_word(CMD_SET_TEMPERATURE_OFFSET, offset_raw(self.offset_centi))
            .await
            .is_none()
        {
            return self.fail(Fault::OffsetRejected);
        }
        self.written_offset = Some(self.offset_centi);
        Some(())
    }

    /// `get_serial_number` — a unique 48-bit id, and the cheapest proof that a
    /// real Sensirion part is on the bus.
    ///
    /// Worth logging once per boot: it says which physical sensor is in which
    /// node, and counterfeits (the SCD4x is among the most-copied parts around)
    /// tend to give themselves away here, returning zeroes or the same number
    /// on every unit.
    ///
    /// Only valid while the sensor is idle, so this stops a running measurement.
    pub async fn serial_number(&mut self) -> Option<u64> {
        self.ensure_idle().await;
        self.command(CMD_GET_SERIAL_NUMBER).await?;
        let mut buf = [0u8; 9];
        self.i2c.read(ADDR, &mut buf).await.ok()?;
        Some(serial_from_words([
            crc_word(&buf[0..3])?,
            crc_word(&buf[3..6])?,
            crc_word(&buf[6..9])?,
        ]))
    }

    /// `perform_self_test` — ask the sensor whether it believes it is healthy.
    ///
    /// `Some(true)` means no malfunction detected, `Some(false)` a malfunction,
    /// `None` that the sensor did not answer. This is the one call that
    /// separates "the reading is wrong" from "the hardware is broken", and it
    /// is worth every bit of the ten seconds it takes — the alternative is an
    /// evening of swapping wires and supplies (2026-08-26).
    ///
    /// Blocks for [`SELF_TEST_MS`], so this is not something to run per round.
    /// Only valid while the sensor is idle, so it stops a running measurement;
    /// periodic mode restarts by itself on the next sample.
    pub async fn self_test(&mut self) -> Option<bool> {
        self.ensure_idle().await;
        self.command(CMD_PERFORM_SELF_TEST).await?;
        Timer::after(Duration::from_millis(SELF_TEST_MS)).await;
        let mut buf = [0u8; 3];
        self.i2c.read(ADDR, &mut buf).await.ok()?;
        Some(self_test_passed(crc_word(&buf)?))
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
        // Before anything else: a pending offset change has to land while the
        // sensor is idle, and in periodic mode applying it stops the
        // measurement, so the restart below has to see the result.
        self.fault = None;
        self.sync_offset().await?;
        match self.mode {
            Mode::Periodic => {
                if !self.started {
                    // The sensor is powered from 3V3 and never sees the ESP's
                    // reset, so it can still be measuring from a previous boot.
                    // In periodic mode it accepts only read_measurement,
                    // get_data_ready_status and stop_periodic_measurement, and
                    // refuses `start_periodic` — leaving `started` false, so
                    // every later round retried the same refused command and the
                    // node never produced a reading until someone pulled power.
                    // Observed on `schlafzimmer` after a monitor-triggered
                    // reset, 2026-08-25. Stopping first costs 500 ms once —
                    // `ensure_idle` skips it when `sync_offset` just stopped.
                    self.ensure_idle().await;
                    if self.command(CMD_START_PERIODIC).await.is_none() {
                        return self.fail(Fault::StartRejected);
                    }
                    self.started = true;
                    self.known_idle = false;
                    // The first conversion needs ~5 s; report nothing this round
                    // rather than blocking the publish path.
                    return self.fail(Fault::Warming);
                }
                match self.ready().await {
                    None => return self.fail(Fault::ReadyUnreadable),
                    Some(false) => return self.fail(Fault::NeverReady),
                    Some(true) => {}
                }
                match self.read_measurement().await {
                    Some(sample) => Some(sample),
                    None => self.fail(Fault::MeasurementUnreadable),
                }
            }
            Mode::SingleShot => {
                if self.command(CMD_MEASURE_SINGLE_SHOT).await.is_none() {
                    return self.fail(Fault::StartRejected);
                }
                Timer::after(Duration::from_millis(SINGLE_SHOT_MS)).await;
                match self.read_measurement().await {
                    Some(sample) => Some(sample),
                    None => self.fail(Fault::MeasurementUnreadable),
                }
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
            self.fault = Some(Fault::NoValidSample);
            return out;
        }
        Self::push_int(&mut out, "co2", co2_ppm(co2_raw));
        Self::push_tenths(&mut out, "temperature", temp_tenths(t_raw));
        Self::push_tenths(&mut out, "humidity", rh_tenths(rh_raw));
        out
    }

    fn fault(&self) -> Option<&'static str> {
        self.fault.map(Fault::describe)
    }
}

const _: () = {
    assert!(co2_ppm(812) == 812);
    assert!(temp_tenths(0) == -450);
    assert!(rh_tenths(65535) == 1000);
    assert!(!data_ready(0x8000)); // only the reserved high bits set -> not ready
    assert!(data_ready(0x8001));
    assert!(offset_raw(0) == 0);
    // A negative offset has no representation in the register, and an absurd
    // one would wreck both signals; both ends clamp instead of wrapping.
    assert!(offset_raw(-1) == 0);
    assert!(offset_raw(MAX_OFFSET_CENTI + 1) == offset_raw(MAX_OFFSET_CENTI));
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
            CMD_SET_TEMPERATURE_OFFSET,
            CMD_GET_TEMPERATURE_OFFSET,
            CMD_GET_SERIAL_NUMBER,
            CMD_PERFORM_SELF_TEST,
        ];
        for (i, a) in commands.iter().enumerate() {
            for b in &commands[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn the_offset_transfer_function_round_trips() {
        // Register resolution is 175/65535 ≈ 0.0027 °C, so a hundredth of a
        // degree survives the round trip to within one count of truncation.
        for centi in [0, 100, 245, 400, 1234, MAX_OFFSET_CENTI] {
            let back = offset_centi(offset_raw(centi));
            assert!(
                (back - centi).abs() <= 1,
                "{centi} centi -> {} -> {back}",
                offset_raw(centi)
            );
        }
    }

    #[test]
    fn the_offset_shares_the_temperature_scale() {
        // Same 175 °C span as the temperature signal, just without its -45 °C
        // zero point: a full-scale offset word is 175 °C, and the datasheet's
        // own worked example (4 °C) has to land where the sensor ships.
        assert_eq!(offset_centi(65535), 17500);
        assert_eq!(offset_raw(DEFAULT_OFFSET_CENTI), 1497);
        assert_eq!(temp_tenths(65535) - temp_tenths(0), 1750);
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
    fn periodic_mode_stops_a_measurement_left_running_by_a_previous_boot() {
        use super::super::mock::FakeI2c;

        // The SCD41 is powered from 3V3 and does not see the ESP's reset, so it
        // can still be measuring when the firmware restarts. `start_periodic` is
        // refused in that state, which used to leave the driver retrying it for
        // ever — the node found the sensor at 0x62 and then published nothing
        // until the board was unplugged. Stop before starting, always.
        let mut sensor = Scd41::new(FakeI2c::new(ADDR, []), Mode::Periodic);
        let _ = readings(&mut sensor);

        assert_eq!(sent(&sensor, CMD_STOP_PERIODIC), 1);
        let order: Vec<&[u8]> = sensor.i2c.writes();
        let stop = order
            .iter()
            .position(|w| *w == CMD_STOP_PERIODIC.to_be_bytes());
        let start = order
            .iter()
            .position(|w| *w == CMD_START_PERIODIC.to_be_bytes());
        assert!(
            stop < start,
            "stop_periodic must precede start_periodic, got {order:?}"
        );
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

    // --- Temperature offset --------------------------------------------------

    /// Every `set_temperature_offset` frame on the bus, as the raw word it
    /// carried. The offset write is command + argument + CRC, so `sent()` (which
    /// matches bare two-byte command frames) cannot see it.
    #[cfg(feature = "drivers")]
    fn offset_writes(sensor: &Scd41<super::super::mock::FakeI2c>) -> Vec<u16> {
        sensor
            .i2c
            .writes()
            .into_iter()
            .filter(|w| w.len() == 5 && w[0..2] == CMD_SET_TEMPERATURE_OFFSET.to_be_bytes())
            .map(|w| {
                assert_eq!(
                    w[4],
                    super::super::crc8_sensirion(&w[2..4]),
                    "bad offset CRC"
                );
                u16::from_be_bytes([w[2], w[3]])
            })
            .collect()
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn the_offset_is_programmed_once_per_boot_and_again_when_it_changes() {
        use super::super::mock::FakeI2c;

        // The offset register is volatile — we never `persist_settings`, so it
        // has to be rewritten every boot or the sensor silently falls back to
        // its 4 °C default.
        let mut sensor = Scd41::new(FakeI2c::new(ADDR, []), Mode::Periodic);
        sensor.set_temperature_offset(245);
        let _ = readings(&mut sensor);
        assert_eq!(offset_writes(&sensor), vec![offset_raw(245)]);

        // Re-sending the same value must not touch the bus: on a periodic node
        // every write costs a stop/start and five wasted seconds of warm-up.
        sensor.set_temperature_offset(245);
        let _ = readings(&mut sensor);
        assert_eq!(offset_writes(&sensor), vec![offset_raw(245)]);

        // A new value from Home Assistant does go through.
        sensor.set_temperature_offset(180);
        let _ = readings(&mut sensor);
        assert_eq!(
            offset_writes(&sensor),
            vec![offset_raw(245), offset_raw(180)]
        );
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn programming_the_offset_stops_the_sensor_exactly_once() {
        use super::super::mock::FakeI2c;

        // The register is only writable while the sensor is idle, and
        // `start_periodic` is only accepted while it is idle. Both needs are
        // real, but the 500 ms stop that satisfies them should be paid once —
        // the naive version stopped in the offset path and then again in the
        // start path, doubling the boot round's dead time.
        let mut sensor = Scd41::new(FakeI2c::new(ADDR, []), Mode::Periodic);
        sensor.set_temperature_offset(245);
        let _ = readings(&mut sensor);

        assert_eq!(sent(&sensor, CMD_STOP_PERIODIC), 1);
        assert_eq!(sent(&sensor, CMD_START_PERIODIC), 1);

        // ...and in the right order: stop, set the offset, then start, so the
        // measurement that follows is the one running on the new offset.
        let order: Vec<&[u8]> = sensor.i2c.writes();
        let pos = |pred: &dyn Fn(&&[u8]) -> bool| order.iter().position(pred);
        let stop = pos(&|w| **w == CMD_STOP_PERIODIC.to_be_bytes());
        let set = pos(&|w| w.len() == 5 && w[0..2] == CMD_SET_TEMPERATURE_OFFSET.to_be_bytes());
        let start = pos(&|w| **w == CMD_START_PERIODIC.to_be_bytes());
        assert!(stop < set && set < start, "{stop:?} {set:?} {start:?}");
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn single_shot_mode_programs_the_offset_without_stopping_anything() {
        use super::super::mock::FakeI2c;

        // A battery node cold-boots into an idle sensor every round, so the
        // write needs no stop — and paying 500 ms per wake-up for one would be
        // a real dent in the power budget.
        let replies = vec![measurement(812, 24_900, 32_768)];
        let mut sensor = Scd41::new(FakeI2c::new(ADDR, replies), Mode::SingleShot);
        sensor.set_temperature_offset(245);
        let _ = readings(&mut sensor);

        assert_eq!(offset_writes(&sensor), vec![offset_raw(245)]);
        assert_eq!(sent(&sensor, CMD_STOP_PERIODIC), 0);
    }

    // --- Identity and self test ----------------------------------------------

    #[test]
    fn the_serial_number_matches_the_datasheet_example() {
        // Datasheet v1.5, Table 25: words f896 / 9f07 / 3bbe are documented as
        // serial number 273'325'796'834'238.
        assert_eq!(
            serial_from_words([0xf896, 0x9f07, 0x3bbe]),
            273_325_796_834_238
        );
        assert_eq!(serial_from_words([0, 0, 0]), 0);
        assert_eq!(serial_from_words([0xffff, 0xffff, 0xffff]), (1 << 48) - 1);
    }

    #[test]
    fn zero_is_the_good_news_for_the_self_test() {
        // Inverted against `data_ready`, where zero means "nothing yet". Getting
        // this backwards would report every healthy sensor as broken, and — far
        // worse — every broken one as healthy.
        assert!(self_test_passed(0x0000));
        assert!(!self_test_passed(0x0001));
        assert!(!self_test_passed(0xFFFF));
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn the_serial_number_is_read_off_the_bus() {
        use super::super::mock::{block_on, FakeI2c};

        let mut reply = word(0xf896);
        reply.extend(word(0x9f07));
        reply.extend(word(0x3bbe));
        let mut sensor = Scd41::new(FakeI2c::new(ADDR, vec![reply]), Mode::Periodic);

        assert_eq!(block_on(sensor.serial_number()), Some(273_325_796_834_238));
        // The command is only valid while the sensor is idle, and at boot we do
        // not know that it is — it keeps its own rail across an ESP reset.
        assert_eq!(sent(&sensor, CMD_STOP_PERIODIC), 1);
        assert_eq!(sent(&sensor, CMD_GET_SERIAL_NUMBER), 1);
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn a_corrupt_serial_number_is_refused_rather_than_invented() {
        use super::super::mock::{block_on, FakeI2c};

        // A wrong CRC must not become a plausible-looking id: this number is
        // used to decide whether a part is genuine.
        let mut reply = word(0xf896);
        reply.extend(word(0x9f07));
        reply.extend(word(0x3bbe));
        let last = reply.len() - 1;
        reply[last] ^= 0xFF;
        let mut sensor = Scd41::new(FakeI2c::new(ADDR, vec![reply]), Mode::Periodic);

        assert_eq!(block_on(sensor.serial_number()), None);
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn the_self_test_reports_both_verdicts() {
        use super::super::mock::{block_on, FakeI2c};

        // Like the single-shot test, this really does wait the datasheet's ten
        // seconds — a fixed sensor timing, not a policy knob.
        let mut healthy = Scd41::new(FakeI2c::new(ADDR, vec![word(0x0000)]), Mode::Periodic);
        assert_eq!(block_on(healthy.self_test()), Some(true));
        assert_eq!(sent(&healthy, CMD_PERFORM_SELF_TEST), 1);
        assert_eq!(sent(&healthy, CMD_STOP_PERIODIC), 1);

        let mut broken = Scd41::new(FakeI2c::new(ADDR, vec![word(0x0100)]), Mode::Periodic);
        assert_eq!(block_on(broken.self_test()), Some(false));

        // A sensor that says nothing is not a sensor that says "healthy".
        let mut absent = Scd41::new(FakeI2c::new(0x00, []), Mode::Periodic);
        assert_eq!(block_on(absent.self_test()), None);
    }
}

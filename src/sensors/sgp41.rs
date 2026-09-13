//! SGP41 / SGP40 gas-sensor driver (I²C, address **0x59**).
//!
//! Two things make this driver unlike the others in this module, and both are
//! worth reading before changing anything here.
//!
//! **It has to be sampled at 1 Hz, not once per publish round.** What the part
//! returns is a hotplate resistance, and that is only comparable while the
//! heater keeps running: let it cool between samples and consecutive readings
//! are not on the same scale. Sensirion specifies the index algorithm for a
//! 0.5–10 s interval on top of that. Sampling this once a minute like the SHT31
//! would yield a number that looks like a VOC index and is not one — so
//! [`Sgp41::sample_once`] is a single 1 Hz step the caller drives in a loop,
//! and [`Sgp41::latest`] only reports what those steps last computed. That is
//! also why this sensor belongs on a node that stays awake: any node that
//! deep-sleeps is not there to drive the loop, whatever it is powered from.
//!
//! **The output is an index, not a concentration.** 1..500, where 100 is the
//! running average of roughly the last 24 hours. There is no µg/m³ and no ppb,
//! and that is a property of the measurement rather than a shortcoming of the
//! part: a metal-oxide film responds to a mixture, and the same resistance can
//! mean very different mixtures. Sensirion's own algorithm is what turns the
//! raw value into the index, and the 45-second blackout it starts with is why
//! [`Sgp41::latest`] returns `None` at first.
//!
//! ## Which part is on the board
//!
//! The SGP40 and SGP41 share the address and most of the command set, and
//! breakout listings routinely mix them up. They differ where it matters: the
//! SGP41 answers `measure_raw_signals` with **two** words (VOC and NOx), the
//! SGP40 has no NOx channel at all. [`Sgp41::detect`] asks that question once
//! and remembers the answer, so a node that turns out to have an SGP40 simply
//! announces one entity instead of two rather than publishing an invented one.
//!
//! Indoors the NOx channel will read its floor forever unless something burns:
//! NOx comes from combustion, and a flat NOx index is the sensor working, not
//! failing.

#[cfg(feature = "drivers")]
use embassy_time::{Duration, Timer};
#[cfg(feature = "drivers")]
use embedded_hal_async::i2c::I2c as I2cBus;
#[cfg(feature = "drivers")]
use gas_index_algorithm::{AlgorithmType, GasIndexAlgorithm};
#[cfg(feature = "drivers")]
use heapless::{String, Vec};

use super::EntityDescriptor;
#[cfg(feature = "drivers")]
use super::{crc8_sensirion, crc_word, Reading, MAX_READINGS};

/// I²C address. Both parts use it; there is no strapping option.
pub const ADDR: u8 = 0x59;

/// Measure both raw signals. SGP41 only: answers with VOC and NOx.
pub const CMD_MEASURE_RAW_SIGNALS: u16 = 0x2619;
/// Measure the VOC raw signal. Present on both parts; one word back.
pub const CMD_MEASURE_RAW: u16 = 0x260F;
/// Run the conditioning phase. SGP41 only; see [`CONDITIONING_STEPS`].
pub const CMD_EXECUTE_CONDITIONING: u16 = 0x2612;
/// Park the hotplate. Worth doing before a long idle to spare the element.
pub const CMD_TURN_HEATER_OFF: u16 = 0x3615;
/// 48-bit serial number, three words.
pub const CMD_GET_SERIAL_NUMBER: u16 = 0x3682;

/// Conversion time for `measure_raw_signals` / `execute_conditioning` (ms).
pub const MEASURE_MS: u64 = 50;
/// Conversion time for the SGP40's `measure_raw` (ms).
pub const MEASURE_SGP40_MS: u64 = 30;
/// Delay after a command that only writes.
pub const CMD_DELAY_MS: u64 = 2;

/// The interval [`Sgp41::sample_once`] must be driven at, in seconds.
///
/// Not a tunable. It is what the index algorithm is parameterised for and what
/// keeps the hotplate at a steady temperature; the two reasons happen to agree.
pub const SAMPLING_INTERVAL_SECS: f32 = 1.0;

/// 1 Hz steps of `execute_conditioning` before the first real measurement.
///
/// Sensirion's figure, and an upper bound rather than a target: conditioning
/// longer than this is explicitly discouraged.
pub const CONDITIONING_STEPS: u8 = 10;

/// Humidity ticks standing for 50 %RH, used until a real reading arrives.
pub const DEFAULT_RH_TICKS: u16 = 0x8000;
/// Temperature ticks standing for 25 °C, likewise.
pub const DEFAULT_T_TICKS: u16 = 0x6666;

/// The VOC channel, which every part on this address has.
pub const DESCRIPTORS: &[EntityDescriptor] = &[EntityDescriptor {
    key: "voc_index",
    name: "VOC-Index",
    // Deliberately unitless: 100 is "the last day's average here", which no
    // unit describes. `aqi` is the device class Home Assistant has for exactly
    // this kind of dimensionless air-quality number.
    unit: "",
    device_class: "aqi",
    state_class: "measurement",
}];

/// VOC plus the NOx channel, for a part that turned out to be an SGP41.
pub const DESCRIPTORS_WITH_NOX: &[EntityDescriptor] = &[
    EntityDescriptor {
        key: "voc_index",
        name: "VOC-Index",
        unit: "",
        device_class: "aqi",
        state_class: "measurement",
    },
    EntityDescriptor {
        key: "nox_index",
        name: "NOx-Index",
        unit: "",
        device_class: "aqi",
        state_class: "measurement",
    },
];

/// The descriptor set for a slot, depending on whether the fitted part has a
/// NOx channel. Mirrors [`crate::sensors::sds011::descriptors`].
///
/// This is a *static* answer to a question the driver can also settle at
/// runtime, and both are wanted: discovery is built from the node config, so
/// the entity set has to be known without a bus, while [`Sgp41::detect`] stops
/// a part without the channel from publishing a NOx value anyway. Get the
/// config wrong and the announcement digest re-announces on the next connect
/// once it is corrected -- see `discovery::announcement_tag`.
pub const fn descriptors(with_nox: bool) -> &'static [EntityDescriptor] {
    if with_nox {
        DESCRIPTORS_WITH_NOX
    } else {
        DESCRIPTORS
    }
}

/// Which part answered on [`ADDR`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Part {
    /// VOC only.
    Sgp40,
    /// VOC and NOx.
    Sgp41,
}

impl Part {
    /// Discovery metadata for the channels this part actually has.
    pub const fn descriptors(self) -> &'static [EntityDescriptor] {
        match self {
            Part::Sgp40 => DESCRIPTORS,
            Part::Sgp41 => DESCRIPTORS_WITH_NOX,
        }
    }
}

/// Relative humidity in ticks, as the part wants it: `RH% · 65535 / 100`.
///
/// Takes tenths of a percent, like the SHT31 reports, and clamps rather than
/// wrapping — a bad reading should feed the sensor a plausible humidity, not a
/// wildly wrong one.
pub const fn rh_ticks(rh_tenths: i32) -> u16 {
    let clamped = if rh_tenths < 0 {
        0
    } else if rh_tenths > 1000 {
        1000
    } else {
        rh_tenths
    };
    // Rounded, not truncated: 50 %RH is 32767.5 ticks, and the datasheet's
    // default word for it is 0x8000. Truncating would miss it by one.
    ((clamped as u32 * 65535 + 500) / 1000) as u16
}

/// Temperature in ticks: `(T°C + 45) · 65535 / 175`. Tenths in, clamped to the
/// range the encoding can represent.
pub const fn t_ticks(t_tenths: i32) -> u16 {
    let shifted = t_tenths + 450;
    let clamped = if shifted < 0 {
        0
    } else if shifted > 1750 {
        1750
    } else {
        shifted
    };
    ((clamped as u32 * 65535 + 875) / 1750) as u16
}

/// The last indices the sampling loop computed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Indices {
    /// 1..500, 100 being the running ~24 h average.
    pub voc: i32,
    /// 1..500, or `None` on an SGP40, which has no NOx channel.
    pub nox: Option<i32>,
}

#[cfg(feature = "drivers")]
pub struct Sgp41<I2C> {
    i2c: I2C,
    /// `None` until [`Self::detect`] has managed to ask.
    part: Option<Part>,
    voc: GasIndexAlgorithm,
    nox: GasIndexAlgorithm,
    latest: Option<Indices>,
    /// Counts down the conditioning steps; zero means measuring.
    conditioning_left: u8,
    /// Compensation handed over on the next sample.
    rh: u16,
    t: u16,
    fault: Option<&'static str>,
}

#[cfg(feature = "drivers")]
impl<I2C: I2cBus> Sgp41<I2C> {
    pub fn new(i2c: I2C) -> Self {
        Self {
            i2c,
            part: None,
            voc: GasIndexAlgorithm::new(AlgorithmType::Voc, SAMPLING_INTERVAL_SECS),
            nox: GasIndexAlgorithm::new(AlgorithmType::Nox, SAMPLING_INTERVAL_SECS),
            latest: None,
            conditioning_left: CONDITIONING_STEPS,
            rh: DEFAULT_RH_TICKS,
            t: DEFAULT_T_TICKS,
            fault: None,
        }
    }

    /// Feed the part the ambient conditions its own compensation needs.
    ///
    /// The raw signal drifts with humidity, so an SGP41 next to a warm board
    /// wants the *room's* humidity, not the enclosure's — same argument as the
    /// SDS011's correction, and on this fleet the same SHT31 supplies both.
    pub fn compensate(&mut self, rh_tenths: i32, t_tenths: i32) {
        self.rh = rh_ticks(rh_tenths);
        self.t = t_ticks(t_tenths);
    }

    /// Which part this is, once known.
    pub fn part(&self) -> Option<Part> {
        self.part
    }

    /// The most recent indices, or `None` while the algorithm is still in its
    /// 45-second blackout or the part has not answered yet.
    pub fn latest(&self) -> Option<Indices> {
        self.latest
    }

    pub fn fault(&self) -> Option<&'static str> {
        self.fault
    }

    /// Discovery metadata for whatever answered, defaulting to VOC-only until
    /// the part is known — announcing a NOx entity that turns out not to exist
    /// is worse than announcing it late.
    pub fn descriptors(&self) -> &'static [EntityDescriptor] {
        match self.part {
            Some(part) => part.descriptors(),
            None => DESCRIPTORS,
        }
    }

    async fn write_cmd(&mut self, cmd: u16) -> Option<()> {
        self.i2c.write(ADDR, &cmd.to_be_bytes()).await.ok()?;
        Timer::after(Duration::from_millis(CMD_DELAY_MS)).await;
        Some(())
    }

    /// A command plus the two CRC-guarded compensation words the measuring
    /// commands take.
    async fn write_measure_cmd(&mut self, cmd: u16) -> Option<()> {
        let [rh_msb, rh_lsb] = self.rh.to_be_bytes();
        let [t_msb, t_lsb] = self.t.to_be_bytes();
        let frame = [
            cmd.to_be_bytes()[0],
            cmd.to_be_bytes()[1],
            rh_msb,
            rh_lsb,
            crc8_sensirion(&[rh_msb, rh_lsb]),
            t_msb,
            t_lsb,
            crc8_sensirion(&[t_msb, t_lsb]),
        ];
        self.i2c.write(ADDR, &frame).await.ok()
    }

    /// The 48-bit serial number, mostly as an "is anything there" probe.
    pub async fn serial_number(&mut self) -> Option<u64> {
        self.write_cmd(CMD_GET_SERIAL_NUMBER).await?;
        let mut buf = [0u8; 9];
        self.i2c.read(ADDR, &mut buf).await.ok()?;
        let mut serial = 0u64;
        for chunk in buf.chunks(3) {
            serial = (serial << 16) | crc_word(chunk)? as u64;
        }
        Some(serial)
    }

    /// Work out whether this is an SGP41 or an SGP40, and remember it.
    ///
    /// Asks for both raw signals. An SGP41 returns two CRC-guarded words; an
    /// SGP40 does not have the command, so the write or the six-byte read
    /// fails, and it is asked for its single word instead.
    pub async fn detect(&mut self) -> Option<Part> {
        if let Some(part) = self.part {
            return Some(part);
        }
        if self.write_measure_cmd(CMD_MEASURE_RAW_SIGNALS).await.is_some() {
            Timer::after(Duration::from_millis(MEASURE_MS)).await;
            let mut buf = [0u8; 6];
            if self.i2c.read(ADDR, &mut buf).await.is_ok()
                && crc_word(&buf[0..3]).is_some()
                && crc_word(&buf[3..6]).is_some()
            {
                self.part = Some(Part::Sgp41);
                self.fault = None;
                return self.part;
            }
        }
        if self.write_measure_cmd(CMD_MEASURE_RAW).await.is_some() {
            Timer::after(Duration::from_millis(MEASURE_SGP40_MS)).await;
            let mut buf = [0u8; 3];
            if self.i2c.read(ADDR, &mut buf).await.is_ok() && crc_word(&buf).is_some() {
                self.part = Some(Part::Sgp40);
                // An SGP40 has nothing to condition, so do not wait for it.
                self.conditioning_left = 0;
                self.fault = None;
                return self.part;
            }
        }
        self.fault = Some("no SGP4x answered at 0x59; check SDA/SCL and 3V3");
        None
    }

    /// One 1 Hz step: condition if still needed, otherwise measure and feed the
    /// index algorithms.
    ///
    /// Call this once a second. Calling it slower does not produce a slower
    /// index, it produces a wrong one — see the module note.
    pub async fn sample_once(&mut self) {
        let Some(part) = self.detect().await else {
            self.latest = None;
            return;
        };

        // The conditioning phase runs the hotplate without trusting what comes
        // back, so its reading is read and dropped rather than fed to the index.
        if self.conditioning_left > 0 {
            if self.write_measure_cmd(CMD_EXECUTE_CONDITIONING).await.is_some() {
                Timer::after(Duration::from_millis(MEASURE_MS)).await;
                let mut buf = [0u8; 3];
                let _ = self.i2c.read(ADDR, &mut buf).await;
            }
            self.conditioning_left -= 1;
            return;
        }

        let (cmd, delay, len) = match part {
            Part::Sgp41 => (CMD_MEASURE_RAW_SIGNALS, MEASURE_MS, 6usize),
            Part::Sgp40 => (CMD_MEASURE_RAW, MEASURE_SGP40_MS, 3usize),
        };
        if self.write_measure_cmd(cmd).await.is_none() {
            self.fault = Some("SGP4x stopped acknowledging its measure command");
            return;
        }
        Timer::after(Duration::from_millis(delay)).await;

        let mut buf = [0u8; 6];
        if self.i2c.read(ADDR, &mut buf[..len]).await.is_err() {
            self.fault = Some("SGP4x acknowledged but returned nothing");
            return;
        }
        let Some(sraw_voc) = crc_word(&buf[0..3]) else {
            self.fault = Some("SGP4x CRC mismatch; the bus is noisy");
            return;
        };
        self.fault = None;

        // Zero is the algorithm's "still in its blackout" answer, not an index.
        let voc = self.voc.process(sraw_voc as i32);
        let nox = if len == 6 {
            crc_word(&buf[3..6]).map(|sraw| self.nox.process(sraw as i32))
        } else {
            None
        };
        self.latest = if voc > 0 {
            Some(Indices {
                voc,
                nox: nox.filter(|n| *n > 0),
            })
        } else {
            None
        };
    }

    /// Park the hotplate. Not needed on a mains node that samples forever, but
    /// the right thing before a long idle.
    pub async fn turn_heater_off(&mut self) {
        let _ = self.write_cmd(CMD_TURN_HEATER_OFF).await;
    }

    /// The readings to publish, or an empty `Vec` if there is nothing to say
    /// yet — the same contract the [`super::Sensor`] drivers use.
    pub fn readings(&self) -> Vec<Reading, MAX_READINGS> {
        let mut out = Vec::new();
        let Some(indices) = self.latest else {
            return out;
        };
        let mut voc: String<16> = String::new();
        if write_index(&mut voc, indices.voc).is_ok() {
            let _ = out.push(Reading {
                key: "voc_index",
                value: voc,
            });
        }
        if let Some(nox) = indices.nox {
            let mut value: String<16> = String::new();
            if write_index(&mut value, nox).is_ok() {
                let _ = out.push(Reading {
                    key: "nox_index",
                    value,
                });
            }
        }
        out
    }
}

/// Format an index as a plain integer. Float-free, like every other value this
/// firmware publishes.
#[cfg(feature = "drivers")]
fn write_index(buf: &mut String<16>, index: i32) -> Result<(), core::fmt::Error> {
    use core::fmt::Write as _;
    write!(buf, "{}", index)
}

#[cfg(all(test, feature = "drivers"))]
mod tests {
    use super::*;
    use crate::sensors::mock::{block_on, FakeI2c};

    /// Sensirion's own worked examples for the compensation encoding.
    #[test]
    fn compensation_matches_the_datasheet_defaults() {
        assert_eq!(rh_ticks(500), DEFAULT_RH_TICKS);
        assert_eq!(t_ticks(250), DEFAULT_T_TICKS);
    }

    #[test]
    fn compensation_clamps_instead_of_wrapping() {
        // A broken SHT31 read must feed the part a plausible humidity, not a
        // number that wraps to the other end of the scale.
        assert_eq!(rh_ticks(-200), 0);
        assert_eq!(rh_ticks(1400), u16::MAX);
        assert_eq!(t_ticks(-900), 0);
        assert_eq!(t_ticks(2000), u16::MAX);
    }

    fn word(value: u16) -> [u8; 3] {
        let [msb, lsb] = value.to_be_bytes();
        [msb, lsb, crc8_sensirion(&[msb, lsb])]
    }

    fn two_words(a: u16, b: u16) -> heapless::Vec<u8, 8> {
        let mut v = heapless::Vec::new();
        v.extend_from_slice(&word(a)).unwrap();
        v.extend_from_slice(&word(b)).unwrap();
        v
    }

    /// Six bytes that fail their CRC, which is how a part without
    /// `measure_raw_signals` looks from here: it does not answer that command
    /// with anything the driver can believe. (The mock insists a scripted reply
    /// is exactly as long as the driver asked for -- deliberately, so a test
    /// cannot quietly disagree with the wire -- so "nothing valid" has to be
    /// expressed as bad data rather than as a short read.)
    fn nothing_valid() -> std::vec::Vec<u8> {
        let mut bytes = word(30000).to_vec();
        bytes.extend_from_slice(&word(15000));
        // Break both CRCs.
        bytes[2] ^= 0xFF;
        bytes[5] ^= 0xFF;
        bytes
    }

    #[test]
    fn two_words_back_means_sgp41() {
        let bus = FakeI2c::new(ADDR, [two_words(30000, 15000).to_vec()]);
        let mut sensor = Sgp41::new(bus);
        assert_eq!(block_on(sensor.detect()), Some(Part::Sgp41));
        assert_eq!(sensor.descriptors().len(), 2);
    }

    /// The listing this part was bought from advertised an SGP40 and shipped
    /// something with a NOx channel. The reverse has to work too: a real SGP40
    /// must announce one entity, not a NOx reading it cannot take.
    #[test]
    fn one_word_back_means_sgp40() {
        // Nothing for the six-byte read, then a single word for `measure_raw`.
        let bus = FakeI2c::new(ADDR, [nothing_valid(), word(30000).to_vec()]);
        let mut sensor = Sgp41::new(bus);
        assert_eq!(block_on(sensor.detect()), Some(Part::Sgp40));
        assert_eq!(sensor.descriptors().len(), 1);
        assert!(sensor
            .descriptors()
            .iter()
            .all(|d| d.key != "nox_index"));
    }

    #[test]
    fn nothing_on_the_bus_is_reported_as_wiring() {
        let mut sensor = Sgp41::new(FakeI2c::empty());
        assert_eq!(block_on(sensor.detect()), None);
        assert!(sensor.fault().unwrap().contains("0x59"));
        assert!(sensor.latest().is_none());
    }

    #[test]
    fn an_sgp40_skips_the_conditioning_it_does_not_have() {
        let bus = FakeI2c::new(ADDR, [nothing_valid(), word(30000).to_vec()]);
        let mut sensor = Sgp41::new(bus);
        assert_eq!(block_on(sensor.detect()), Some(Part::Sgp40));
        assert_eq!(sensor.conditioning_left, 0);
    }

    /// The algorithm answers 0 for its first 45 s. Publishing that would put a
    /// zero on a scale whose minimum is 1, so it must read as "nothing yet".
    #[test]
    fn the_initial_blackout_publishes_nothing() {
        let replies = core::iter::repeat_with(|| two_words(30000, 15000).to_vec()).take(4);
        let bus = FakeI2c::new(ADDR, replies);
        let mut sensor = Sgp41::new(bus);
        sensor.conditioning_left = 0;
        block_on(sensor.sample_once());
        assert!(sensor.latest().is_none(), "blackout must not publish");
        assert!(sensor.readings().is_empty());
    }

    #[test]
    fn descriptor_keys_match_the_readings_they_promise() {
        // Discovery announces `descriptors()`; `readings()` must not invent a
        // key that was never announced, or Home Assistant drops the value.
        for part in [Part::Sgp40, Part::Sgp41] {
            for desc in part.descriptors() {
                assert!(
                    ["voc_index", "nox_index"].contains(&desc.key),
                    "unexpected key {}",
                    desc.key
                );
                assert_eq!(desc.state_class, "measurement");
                assert_eq!(desc.device_class, "aqi");
            }
        }
    }
}

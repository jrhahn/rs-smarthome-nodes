//! SDS011 particulate-matter driver (UART, #15).
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
//! sets sleep/work; the cycle is: wake fan -> let the airflow establish -> read
//! frames until they settle -> put the sensor back to sleep. How long the
//! middle step takes is decided by the readings themselves rather than by a
//! fixed guess (see [`Sds011::warm_up`]). The sensor must also be kept dry
//! (condensation ruins both reading and hardware).

#[cfg(feature = "drivers")]
use embassy_time::{with_timeout, Duration, Instant, Timer};
#[cfg(feature = "drivers")]
use embedded_io_async::{Read, Write};
#[cfg(feature = "drivers")]
use heapless::{String, Vec};

use super::EntityDescriptor;
#[cfg(feature = "drivers")]
use super::{write_tenths, Reading, Sensor, MAX_READINGS};

/// Frame markers.
pub const HEAD: u8 = 0xAA;
pub const TAIL: u8 = 0xAB;
/// Command byte for a measurement-data reply.
pub const CMD_DATA: u8 = 0xC0;
pub const FRAME_LEN: usize = 10;
/// Length of a host->sensor command frame.
pub const CMD_FRAME_LEN: usize = 19;
/// Shortest the fan must run before a frame means anything.
///
/// Below this the airflow has not established and the sensor reports a stable
/// but meaningless value — usually whatever it last saw — so a settling check
/// would happily agree with itself and stop early.
pub const MIN_WARMUP_SECS: u64 = 10;
/// Ceiling on the warm-up. Reached when the air genuinely will not settle
/// (someone is cooking), in which case the latest frame is still the best
/// answer available and is worth more than the fan time to keep waiting.
pub const MAX_WARMUP_SECS: u64 = 30;
/// Consecutive frames that must agree before the reading counts as settled.
/// One repeat is luck; three in a row is airflow.
const STABLE_FRAMES: u32 = 3;
/// Two readings agree within 1 µg/m³ plus 5 % of the larger: an absolute floor
/// for clean air, where the sensor's own noise dominates, and a proportional
/// band for dirty air, where it does not.
const AGREE_TENTHS: i32 = 10;
const AGREE_PERCENT: i32 = 5;
/// How long to wait for a measurement frame once the fan has warmed up. The
/// sensor reports every ~1 s, so this is generous; exceeding it means silence.
#[cfg(feature = "drivers")]
pub const FRAME_TIMEOUT: Duration = Duration::from_secs(5);
/// A gap this long with no byte means the receive buffer is empty, i.e. the
/// stale pre-warm-up frames have been flushed (frames arrive ~1 s apart).
#[cfg(feature = "drivers")]
const DRAIN_QUIET: Duration = Duration::from_millis(100);
/// Consecutive zero-length reads tolerated before giving up.
///
/// A UART that keeps *completing* a read with no bytes never yields, so the
/// surrounding timeout could never fire and the driver would spin for ever.
/// esp-hal's UART does not do this — it waits for at least one byte — but the
/// trait permits it, and a bound is three lines.
#[cfg(feature = "drivers")]
const EMPTY_READ_LIMIT: usize = 64;

pub const DESCRIPTORS: &[EntityDescriptor] = &[
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

/// Build the sleep/work command frame: `AA B4 06 01 <mode> 00×10 FF FF CHK AB`,
/// where `mode` is 1 for "work" (fan + laser on) and 0 for "sleep", the two
/// `FF`s address every sensor on the line, and `CHK` sums bytes 2..=16.
pub const fn sleep_work_frame(work: bool) -> [u8; CMD_FRAME_LEN] {
    let mut f = [0u8; CMD_FRAME_LEN];
    f[0] = HEAD;
    f[1] = 0xB4;
    f[2] = 0x06; // set sleep/work
    f[3] = 0x01; // 1 = set value (0 would only query)
    f[4] = work as u8;
    f[15] = 0xFF; // device id: broadcast
    f[16] = 0xFF;
    let mut sum: u8 = 0;
    let mut i = 2;
    while i <= 16 {
        sum = sum.wrapping_add(f[i]);
        i += 1;
    }
    f[17] = sum;
    f[18] = TAIL;
    f
}

/// Do two frames report the same air, within the sensor's own noise?
pub fn frames_agree(a: &[u8; FRAME_LEN], b: &[u8; FRAME_LEN]) -> bool {
    agree(pm_tenths(a[2], a[3]), pm_tenths(b[2], b[3]))
        && agree(pm_tenths(a[4], a[5]), pm_tenths(b[4], b[5]))
}

/// Are two values in tenths of µg/m³ the same reading, allowing for noise?
pub const fn agree(a: i32, b: i32) -> bool {
    let larger = if a > b { a } else { b };
    let tolerance = AGREE_TENTHS + larger * AGREE_PERCENT / 100;
    let difference = if a > b { a - b } else { b - a };
    difference <= tolerance
}

/// SDS011 on a UART, generic over the byte stream so the driver stays
/// HAL-agnostic (esp-hal's async `Uart` implements `embedded-io-async`).
#[cfg(feature = "drivers")]
pub struct Sds011<U> {
    uart: U,
    /// Fan time before any frame is trusted (see [`MIN_WARMUP_SECS`]).
    pub min_warmup: Duration,
    /// Hard ceiling on the warm-up (see [`MAX_WARMUP_SECS`]).
    pub max_warmup: Duration,
    /// How long to wait for a frame once the fan has warmed up. Configurable
    /// for the same reason as the warm-up — and so a test does not have to sit
    /// through the real thing.
    pub frame_timeout: Duration,
}

#[cfg(feature = "drivers")]
impl<U: Read + Write> Sds011<U> {
    pub fn new(uart: U) -> Self {
        Self {
            uart,
            min_warmup: Duration::from_secs(MIN_WARMUP_SECS),
            max_warmup: Duration::from_secs(MAX_WARMUP_SECS),
            frame_timeout: FRAME_TIMEOUT,
        }
    }

    /// Turn the fan + laser on (`true`) or park the sensor (`false`).
    async fn set_work(&mut self, work: bool) -> Option<()> {
        self.uart.write_all(&sleep_work_frame(work)).await.ok()?;
        self.uart.flush().await.ok()?;
        Some(())
    }

    /// Read a single byte, awaiting the UART until one arrives.
    async fn read_byte(&mut self) -> Option<u8> {
        let mut b = [0u8; 1];
        for _ in 0..EMPTY_READ_LIMIT {
            match self.uart.read(&mut b).await {
                Ok(0) => continue,
                Ok(_) => return Some(b[0]),
                Err(_) => return None,
            }
        }
        None
    }

    /// Throw away whatever is already buffered: frames the sensor emitted
    /// during (or before) the warm-up describe air we don't want to report, and
    /// the reply to a sleep/work command sits in the same stream.
    async fn drain(&mut self) {
        let mut scratch = [0u8; 64];
        // Bounded for the same reason as `read_byte`: a read that completes
        // instantly with nothing would otherwise loop without ever yielding.
        for _ in 0..EMPTY_READ_LIMIT {
            match with_timeout(DRAIN_QUIET, self.uart.read(&mut scratch)).await {
                Ok(Ok(1..)) => continue,
                // Quiet line, error, or a read that returned nothing: done.
                _ => return,
            }
        }
    }

    /// Resynchronise on `HEAD` and read one complete, checksum-valid frame.
    /// Loops past malformed frames; the caller bounds it with a timeout.
    async fn read_frame(&mut self) -> Option<[u8; FRAME_LEN]> {
        loop {
            // Hunt for the start marker; anything else is mid-frame noise.
            while self.read_byte().await? != HEAD {}

            let mut frame = [0u8; FRAME_LEN];
            frame[0] = HEAD;
            for slot in frame.iter_mut().skip(1) {
                *slot = self.read_byte().await?;
            }
            if frame_ok(&frame) {
                return Some(frame);
            }
        }
    }

    /// Run the fan until the readings settle, and return the last frame seen.
    ///
    /// The old fixed 20 s was a guess in both directions: too long for still
    /// air, too short for air that is actually changing. Instead the driver
    /// serves the minimum airflow time, then watches until consecutive frames
    /// agree — typically ~3 s later, which is fan life back — and gives up at
    /// the ceiling with whatever it has, since an unsettled reading still beats
    /// none.
    async fn warm_up(&mut self) -> Option<[u8; FRAME_LEN]> {
        Timer::after(self.min_warmup).await;
        // Everything said during the spin-up describes air that was not moving
        // through the sensor yet.
        self.drain().await;

        let deadline = Instant::now() + self.max_warmup;
        let mut last: Option<[u8; FRAME_LEN]> = None;
        let mut agreeing = 0u32;
        loop {
            // One frame is always read, however tight the budget: an unsettled
            // reading is worth more than an empty round.
            let Ok(Some(frame)) = with_timeout(self.frame_timeout, self.read_frame()).await else {
                // Silence: the sensor stopped talking, so this is all there is.
                break;
            };
            agreeing = match last {
                Some(previous) if frames_agree(&previous, &frame) => agreeing + 1,
                _ => 1,
            };
            last = Some(frame);
            if agreeing >= STABLE_FRAMES || Instant::now() >= deadline {
                break;
            }
        }
        last
    }

    fn push(readings: &mut Vec<Reading, MAX_READINGS>, key: &'static str, tenths: i32) {
        let mut value = String::new();
        write_tenths(&mut value, tenths);
        let _ = readings.push(Reading { key, value });
    }
}

#[cfg(feature = "drivers")]
impl<U: Read + Write> Sensor for Sds011<U> {
    fn kind(&self) -> &'static str {
        "SDS011"
    }

    fn descriptors(&self) -> &'static [EntityDescriptor] {
        DESCRIPTORS
    }

    async fn measure(&mut self) -> Vec<Reading, MAX_READINGS> {
        let mut out = Vec::new();

        if self.set_work(true).await.is_none() {
            return out;
        }

        let frame = self.warm_up().await;

        // Park the fan again whatever happened — its 8000 h life is the scarce
        // resource here, so it must never be left running by an error path.
        let _ = self.set_work(false).await;

        if let Some(f) = frame {
            Self::push(&mut out, "pm25", pm_tenths(f[2], f[3]));
            Self::push(&mut out, "pm10", pm_tenths(f[4], f[5]));
        }
        out
    }
}

const _: () = {
    // 0x00F5 = 245 -> 24.5 µg/m³ (245 tenths)
    assert!(pm_tenths(0xF5, 0x00) == 245);
    assert!(pm_tenths(0x00, 0x01) == 256);
    // Checksum of the canonical "start working" frame from the protocol sheet.
    let work = sleep_work_frame(true);
    assert!(
        work[4] == 1
            && work[17]
                == 0x06u8
                    .wrapping_add(0x01)
                    .wrapping_add(0x01)
                    .wrapping_add(0xFF)
                    .wrapping_add(0xFF)
    );
    let sleep = sleep_work_frame(false);
    assert!(sleep[4] == 0 && sleep[18] == TAIL);
};

#[cfg(test)]
mod tests {
    use super::*;
    // `super::*` brings in heapless's `Vec` once the drivers are compiled; the
    // tests want the growable one.
    #[allow(unused_imports)]
    use std::{string::String, vec::Vec};

    /// Build a well-formed measurement frame for the given raw PM words.
    fn frame(pm25: u16, pm10: u16) -> [u8; FRAME_LEN] {
        let mut f = [0u8; FRAME_LEN];
        f[0] = HEAD;
        f[1] = CMD_DATA;
        f[2..4].copy_from_slice(&pm25.to_le_bytes());
        f[4..6].copy_from_slice(&pm10.to_le_bytes());
        f[6] = 0xA1; // device id, not covered by the reading
        f[7] = 0xB2;
        f[8] = f[2..8].iter().fold(0u8, |a, b| a.wrapping_add(*b));
        f[9] = TAIL;
        f
    }

    #[test]
    fn a_well_formed_frame_is_accepted() {
        assert!(frame_ok(&frame(245, 1000)));
    }

    #[test]
    fn a_frame_is_rejected_on_any_marker_or_checksum_error() {
        let good = frame(245, 1000);
        // Markers and command byte.
        for (i, wrong) in [(0, 0x00), (1, 0xC5), (9, 0x00)] {
            let mut f = good;
            f[i] = wrong;
            assert!(!frame_ok(&f), "byte {i} = {wrong:#04x} accepted");
        }
        // A single bit flipped anywhere in the checksummed payload.
        for byte in 2..8 {
            for bit in 0..8 {
                let mut f = good;
                f[byte] ^= 1 << bit;
                assert!(!frame_ok(&f), "bit {bit} of byte {byte} accepted");
            }
        }
        // ... and a checksum that simply does not match.
        let mut f = good;
        f[8] = f[8].wrapping_add(1);
        assert!(!frame_ok(&f));
    }

    #[test]
    fn pm_words_decode_as_tenths() {
        // The protocol reports tenths of µg/m³, little-endian.
        assert_eq!(pm_tenths(0xF5, 0x00), 245); // 24.5 µg/m³
        assert_eq!(pm_tenths(0x00, 0x01), 256);
        assert_eq!(pm_tenths(0x00, 0x00), 0);
        assert_eq!(pm_tenths(0xFF, 0xFF), 65535);
        // Byte order matters: swapping the two must not give the same number.
        assert_ne!(pm_tenths(0x01, 0x02), pm_tenths(0x02, 0x01));
    }

    #[test]
    fn a_decoded_frame_reports_both_particle_sizes_independently() {
        let f = frame(123, 4567);
        assert_eq!(pm_tenths(f[2], f[3]), 123);
        assert_eq!(pm_tenths(f[4], f[5]), 4567);
    }

    #[test]
    fn the_sleep_and_work_commands_differ_only_where_they_should() {
        let work = sleep_work_frame(true);
        let sleep = sleep_work_frame(false);

        for f in [&work, &sleep] {
            assert_eq!(f[0], HEAD);
            assert_eq!(f[1], 0xB4);
            assert_eq!(f[2], 0x06); // set sleep/work
            assert_eq!(f[3], 0x01); // set, not query
            assert_eq!([f[15], f[16]], [0xFF, 0xFF]); // broadcast
            assert_eq!(f[18], TAIL);
            let sum = f[2..17].iter().fold(0u8, |a, b| a.wrapping_add(*b));
            assert_eq!(f[17], sum, "checksum");
        }

        assert_eq!(work[4], 1);
        assert_eq!(sleep[4], 0);
        // The mode byte and the checksum are the only difference — a frame that
        // differed elsewhere would be addressing something else entirely.
        let differing: Vec<usize> = (0..CMD_FRAME_LEN)
            .filter(|&i| work[i] != sleep[i])
            .collect();
        assert_eq!(differing, vec![4, 17]);
    }

    // --- Driver, against a scripted UART -------------------------------------

    #[cfg(feature = "drivers")]
    fn sensor(segments: Vec<super::super::mock::Segment>) -> Sds011<super::super::mock::FakeUart> {
        let mut sensor = Sds011::new(super::super::mock::FakeUart::new(segments));
        // The fan spin-up and the patience for a stubborn sensor are the two
        // things a test must not sit through.
        sensor.min_warmup = Duration::from_ticks(0);
        sensor.max_warmup = Duration::from_secs(1);
        sensor.frame_timeout = Duration::from_millis(50);
        sensor
    }

    #[cfg(feature = "drivers")]
    fn readings(sensor: &mut Sds011<super::super::mock::FakeUart>) -> Vec<(&'static str, String)> {
        use super::super::mock::block_on;
        use super::super::Sensor;
        block_on(sensor.measure())
            .iter()
            .map(|r| (r.key, r.value.to_string()))
            .collect()
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn a_frame_read_after_the_warm_up_becomes_two_readings() {
        use super::super::mock::Segment;

        let mut sensor = sensor(vec![
            // What the sensor said while the fan was still spinning up.
            Segment::now(frame(9999, 9999).to_vec()),
            // ... and the frame that describes air actually flowing through it.
            Segment::after_a_gap(frame(245, 1000).to_vec()),
        ]);

        assert_eq!(
            readings(&mut sensor),
            vec![("pm25", "24.5".to_string()), ("pm10", "100.0".to_string())]
        );
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn frames_buffered_during_the_warm_up_are_discarded() {
        use super::super::mock::Segment;

        // Three stale frames arrive before the airflow settles. Reporting the
        // first one would publish the air from the *previous* round.
        let mut stale = Vec::new();
        for _ in 0..3 {
            stale.extend_from_slice(&frame(9999, 9999));
        }
        let mut sensor = sensor(vec![
            Segment::now(stale),
            Segment::after_a_gap(frame(120, 300).to_vec()),
        ]);

        assert_eq!(
            readings(&mut sensor),
            vec![("pm25", "12.0".to_string()), ("pm10", "30.0".to_string())]
        );
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn the_fan_is_woken_and_parked_again() {
        use super::super::mock::Segment;

        let mut sensor = sensor(vec![Segment::after_a_gap(frame(245, 1000).to_vec())]);
        let _ = readings(&mut sensor);

        // The 8000 h fan life is the scarce resource: it must be asked to work
        // and then put back to sleep, in that order.
        assert_eq!(
            sensor.uart.commands(),
            &[
                sleep_work_frame(true).to_vec(),
                sleep_work_frame(false).to_vec()
            ]
        );
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn the_fan_is_parked_even_when_nothing_is_read() {
        use super::super::mock::Segment;

        // Every exit path, because a fan left spinning is the one failure that
        // costs hardware rather than a reading.
        let cases: Vec<Vec<super::super::mock::Segment>> = vec![
            // Silence after the warm-up.
            vec![],
            // A frame with a broken checksum, and nothing after it.
            vec![Segment::after_a_gap({
                let mut f = frame(245, 1000).to_vec();
                f[8] ^= 0xFF;
                f
            })],
            // Nothing but noise.
            vec![Segment::after_a_gap(vec![0x11, 0x22, 0x33, 0x44])],
        ];

        for (i, segments) in cases.into_iter().enumerate() {
            let mut sensor = sensor(segments);
            assert!(
                readings(&mut sensor).is_empty(),
                "case {i} published something"
            );
            assert_eq!(
                sensor.uart.commands().last(),
                Some(&sleep_work_frame(false).to_vec()),
                "case {i} left the fan running"
            );
        }
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn the_reader_resynchronises_past_junk() {
        use super::super::mock::Segment;

        // A mid-frame start and line noise, as a UART that was opened partway
        // through a transmission hands over.
        let mut stream = vec![0x00, 0xC0, 0x12, 0x99];
        stream.extend_from_slice(&frame(245, 1000));

        let mut sensor = sensor(vec![Segment::after_a_gap(stream)]);
        assert_eq!(
            readings(&mut sensor),
            vec![("pm25", "24.5".to_string()), ("pm10", "100.0".to_string())]
        );
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn a_frame_that_fails_its_checksum_is_skipped_for_the_next_one() {
        use super::super::mock::Segment;

        let mut corrupt = frame(9999, 9999);
        corrupt[3] ^= 0x20; // payload changed, checksum now wrong
        let mut stream = corrupt.to_vec();
        stream.extend_from_slice(&frame(245, 1000));

        let mut sensor = sensor(vec![Segment::after_a_gap(stream)]);
        assert_eq!(
            readings(&mut sensor),
            vec![("pm25", "24.5".to_string()), ("pm10", "100.0".to_string())]
        );
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn a_false_head_byte_costs_one_frame_and_no_more() {
        use super::super::mock::Segment;

        // A stray 0xAA in the noise makes the reader treat the *next* frame as
        // that frame's body, so it is lost. What matters is that it recovers on
        // the frame after — the sensor sends one a second.
        let mut stream = vec![HEAD, 0xFF];
        stream.extend_from_slice(&frame(9999, 9999));
        stream.extend_from_slice(&frame(245, 1000));

        let mut sensor = sensor(vec![Segment::after_a_gap(stream)]);
        assert_eq!(
            readings(&mut sensor),
            vec![("pm25", "24.5".to_string()), ("pm10", "100.0".to_string())]
        );
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn a_disconnected_sensor_is_silent_rather_than_fatal() {
        use super::super::mock::{block_on, FakeUart};
        use super::super::Sensor;

        // The write fails, so there is nothing to wait for and nothing to park.
        let mut sensor = Sds011::new(FakeUart::disconnected());
        sensor.min_warmup = Duration::from_ticks(0);
        assert!(block_on(sensor.measure()).is_empty());
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn a_uart_that_never_yields_cannot_wedge_the_driver() {
        use super::super::mock::{block_on, StarvedUart};
        use super::super::Sensor;

        // A read that keeps completing with zero bytes never returns Pending,
        // so the surrounding timeout could never fire: without a bound on the
        // empty reads this call would spin for ever and take the node with it.
        let mut sensor = Sds011::new(StarvedUart);
        sensor.min_warmup = Duration::from_ticks(0);
        sensor.max_warmup = Duration::from_millis(50);
        sensor.frame_timeout = Duration::from_millis(50);
        assert!(block_on(sensor.measure()).is_empty());
    }

    // --- Adaptive warm-up ----------------------------------------------------

    #[test]
    fn agreement_allows_noise_but_not_a_real_change() {
        // Clean air: the absolute floor does the work, since 5 % of nothing is
        // nothing.
        assert!(agree(0, 10));
        assert!(!agree(0, 20));
        // Ordinary indoor air, ±0.5 µg/m³ of jitter.
        assert!(agree(200, 205));
        assert!(agree(205, 200)); // symmetric
        assert!(!agree(200, 260));
        // Dirty air, where 1 µg/m³ of tolerance would never settle.
        assert!(agree(2000, 2090));
        assert!(!agree(2000, 2200));
        // A value compared with itself always agrees, at any magnitude.
        for value in [0, 1, 245, 9999, 65535] {
            assert!(agree(value, value));
        }
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn frames_agree_on_both_particle_sizes() {
        // PM2.5 settling while PM10 is still climbing is not a settled reading.
        assert!(frames_agree(&frame(245, 1000), &frame(248, 1010)));
        assert!(!frames_agree(&frame(245, 1000), &frame(248, 1400)));
        assert!(!frames_agree(&frame(245, 1000), &frame(600, 1010)));
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn a_settled_reading_stops_the_fan_early() {
        use super::super::mock::Segment;

        // Three frames that agree are enough. The fourth is wildly different
        // and must never be reached — reading it would mean the driver kept the
        // fan running past the point it had its answer.
        let mut stream = Vec::new();
        for pm in [245, 247, 246, 9999] {
            stream.extend_from_slice(&frame(pm, 1000));
        }

        let mut sensor = sensor(vec![Segment::after_a_gap(stream)]);
        assert_eq!(
            readings(&mut sensor),
            vec![("pm25", "24.6".to_string()), ("pm10", "100.0".to_string())]
        );
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn it_keeps_watching_while_the_air_is_still_changing() {
        use super::super::mock::Segment;

        // The old fixed warm-up would have grabbed the first of these and
        // published a number the air had already left behind.
        let mut stream = Vec::new();
        for pm in [500, 300, 200, 205, 203] {
            stream.extend_from_slice(&frame(pm, 1000));
        }

        let mut sensor = sensor(vec![Segment::after_a_gap(stream)]);
        assert_eq!(
            readings(&mut sensor),
            vec![("pm25", "20.3".to_string()), ("pm10", "100.0".to_string())]
        );
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn the_ceiling_stops_the_watching_but_still_reports() {
        use super::super::mock::Segment;

        // Air that will not settle — someone is cooking — must not spin the fan
        // for ever. One frame is always read, and an unsettled reading beats an
        // empty round.
        let mut stream = Vec::new();
        for pm in [500, 100] {
            stream.extend_from_slice(&frame(pm, 1000));
        }

        let mut sensor = sensor(vec![Segment::after_a_gap(stream)]);
        sensor.max_warmup = Duration::from_ticks(0);
        assert_eq!(
            readings(&mut sensor),
            vec![("pm25", "50.0".to_string()), ("pm10", "100.0".to_string())]
        );
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn the_minimum_warm_up_is_served_before_anything_is_believed() {
        use super::super::mock::Segment;
        use embassy_time::Instant;

        // Frames arriving during the spin-up agree with each other perfectly —
        // the sensor repeats its last value — so without a floor the settling
        // check would stop immediately on air that is not moving yet.
        let mut stale = Vec::new();
        for _ in 0..3 {
            stale.extend_from_slice(&frame(9999, 9999));
        }

        let mut sensor = sensor(vec![
            Segment::now(stale),
            Segment::after_a_gap({
                let mut stream = Vec::new();
                for pm in [245, 246, 245] {
                    stream.extend_from_slice(&frame(pm, 1000));
                }
                stream
            }),
        ]);
        sensor.min_warmup = Duration::from_millis(120);

        let started = Instant::now();
        let values = readings(&mut sensor);
        assert!(Instant::now() - started >= Duration::from_millis(120));
        assert_eq!(values[0], ("pm25", "24.5".to_string()));
    }
}

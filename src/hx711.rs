//! Non-blocking driver for the HX711 24-bit load-cell amplifier.
//!
//! The HX711 has no data bus: it is read by bit-banging a clock line while
//! sampling a data line. This driver keeps the *waiting* for a conversion
//! fully async (so the Embassy executor stays free to service Wi-Fi, timers,
//! etc.) while performing the actual 24-clock read cycle as a short blocking
//! critical section. The datasheet mandates that a single `PD_SCK` high pulse
//! never exceeds 60 µs — otherwise the chip enters power-down — so the tight
//! read loop must *not* yield to the executor mid-transfer.
//!
//! The pins and the delay come in as `embedded-hal` traits rather than esp-hal
//! types, for the same reason the bus drivers do: the bit-level protocol — 24
//! bits most-significant-first, sampled while the clock is high, followed by
//! the gain-select pulses — is then exercised against fake pins on the host.
//! Get the bit order or the pulse count wrong and the scale reads plausible
//! nonsense, which is the worst kind of wrong.

use embassy_time::{Duration, Instant, Timer};
use embedded_hal::delay::DelayNs;
use embedded_hal::digital::{InputPin, OutputPin};

/// Holding `PD_SCK` high for longer than 60 µs latches the HX711 into
/// power-down (datasheet). Use a comfortable margin.
const POWER_DOWN_US: u32 = 80;

/// Gain / channel selection, encoded as the number of extra clock pulses that
/// follow the 24 data pulses (25/26/27).
///
/// Only `A128` is used by the bird scale, but the full set is exposed so the
/// driver is reusable for the HX711's second channel.
#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Gain {
    /// Channel A, gain 128 (25 total pulses).
    A128 = 1,
    /// Channel B, gain 32 (26 total pulses).
    B32 = 2,
    /// Channel A, gain 64 (27 total pulses).
    A64 = 3,
}

/// Driver wrapping the HX711 data (`DT`) input and serial-clock (`SCK`) output.
pub struct Hx711<DT, SCK, D> {
    dt: DT,
    sck: SCK,
    gain: Gain,
    delay: D,
}

impl<DT: InputPin, SCK: OutputPin, D: DelayNs> Hx711<DT, SCK, D> {
    /// Create a new driver. The clock line is driven low to keep the device
    /// out of power-down after construction.
    pub fn new(dt: DT, mut sck: SCK, delay: D) -> Self {
        let _ = sck.set_low();
        Self {
            dt,
            sck,
            gain: Gain::A128,
            delay,
        }
    }

    /// Select the channel / gain used for the *next* conversion.
    #[allow(dead_code)]
    pub fn set_gain(&mut self, gain: Gain) {
        self.gain = gain;
    }

    /// Put the HX711 into its ~0.3 µA power-down state by parking `PD_SCK` high.
    ///
    /// NOTE: on the ESP32-C3 the pad level is *not* retained across deep sleep
    /// unless RTC GPIO hold is enabled, so this only saves power while the MCU
    /// stays awake. Enabling hold on `SCK` to keep the chip powered down through
    /// deep sleep is the follow-up battery optimisation (see issue #5).
    ///
    /// Light sleep *would* retain the level, and calling this between polls was
    /// tried on that basis. It had to come back out: `Rtc::sleep_light` resets
    /// this chip rather than resuming (see the note in `main`'s `run_battery`),
    /// so there is currently no sleep on this board that both retains the pad
    /// and returns.
    #[allow(dead_code)]
    pub fn power_down(&mut self) {
        let _ = self.sck.set_low();
        let _ = self.sck.set_high();
        self.delay.delay_us(POWER_DOWN_US);
    }

    /// Wake the HX711 from power-down. The next conversion needs the internal
    /// filter to settle, so the first `read` afterwards should be discarded.
    #[allow(dead_code)]
    pub fn power_up(&mut self) {
        let _ = self.sck.set_low();
    }

    /// True when a fresh conversion result is latched and ready to be clocked
    /// out (`DT` is pulled low by the HX711 when data is ready).
    fn is_ready(&mut self) -> bool {
        self.dt.is_low().unwrap_or(false)
    }

    /// Asynchronously wait until a conversion result is available, or until
    /// `timeout` elapses.
    ///
    /// Polls `DT` roughly at the HX711's 10 SPS output rate, yielding to the
    /// executor between checks so nothing else is blocked while we wait.
    /// Returns `true` once data is ready, or `false` on timeout — which is what
    /// a disconnected sensor looks like (with `DT` pulled up it never goes low).
    pub async fn wait_ready(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !self.is_ready() {
            if Instant::now() >= deadline {
                return false;
            }
            Timer::after(Duration::from_millis(5)).await;
        }
        true
    }

    /// Read one raw 24-bit sample, sign-extended to `i32`.
    ///
    /// Waits (async) for the device to become ready, then performs the blocking
    /// 24 + N pulse read cycle. Returns the value (range −2^23..2^23), or `None`
    /// if the device did not become ready within `timeout`.
    pub async fn read(&mut self, timeout: Duration) -> Option<i32> {
        if self.wait_ready(timeout).await {
            Some(self.read_raw())
        } else {
            None
        }
    }

    /// The timing-critical portion: clock out 24 data bits (MSB first) plus the
    /// 1–3 gain-select pulses. Kept blocking and interrupt-free-ish to respect
    /// the 60 µs max-high-time constraint.
    fn read_raw(&mut self) -> i32 {
        let mut value: u32 = 0;

        // 24 data pulses, most-significant bit first. The bit is sampled while
        // the clock is *high*, which is what the datasheet's timing diagram
        // shows and what the fake-pin tests assert.
        for _ in 0..24 {
            let _ = self.sck.set_high();
            self.delay.delay_us(1);
            value = (value << 1) | (self.dt.is_high().unwrap_or(false) as u32);
            let _ = self.sck.set_low();
            self.delay.delay_us(1);
        }

        // 1–3 extra pulses set the gain/channel for the next conversion.
        for _ in 0..(self.gain as u8) {
            let _ = self.sck.set_high();
            self.delay.delay_us(1);
            let _ = self.sck.set_low();
            self.delay.delay_us(1);
        }

        sign_extend_24(value)
    }
}

/// Sign-extend a 24-bit two's-complement value (held in the low 24 bits of a
/// `u32`) into a full `i32`.
const fn sign_extend_24(raw: u32) -> i32 {
    if raw & 0x0080_0000 != 0 {
        (raw | 0xFF00_0000) as i32
    } else {
        raw as i32
    }
}

// The crate is `no_std`/`no_main` and links against `esp-hal`, so the default
// `cargo test` harness (which needs `std` on the host) cannot run. Encode the
// sign-extension invariants as `const` assertions instead: they are checked at
// compile time on every build, for the real target.
const _: () = {
    assert!(sign_extend_24(0x00_0000) == 0);
    assert!(sign_extend_24(0x00_0001) == 1);
    assert!(sign_extend_24(0x7F_FFFF) == 8_388_607);
    assert!(sign_extend_24(0xFF_FFFF) == -1);
    assert!(sign_extend_24(0x80_0000) == -8_388_608);
};

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;
    use std::rc::Rc;

    /// The state the two fake pins share, so a test can check not just *what*
    /// was read but *when* — sampling on the wrong clock phase is the classic
    /// bit-bang bug, and it produces plausible numbers rather than obvious ones.
    #[derive(Default)]
    struct Line {
        /// Rising edges seen on SCK: 24 data pulses plus the gain-select ones.
        pulses: usize,
        /// True while SCK is high.
        clock_high: bool,
        /// Times DT was sampled, and how often that happened with SCK low.
        samples: usize,
        sampled_while_low: usize,
        /// Bits the device presents, most-significant first.
        bits: Vec<bool>,
        /// What DT reads before the first clock pulse: low means "ready".
        resting_high: bool,
    }

    type Shared = Rc<RefCell<Line>>;

    struct FakeSck(Shared);
    struct FakeDt(Shared);
    struct FakeDelay {
        micros: u32,
    }

    #[derive(Debug)]
    struct Never;
    impl embedded_hal::digital::Error for Never {
        fn kind(&self) -> embedded_hal::digital::ErrorKind {
            embedded_hal::digital::ErrorKind::Other
        }
    }
    impl embedded_hal::digital::ErrorType for FakeSck {
        type Error = Never;
    }
    impl embedded_hal::digital::ErrorType for FakeDt {
        type Error = Never;
    }

    impl OutputPin for FakeSck {
        fn set_high(&mut self) -> Result<(), Never> {
            let mut line = self.0.borrow_mut();
            line.pulses += 1;
            line.clock_high = true;
            Ok(())
        }
        fn set_low(&mut self) -> Result<(), Never> {
            self.0.borrow_mut().clock_high = false;
            Ok(())
        }
    }

    impl InputPin for FakeDt {
        fn is_high(&mut self) -> Result<bool, Never> {
            let mut line = self.0.borrow_mut();
            // Before the first pulse the pin is just reporting readiness; the
            // data only starts moving once the clock does.
            if line.pulses == 0 {
                return Ok(line.resting_high);
            }
            if !line.clock_high {
                line.sampled_while_low += 1;
            }
            let index = line.pulses - 1;
            line.samples += 1;
            Ok(line.bits.get(index).copied().unwrap_or(false))
        }
        fn is_low(&mut self) -> Result<bool, Never> {
            Ok(!self.is_high()?)
        }
    }

    impl DelayNs for FakeDelay {
        fn delay_ns(&mut self, ns: u32) {
            self.micros += ns / 1000;
        }
    }

    /// A device holding `raw` in its output register, ready to be clocked out.
    fn device(raw: u32) -> (Hx711<FakeDt, FakeSck, FakeDelay>, Shared) {
        let line = Rc::new(RefCell::new(Line {
            // Most-significant bit first.
            bits: (0..24).map(|i| raw & (1 << (23 - i)) != 0).collect(),
            resting_high: false, // low = a conversion is ready
            ..Line::default()
        }));
        let driver = Hx711::new(
            FakeDt(Rc::clone(&line)),
            FakeSck(Rc::clone(&line)),
            FakeDelay { micros: 0 },
        );
        (driver, line)
    }

    fn read(raw: u32) -> (i32, Line) {
        let (mut driver, line) = device(raw);
        let value = crate::sensors::mock::block_on(driver.read(Duration::from_millis(50)))
            .expect("device was ready");
        // The driver holds the other two handles to the shared line.
        drop(driver);
        let line = Rc::try_unwrap(line).ok().expect("sole owner").into_inner();
        (value, line)
    }

    #[test]
    fn a_reading_is_clocked_out_most_significant_bit_first() {
        // If the bit order were reversed these would come back as entirely
        // different, entirely plausible numbers.
        for raw in [
            0x00_0000, 0x00_0001, 0x12_3456, 0x7F_FFFF, 0x80_0000, 0xFF_FFFF,
        ] {
            let (value, _) = read(raw);
            assert_eq!(value, sign_extend_24(raw), "raw {raw:#08x}");
        }
    }

    #[test]
    fn exactly_twenty_four_bits_are_sampled() {
        let (_, line) = read(0x12_3456);
        assert_eq!(line.samples, 24);
    }

    #[test]
    fn every_bit_is_sampled_while_the_clock_is_high() {
        // The datasheet's timing diagram puts the data valid during the high
        // phase; sampling on the low phase reads the *next* bit on real
        // hardware and is invisible in a logic-free test.
        let (_, line) = read(0xA5_5A5A);
        assert_eq!(line.sampled_while_low, 0);
    }

    #[test]
    fn the_gain_selects_the_number_of_trailing_pulses() {
        // 25 / 26 / 27 total. Getting this wrong silently switches channel or
        // gain on the *next* conversion, so the reading after it is wrong.
        for (gain, expected) in [(Gain::A128, 25), (Gain::B32, 26), (Gain::A64, 27)] {
            let (mut driver, line) = device(0x12_3456);
            driver.set_gain(gain);
            let _ = crate::sensors::mock::block_on(driver.read(Duration::from_millis(50)));
            drop(driver);
            assert_eq!(line.borrow().pulses, expected);
        }
    }

    #[test]
    fn a_disconnected_amplifier_times_out_instead_of_returning_garbage() {
        // DT is pulled up, so a missing HX711 never signals ready. Returning a
        // number here would be worse than returning nothing: the scale would
        // publish noise as a weight.
        let line = Rc::new(RefCell::new(Line {
            resting_high: true,
            ..Line::default()
        }));
        let mut driver = Hx711::new(
            FakeDt(Rc::clone(&line)),
            FakeSck(Rc::clone(&line)),
            FakeDelay { micros: 0 },
        );
        let value = crate::sensors::mock::block_on(driver.read(Duration::from_millis(30)));
        assert_eq!(value, None);
        // Nothing was clocked out, so the device was left alone entirely.
        assert_eq!(line.borrow().pulses, 0);
    }

    #[test]
    fn the_clock_is_left_low_between_conversions() {
        // A clock line parked high for more than 60 µs latches the HX711 into
        // power-down, which is exactly what the timing constraint is about.
        let (_, line) = read(0x12_3456);
        assert!(!line.clock_high);
    }

    #[test]
    fn sign_extension_covers_the_whole_range() {
        assert_eq!(sign_extend_24(0x00_0000), 0);
        assert_eq!(sign_extend_24(0x00_0001), 1);
        assert_eq!(sign_extend_24(0x7F_FFFF), 8_388_607);
        assert_eq!(sign_extend_24(0x80_0000), -8_388_608);
        assert_eq!(sign_extend_24(0xFF_FFFF), -1);
        // The boundary either side of the sign bit, where an off-by-one in the
        // mask would show up as a weight jumping to the far end of the scale.
        assert_eq!(sign_extend_24(0x7F_FFFE), 8_388_606);
        assert_eq!(sign_extend_24(0x80_0001), -8_388_607);
    }

    #[test]
    fn a_load_below_the_tare_point_reads_negative() {
        // The reason the sign extension matters at all: an empty pan drifting
        // below its baseline must read as a small negative number, not as
        // eight million grams.
        let (value, _) = read(0xFF_FFF0);
        assert_eq!(value, -16);
    }
}

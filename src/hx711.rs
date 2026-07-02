//! Non-blocking driver for the HX711 24-bit load-cell amplifier.
//!
//! The HX711 has no data bus: it is read by bit-banging a clock line while
//! sampling a data line. This driver keeps the *waiting* for a conversion
//! fully async (so the Embassy executor stays free to service Wi-Fi, timers,
//! etc.) while performing the actual 24-clock read cycle as a short blocking
//! critical section. The datasheet mandates that a single `PD_SCK` high pulse
//! never exceeds 60 µs — otherwise the chip enters power-down — so the tight
//! read loop must *not* yield to the executor mid-transfer.

use embassy_time::{Duration, Timer};
use esp_hal::{
    delay::Delay,
    gpio::{Input, Output},
};

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
pub struct Hx711<'d> {
    dt: Input<'d>,
    sck: Output<'d>,
    gain: Gain,
    delay: Delay,
}

impl<'d> Hx711<'d> {
    /// Create a new driver. The clock line is driven low to keep the device
    /// out of power-down after construction.
    pub fn new(dt: Input<'d>, mut sck: Output<'d>) -> Self {
        sck.set_low();
        Self {
            dt,
            sck,
            gain: Gain::A128,
            delay: Delay::new(),
        }
    }

    /// Select the channel / gain used for the *next* conversion.
    #[allow(dead_code)]
    pub fn set_gain(&mut self, gain: Gain) {
        self.gain = gain;
    }

    /// True when a fresh conversion result is latched and ready to be clocked
    /// out (`DT` is pulled low by the HX711 when data is ready).
    fn is_ready(&self) -> bool {
        self.dt.is_low()
    }

    /// Asynchronously wait until a conversion result is available.
    ///
    /// Polls `DT` roughly at the HX711's 10 SPS output rate, yielding to the
    /// executor between checks so nothing else is blocked while we wait.
    pub async fn wait_ready(&mut self) {
        while !self.is_ready() {
            Timer::after(Duration::from_millis(10)).await;
        }
    }

    /// Read one raw 24-bit sample, sign-extended to `i32`.
    ///
    /// Waits (async) for the device to become ready, then performs the blocking
    /// 24 + N pulse read cycle. Returns a value in the range −2^23..2^23.
    pub async fn read(&mut self) -> i32 {
        self.wait_ready().await;
        self.read_raw()
    }

    /// The timing-critical portion: clock out 24 data bits (MSB first) plus the
    /// 1–3 gain-select pulses. Kept blocking and interrupt-free-ish to respect
    /// the 60 µs max-high-time constraint.
    fn read_raw(&mut self) -> i32 {
        let mut value: u32 = 0;

        // 24 data pulses, most-significant bit first.
        for _ in 0..24 {
            self.sck.set_high();
            self.delay.delay_micros(1);
            value = (value << 1) | (self.dt.is_high() as u32);
            self.sck.set_low();
            self.delay.delay_micros(1);
        }

        // 1–3 extra pulses set the gain/channel for the next conversion.
        for _ in 0..(self.gain as u8) {
            self.sck.set_high();
            self.delay.delay_micros(1);
            self.sck.set_low();
            self.delay.delay_micros(1);
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

//! Minimal 1-Wire driver for the Dallas/Maxim **DS18B20** temperature probe.
//!
//! Like [`crate::hx711`], this is a hand-rolled bit-bang driver rather than a
//! generic crate: the whole bus is a single open-drain data line, and the
//! protocol is nothing but precisely-timed low pulses. Reusing the repo's own
//! style keeps it `no_std`, dependency-free, and consistent with the load-cell
//! driver.
//!
//! Timing model mirrors the HX711 split:
//!   * the reset pulse and each read/write *time slot* are a few-µs **blocking**
//!     critical section (1-Wire tolerances are tight, sub-15 µs on a read
//!     sample), so they must not yield mid-slot;
//!   * the ~750 ms temperature *conversion* is awaited **async** so the executor
//!     stays free.
//!
//! The data line needs an external ~4.7 kΩ pull-up to 3V3 (the module/probe's
//! third wire); the MCU pin is configured open-drain with the weak internal
//! pull-up enabled as a backup.

use embassy_time::{Duration, Timer};
use esp_hal::{delay::Delay, gpio::OutputOpenDrain};

/// 12-bit (default) conversions take up to 750 ms per the datasheet. Wait a hair
/// longer to be safe before clocking the result out.
const CONVERSION_TIME: Duration = Duration::from_millis(760);

// --- ROM / function commands (only the single-drop subset we use) -----------
/// Address every device on the bus at once — valid because the feeder has a
/// single probe, so no ROM search / addressing is needed.
const CMD_SKIP_ROM: u8 = 0xCC;
/// Start a temperature conversion.
const CMD_CONVERT_T: u8 = 0x44;
/// Read the 9-byte scratchpad (temperature LSB/MSB first, CRC last).
const CMD_READ_SCRATCHPAD: u8 = 0xBE;

/// Driver wrapping the single open-drain 1-Wire data line.
pub struct Ds18b20<'d> {
    io: OutputOpenDrain<'d>,
    delay: Delay,
}

impl<'d> Ds18b20<'d> {
    /// Create a driver over an already-configured open-drain data pin. The pin
    /// is left released (high / bus idle).
    pub fn new(mut io: OutputOpenDrain<'d>) -> Self {
        io.set_high();
        Self {
            io,
            delay: Delay::new(),
        }
    }

    /// Trigger a conversion, wait (async) for it to finish, then read the
    /// temperature back.
    ///
    /// Returns the raw 16-bit two's-complement reading (1/16 °C per LSB), or
    /// `None` if no device answered the reset or the scratchpad CRC was bad —
    /// i.e. a disconnected or mis-wired probe, handled the same graceful way as
    /// a silent HX711.
    pub async fn read(&mut self) -> Option<i16> {
        if !self.reset() {
            return None;
        }
        self.write_byte(CMD_SKIP_ROM);
        self.write_byte(CMD_CONVERT_T);

        Timer::after(CONVERSION_TIME).await;

        if !self.reset() {
            return None;
        }
        self.write_byte(CMD_SKIP_ROM);
        self.write_byte(CMD_READ_SCRATCHPAD);

        let mut scratchpad = [0u8; 9];
        for byte in scratchpad.iter_mut() {
            *byte = self.read_byte();
        }

        // Reject a bad frame (floating bus, noise on a long cable) instead of
        // publishing garbage.
        if crc8(&scratchpad[..8]) != scratchpad[8] {
            return None;
        }

        Some(i16::from_le_bytes([scratchpad[0], scratchpad[1]]))
    }

    /// Reset the bus and sample the presence pulse. `true` if at least one
    /// device pulled the line low in response.
    fn reset(&mut self) -> bool {
        // Master reset pulse.
        self.io.set_low();
        self.delay.delay_micros(480);
        // Release and let devices assert their presence pulse.
        self.io.set_high();
        self.delay.delay_micros(70);
        let present = self.io.is_low();
        // See out the rest of the 480 µs presence-detect window.
        self.delay.delay_micros(410);
        present
    }

    /// Write one bit as a low-going time slot (LSB-first byte order handled by
    /// the caller).
    fn write_bit(&mut self, bit: bool) {
        if bit {
            // "1": short low, then release for the rest of the slot.
            self.io.set_low();
            self.delay.delay_micros(6);
            self.io.set_high();
            self.delay.delay_micros(64);
        } else {
            // "0": hold low for essentially the whole slot.
            self.io.set_low();
            self.delay.delay_micros(60);
            self.io.set_high();
            self.delay.delay_micros(10);
        }
    }

    /// Read one bit: pull low briefly to open the slot, release, then sample
    /// while the device is still driving.
    fn read_bit(&mut self) -> bool {
        self.io.set_low();
        self.delay.delay_micros(6);
        self.io.set_high();
        self.delay.delay_micros(9);
        let bit = self.io.is_high();
        self.delay.delay_micros(55);
        bit
    }

    /// Write a byte, least-significant bit first (1-Wire convention).
    fn write_byte(&mut self, byte: u8) {
        for i in 0..8 {
            self.write_bit((byte >> i) & 1 != 0);
        }
    }

    /// Read a byte, least-significant bit first.
    fn read_byte(&mut self) -> u8 {
        let mut byte = 0u8;
        for i in 0..8 {
            if self.read_bit() {
                byte |= 1 << i;
            }
        }
        byte
    }
}

/// Dallas/Maxim 1-Wire CRC-8 (polynomial x^8 + x^5 + x^4 + 1, reflected `0x8C`).
const fn crc8(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    let mut i = 0;
    while i < data.len() {
        let mut byte = data[i];
        let mut bit = 0;
        while bit < 8 {
            let mix = (crc ^ byte) & 1;
            crc >>= 1;
            if mix != 0 {
                crc ^= 0x8C;
            }
            byte >>= 1;
            bit += 1;
        }
        i += 1;
    }
    crc
}

/// Format a raw DS18B20 reading (1/16 °C per LSB) as a fixed-point decimal-°C
/// string with one fractional digit, e.g. `-3.5`. Kept float-free to match the
/// HX711 formatting path.
pub fn write_temp_c(buf: &mut heapless::String<16>, raw: i16) {
    use core::fmt::Write;
    let tenths = temp_tenths(raw);
    if tenths < 0 {
        let _ = buf.push('-');
    }
    let mag = tenths.unsigned_abs();
    // Infallible for a 16-byte buffer ("-123.4" is the worst case).
    let _ = write!(buf, "{}.{}", mag / 10, mag % 10);
}

/// Convert a raw DS18B20 reading (1/16 °C per LSB) to tenths of a degree,
/// rounded to nearest (with a half-LSB bias away from zero).
const fn temp_tenths(raw: i16) -> i32 {
    let scaled = raw as i32 * 10;
    if scaled >= 0 {
        (scaled + 8) / 16
    } else {
        (scaled - 8) / 16
    }
}

// `no_std`/`no_main` means the host `cargo test` harness can't link, so pin the
// CRC and formatting invariants as compile-time `const` assertions instead —
// checked on every build for the real target (same tactic as `hx711`).
const _: () = {
    // CRC-8 over a real +25.0625 °C scratchpad (temperature word 0x0191, config
    // and reserved bytes at power-on defaults); byte 8 of such a frame is 0x70.
    assert!(crc8(&[0x91, 0x01, 0x4B, 0x46, 0x7F, 0xFF, 0x0C, 0x10]) == 0x70);
    assert!(crc8(&[]) == 0);
};

const _: () = {
    // +25.0625 °C -> 25.1; DS18B20 power-on default reads +85 °C exactly.
    assert!(temp_tenths(0x0191) == 251);
    assert!(temp_tenths(0x0550) == 850);
    // -0.5 °C and -25.0625 °C keep their sign through the rounding.
    assert!(temp_tenths(-8) == -5);
    assert!(temp_tenths(0) == 0);
};

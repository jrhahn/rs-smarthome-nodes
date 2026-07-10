//! Tiny persistent state kept in RTC fast RAM.
//!
//! The firmware polls the scale by cold-booting out of deep sleep on a short
//! interval, so plain statics (in regular RAM) are wiped on every wake. The
//! tare baseline and the presence edge must instead survive across deep sleep,
//! which is exactly what `#[ram(rtc_fast, persistent)]` gives us: the memory is
//! zeroed once on the initial power-on and then left untouched across
//! deep-sleep wake-ups, resets, etc.
//!
//! Because the region is zero on first boot, `FLAGS == 0` naturally means
//! "not yet initialised", so no separate magic value is needed.

use esp_hal::macros::ram;

/// Last known empty-house reading, in raw HX711 ticks.
#[ram(rtc_fast, persistent)]
static mut BASELINE: i32 = 0;

/// Packed status bits; see `FLAG_*`.
#[ram(rtc_fast, persistent)]
static mut FLAGS: u32 = 0;

/// Set once the baseline has been tared at least once.
const FLAG_INIT: u32 = 1 << 0;
/// Set while weight is above the presence threshold (edge detection).
const FLAG_PRESENT: u32 = 1 << 1;

fn flags() -> u32 {
    // Single-word reads/writes of a `Persistable` primitive; no other execution
    // context touches these, so a raw read/write is sufficient.
    unsafe { core::ptr::addr_of!(FLAGS).read() }
}

fn set_flags(value: u32) {
    unsafe { core::ptr::addr_of_mut!(FLAGS).write(value) }
}

fn set_flag(bit: u32, on: bool) {
    let updated = if on { flags() | bit } else { flags() & !bit };
    set_flags(updated);
}

/// The persisted empty-house baseline.
pub fn baseline() -> i32 {
    unsafe { core::ptr::addr_of!(BASELINE).read() }
}

/// Replace the persisted baseline.
pub fn set_baseline(value: i32) {
    unsafe { core::ptr::addr_of_mut!(BASELINE).write(value) }
}

/// Whether a baseline has ever been established (false only on the first boot).
pub fn is_initialised() -> bool {
    flags() & FLAG_INIT != 0
}

/// Mark the baseline as established.
pub fn mark_initialised() {
    set_flag(FLAG_INIT, true);
}

/// Whether the previous cycle saw weight on the scale.
pub fn bird_present() -> bool {
    flags() & FLAG_PRESENT != 0
}

/// Record whether weight is currently on the scale.
pub fn set_bird_present(present: bool) {
    set_flag(FLAG_PRESENT, present);
}

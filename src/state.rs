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

/// Empty-house idle wake-ups accumulated since the last publish. Drives the
/// periodic heartbeat: once it reaches `Config::heartbeat_wakes()` the firmware
/// publishes temperature + weight even without a visitor, then resets it. Any
/// real publish (bird arrived / left) also resets it, so the heartbeat clock
/// restarts from the last time Home Assistant already got a fresh reading.
#[ram(rtc_fast, persistent)]
static mut IDLE_WAKES: u32 = 0;

/// Set once the baseline has been tared at least once.
const FLAG_INIT: u32 = 1 << 0;
/// Set while weight is above the presence threshold (edge detection).
const FLAG_PRESENT: u32 = 1 << 1;
/// Set once the Home Assistant discovery configs have been published in this
/// power cycle. They are retained on the broker, so re-sending them on every
/// deep-sleep wake would just spend battery on airtime.
const FLAG_DISCOVERY: u32 = 1 << 2;
/// Set at the end of the first boot of a power cycle. RTC RAM is wiped by a
/// cold power-on but survives deep sleep, so an unset flag means "the board was
/// just plugged in", which is the moment someone might be waiting at the serial
/// console.
///
/// It survives a **reflash** too — verified on hardware 2026-08-19, an
/// `espflash flash` plus the reset it performs left these flags standing. Only
/// removing power clears them. So a board that has already booted once will not
/// re-open the console window just because you flashed it.
const FLAG_BOOTED: u32 = 1 << 3;

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

/// Whether Home Assistant discovery has already been published since the last
/// cold power-on. A fresh power-up (battery swap, unplugging the USB cable)
/// clears RTC RAM, so the configs are re-announced exactly when the broker might
/// have lost them.
///
/// Note that **reflashing does not clear this** (see [`FLAG_BOOTED`]): to force
/// a re-announce you have to pull power, not just flash and reset.
pub fn discovery_published() -> bool {
    flags() & FLAG_DISCOVERY != 0
}

/// Record that the discovery configs went out.
pub fn mark_discovery_published() {
    set_flag(FLAG_DISCOVERY, true);
}

/// Forget that discovery was published, so the next connect re-announces it.
/// Needed when a value baked into the discovery payload changes.
pub fn clear_discovery_published() {
    set_flag(FLAG_DISCOVERY, false);
}

/// Idle wake-ups accumulated since the last publish.
pub fn idle_wakes() -> u32 {
    unsafe { core::ptr::addr_of!(IDLE_WAKES).read() }
}

/// Overwrite the idle wake-up counter (e.g. bump on an empty poll, or reset to
/// zero right after a publish).
pub fn set_idle_wakes(value: u32) {
    unsafe { core::ptr::addr_of_mut!(IDLE_WAKES).write(value) }
}

/// Is this the first boot since the board was **powered up** — power actually
/// removed and reapplied — rather than a wake from deep sleep? A reflash or a
/// reset does not count; see [`FLAG_BOOTED`].
pub fn is_cold_boot() -> bool {
    flags() & FLAG_BOOTED == 0
}

/// Record that this power cycle has booted once; every later wake sees it.
pub fn mark_booted() {
    set_flag(FLAG_BOOTED, true);
}

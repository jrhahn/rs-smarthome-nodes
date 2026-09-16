//! Tiny persistent state kept in RTC fast RAM.
//!
//! The firmware polls the scale by cold-booting out of deep sleep on a short
//! interval, so plain statics (in regular RAM) are wiped on every wake. The
//! tare baseline and the presence edge must instead survive across deep sleep,
//! which is exactly what `#[ram(rtc_fast, persistent)]` gives us: the startup
//! code leaves the region alone instead of zeroing it.
//!
//! **Nothing here may depend on the region being cleared when power is
//! removed.** Discovery used to: it was gated on a "have I announced yet" bit,
//! on the assumption that a cold power-on zeroes RTC RAM. On 2026-09-09 a
//! `wohnzimmer` node had its USB unplugged for several seconds and came back
//! with the bit still set, so its four SDS011 entities stayed unannounced
//! through a reflash, a reset *and* that power cycle — the readings were on the
//! broker the whole time with nothing in Home Assistant to receive them. The
//! board's rails evidently decay slower than the RTC domain forgets.
//!
//! So `FLAGS == 0` is a hint, not a guarantee. See [`discovery_tag`] for the
//! shape that stays correct when the hint is wrong.

use esp_hal::macros::ram;

use crate::sensors::scale;

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

/// Consecutive rounds a load has been on the scale, for the stuck-load bound
/// in [`crate::presence::STUCK_AFTER_SECS`]. Reset by an arrival or a
/// departure, so it only ever counts one uninterrupted stretch.
#[ram(rtc_fast, persistent)]
static mut PRESENT_ROUNDS: u32 = 0;

/// Digest of the discovery messages last successfully announced from this
/// board; see [`crate::discovery::announcement_tag`]. Zero means "nothing".
#[ram(rtc_fast, persistent)]
static mut DISCOVERY_TAG: u32 = 0;

/// Visits counted since the board last lost power entirely.
///
/// Counted at the *arrival*, not at the publish, which is the whole point of
/// keeping it here rather than deriving it in Home Assistant: state topics go
/// out at QoS0 without retain, and `presence_publish_allowed` deliberately
/// drops arrivals that fall inside the 60 s rate limit. Both would undercount,
/// silently, and a bird that comes back twice in half a minute is two visits.
///
/// RTC RAM rather than the flash blob on purpose. A visit is a frequent event —
/// 75 of them on 2026-09-11 — and a flash write means erasing a sector, so
/// counting in flash would spend the sector's endurance on nothing. Losing the
/// total when the cell is swapped is the accepted cost: the entity is
/// `total_increasing`, and Home Assistant treats a drop to zero as a counter
/// reset rather than as negative consumption, so the long-term history it has
/// already recorded survives the board forgetting.
#[ram(rtc_fast, persistent)]
static mut VISIT_COUNT: u32 = 0;

/// Companion to [`VISIT_COUNT`], holding it XORed with
/// [`scale::VISITS_MAGIC`]. The pair is what makes the counter survivable: a
/// word that has just been added to the firmware comes up holding leftover
/// memory, and a reflash is not a cold boot, so there is no moment at which
/// zeroing it would have been reliable. See that constant for the full story.
#[ram(rtc_fast, persistent)]
static mut VISIT_CHECK: u32 = 0;

/// Set once the baseline has been tared at least once.
const FLAG_INIT: u32 = 1 << 0;
/// Set while weight is above the presence threshold (edge detection).
const FLAG_PRESENT: u32 = 1 << 1;
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

/// The digest of the discovery messages last announced from this board, or 0.
///
/// Compared against [`crate::discovery::announcement_tag`] rather than being a
/// boolean, because *any* difference in what Home Assistant should know has to
/// re-announce: a sensor switched on in [`crate::node`] and reflashed, a
/// changed `expire_after`, or RTC RAM that survived a power cycle holding a
/// value from before all of that.
///
/// The boolean it replaces could only ever answer "done", which is precisely
/// the answer that cannot be checked — and it was wrong for six days on the
/// living-room node (see the module note). A digest is checkable: if it does
/// not match what this image would send, the announce happens again, and a
/// stale or garbage word fails to match all by itself.
pub fn discovery_tag() -> u32 {
    unsafe { core::ptr::addr_of!(DISCOVERY_TAG).read() }
}

/// Record the digest of the messages that just went out.
pub fn set_discovery_tag(tag: u32) {
    unsafe { core::ptr::addr_of_mut!(DISCOVERY_TAG).write(tag) }
}

/// How many consecutive rounds a load has been on the scale.
pub fn present_rounds() -> u32 {
    unsafe { core::ptr::addr_of!(PRESENT_ROUNDS).read() }
}

/// Replace the consecutive-load counter.
pub fn set_present_rounds(value: u32) {
    unsafe { core::ptr::addr_of_mut!(PRESENT_ROUNDS).write(value) }
}

/// Visits counted since the last full power loss, or zero if the stored pair
/// does not agree -- which is what leftover RTC memory looks like.
pub fn visit_count() -> u32 {
    let value = unsafe { core::ptr::addr_of!(VISIT_COUNT).read() };
    let check = unsafe { core::ptr::addr_of!(VISIT_CHECK).read() };
    scale::visits_from_pair(value, check)
}

/// Replace the visit counter, writing both halves of the pair so the next read
/// trusts it. Every caller that wants to *record* a visit wants [`count_visit`].
pub fn set_visit_count(value: u32) {
    unsafe {
        core::ptr::addr_of_mut!(VISIT_COUNT).write(value);
        core::ptr::addr_of_mut!(VISIT_CHECK).write(scale::visits_check(value));
    }
}

/// Record one arrival. Saturating, so a board left running for years reports a
/// stuck maximum rather than wrapping to zero and looking like a fresh install.
pub fn count_visit() {
    set_visit_count(visit_count().saturating_add(1));
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

// --- Reset diagnostics -------------------------------------------------------

/// Reset cause of the most recent boot that was **not** an ordinary deep-sleep
/// wake, as the raw `SocResetReason` discriminant. Zero until one happens.
///
/// Latched rather than read live because the two do not line up in time: a
/// battery node wakes some hundreds of times between publishes, so the boot
/// that carries the interesting cause is almost never the boot that gets to
/// talk to the broker. Holding it here means the next publish carries it.
///
/// RTC RAM is exactly the right store for this. A watchdog reset, a brownout
/// and a software reset all leave it intact, so the evidence survives the event
/// it describes; only removing power clears it, and that is a power-on reset —
/// which this then records as itself.
#[ram(rtc_fast, persistent)]
static mut RESET_REASON: u32 = 0;

/// How many such resets since power was last removed. One alone is ambiguous;
/// a count climbing over days is a node rebooting in a loop nobody has noticed,
/// which is the failure this whole pair exists to make visible.
#[ram(rtc_fast, persistent)]
static mut RESET_COUNT: u32 = 0;

/// Deep-sleep wake (`SocResetReason::CoreDeepSleep`). The expected cause on
/// this node and the one worth filtering out; everything else is a report.
pub const RESET_DEEP_SLEEP: u32 = 0x05;

/// Record this boot's reset cause. Call once, early. A deep-sleep wake is the
/// steady state and is ignored, so what stays latched is the last thing that
/// was not routine.
pub fn note_reset(code: u32) {
    if code == RESET_DEEP_SLEEP {
        return;
    }
    unsafe {
        core::ptr::addr_of_mut!(RESET_REASON).write(code);
        let n = core::ptr::addr_of!(RESET_COUNT).read();
        core::ptr::addr_of_mut!(RESET_COUNT).write(n.saturating_add(1));
    }
}

/// The latched cause, or zero if nothing but deep sleep has happened.
pub fn last_reset() -> u32 {
    unsafe { core::ptr::addr_of!(RESET_REASON).read() }
}

/// How many non-routine resets since power-on.
pub fn reset_count() -> u32 {
    unsafe { core::ptr::addr_of!(RESET_COUNT).read() }
}

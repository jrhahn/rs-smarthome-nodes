//! Why the node last restarted, published so the archive can answer it.
//!
//! The outdoor node has twice gone silent mid-cadence with a healthy cell and
//! come back only after power was removed entirely — see
//! [`docs/commissioning.md`]. Nothing in the telemetry could distinguish the
//! candidates, because the evidence destroys itself: the reset-cause register
//! describes *this* boot, so by the time anyone has pulled the cable to revive
//! the node, it reports the cable.
//!
//! Publishing it closes that. The three cases separate cleanly in the archive:
//!
//! * a **watchdog** cause appearing regularly means the node hangs often and
//!   usually recovers on its own, and the silences are the times it did not;
//! * a **brownout** cause means the supply collapses under radio load, which
//!   for a cell reading 3.7 V would mean an aged cell rather than a flat one;
//! * **nothing at all** across a silence means a true hang, with the watchdog
//!   not firing either — the case that needs a power cycle.
//!
//! That third case had no explanation until 2026-09-29, and the explanation was
//! that neither half of it could have gone otherwise: **no watchdog was armed**
//! — `esp_hal::Config::default()` disables all four and `main` only set the
//! clock — and `esp-backtrace`'s panic and exception handlers both end in
//! `halt()`, which is `loop { continue; }`. So any panic parked the board for
//! ever, and nothing on the chip could end it. Both are fixed: see
//! [`PANIC`] and `WATCHDOG_SECS` in `main`. A silence with nothing reported
//! afterwards now means something new, because a panic reports [`PANIC`] and a
//! hang reports `0x07`.
//!
//! Reported as the raw `SocResetReason` discriminant rather than a name: the
//! MQTT archiver stores values as doubles, so a string would not survive the
//! trip. The codes are the hardware's own, listed below.

use core::fmt::Write as _;

use heapless::String;

use crate::sensors::EntityDescriptor;

/// Home Assistant discovery metadata. Neither entity has a unit or a device
/// class — like the visit count, they are bare numbers, and Home Assistant
/// validates `dev_cla` against its own list, so an absent key is how to say
/// "none" without the entity being rejected.
///
/// `reset_count` is `total_increasing` rather than `measurement`: it only ever
/// climbs, and saying so lets Home Assistant draw it as the step function it is
/// and survive the reset to zero that a power cycle brings.
pub const DESCRIPTORS: &[EntityDescriptor] = &[
    EntityDescriptor {
        key: "reset_reason",
        name: "Reset-Grund",
        unit: "",
        device_class: "",
        state_class: "measurement",
    },
    EntityDescriptor {
        key: "reset_count",
        name: "Unerwartete Resets",
        unit: "",
        device_class: "",
        state_class: "total_increasing",
    },
    EntityDescriptor {
        key: "boot_count",
        name: "Kaltstarts",
        unit: "",
        device_class: "",
        state_class: "total_increasing",
    },
];

/// Where the boot counter lives: inside the sector [`crate::ota`] already keeps
/// its bookkeeping in, clear of the record at its start.
///
/// Flash rather than RTC RAM, and that is the entire point of this number. The
/// counter beside it, `reset_count`, lives in RTC RAM, and RTC RAM does not
/// survive a power cut long enough to matter -- so [`latch`] takes its
/// "not ours" branch and the count starts again at 1 with the reason set to
/// `POWER_ON`. Which is *exactly* the state a node that has simply been running
/// quietly reports. The two cases that matter most cannot be told apart:
///
/// | what happened | reset_reason | reset_count |
/// | --- | --- | --- |
/// | nothing; it has been up for days | 1 | 1 |
/// | the supply went away for an hour | 1 | 1 |
///
/// That cost an evening on `bad` in September 2026 -- four outages in two
/// weeks, and no way to say from the archive whether the board had lost power
/// or had stopped waking up. A number that survives power answers it by
/// existing: if it has climbed, the board restarted; if it has not, the board
/// never did, and whatever went wrong happened with the power still on.
pub const BOOT_LOG_OFFSET: u32 = crate::ota::STATE_OFFSET + 0x100;

/// `"BOOT"` little-endian.
const BOOT_MAGIC: u32 = 0x544F_4F42;
const BOOT_VERSION: u8 = 1;
/// magic(4) + version(1) + pad(3) + count(4) + reason(4) + crc(4).
pub const BOOT_LOG_LEN: usize = 20;

/// Stop writing past this, and keep reporting it.
///
/// Every hard boot is a flash write, and the failure this number exists to
/// describe -- a board restarting over and over -- is also the one that would
/// wear the sector out. Ten thousand is far beyond any history worth reading
/// (`bad` managed four in a fortnight) and far below what a sector will take,
/// so the counter stops being written long before it stops being writable. The
/// story is already told by then.
pub const MAX_RECORDED_BOOTS: u32 = 10_000;

// Same sector as the OTA bookkeeping, and it has to stay clear of the record at
// its start: overlapping the two would trade one diagnostic for a node that
// cannot roll back. Checked at build time rather than in a test, the way
// `ota.rs` checks its own layout -- a wrong offset here is not something to
// learn from a red test on a laptop.
const _: () = {
    assert!(BOOT_LOG_OFFSET > crate::ota::STATE_OFFSET + 64);
    assert!(BOOT_LOG_OFFSET + BOOT_LOG_LEN as u32 <= crate::ota::STATE_OFFSET + crate::ota::SECTOR);
};

/// The codes worth recognising when reading the archive back. The full list is
/// `esp_hal`'s `SocResetReason`; these are the ones this node can plausibly
/// produce, and the reason the number is worth publishing at all.
///
/// | code | meaning |
/// | --- | --- |
/// | 0x01 | power on — the cell was disconnected, or the protection board cut |
/// | 0x03 | software reset of the digital core |
/// | 0x05 | deep-sleep wake — routine, never latched |
/// | 0x07 | main watchdog 0 — the app hung and was rebooted |
/// | 0x09 | RTC watchdog |
/// | 0x0F | **brownout** — the supply collapsed |
/// | 0x10 | RTC watchdog, core and RTC |
/// | 0x12 | super watchdog |
/// | 0x15 | USB UART reset — a host attached, e.g. `espflash` |
/// | 0x16 | USB JTAG reset |
/// | 0x100 | **panic** — not a hardware code; see [`PANIC`] |
pub const POWER_ON: u32 = 0x01;

/// Not a hardware code: this firmware restarting itself because it panicked.
///
/// Above every `SocResetReason` discriminant on purpose, so it cannot collide
/// with one the hardware might start reporting. It exists because the reset
/// itself is a software reset (`0x03`) and so is the one an over-the-air update
/// performs -- and "the image is bad" and "the image is new" are not things to
/// read from the same number.
pub const PANIC: u32 = 0x100;

/// Deep-sleep wake (`SocResetReason::CoreDeepSleep`). The expected cause on a
/// battery node, hundreds of times between publishes, and the one worth
/// filtering out; everything else is a report.
pub const DEEP_SLEEP: u32 = 0x05;

/// Marks the counter's RTC-RAM words as ours. `"REST"` in ASCII.
///
/// Without it the counter is nonsense, and this module shipped that way for one
/// evening: the first publish after flashing carried
/// `reset_count 3319124735`. RTC fast RAM is **not** zeroed when power is
/// removed — `state.rs` opens with that warning and the story behind it — so a
/// fresh region holds whatever was there before, and incrementing it produces a
/// large believable-looking number rather than an obvious error.
///
/// Guarding with a tag is the same shape `discovery_tag` uses: a value that
/// stays correct when the region is garbage, instead of a flag that assumes it
/// is not.
pub const EPOCH_TAG: u32 = 0x5245_5354;

/// Above this, a stored count is garbage rather than history.
///
/// The tag alone turned out not to be enough. After the first attempt shipped,
/// the node published `reset_count 3319124736` — the previous nonsense plus
/// one, which means `latch` took the *increment* branch and therefore read the
/// tag as already matching, on the first boot of firmware that had only just
/// introduced the constant. That is not explained. What is certain is that a
/// count in the billions is not a history, whatever the tag says.
///
/// A node resetting every five seconds for a year reaches about 6.3 million, so
/// anything past a million is already beyond a story anyone would tell — and
/// reaching it legitimately would itself be the finding. Bounding is crude next
/// to a tag, but it cannot be defeated by one word comparing equal by accident,
/// and the failure it prevents is a dashboard reading nobody can interpret.
pub const MAX_PLAUSIBLE_COUNT: u32 = 1_000_000;

/// What one boot does to the persistent state: the reset diagnostics, and the
/// count of consecutive refused joins.
///
/// Both live in the same RTC-RAM region and both are only meaningful if that
/// region is ours, so one function decides, and `state.rs` stores what it
/// returns. Pure so it can be tested on the host — `state.rs` needs the HAL for
/// the memory, the decision does not.
///
/// `refusals` is kept across deep sleep but cleared by a **power-on**, which is
/// what `wifi.rs` means by "for the rest of the run". Holding it only in a task
/// local, as this did until 2026-09-17, meant a battery node reset it every
/// couple of seconds and `FALLBACK_AFTER` could never be reached — the
/// protection against a mistyped passphrase was inert on the one node that
/// cannot be reflashed casually. A watchdog reset or a brownout deliberately
/// does *not* forgive: those are the node failing, not someone power-cycling it
/// to ask for another try.
pub const fn latch(
    tag: u32,
    count: u32,
    latched: u32,
    refusals: u32,
    code: u32,
) -> (u32, u32, u32, u32) {
    if tag != EPOCH_TAG || count > MAX_PLAUSIBLE_COUNT {
        // Not ours. Whatever the words held is not a history, and this boot is
        // the first one that can be counted.
        return match code {
            DEEP_SLEEP => (EPOCH_TAG, 0, 0, 0),
            _ => (EPOCH_TAG, 1, code, 0),
        };
    }
    let kept = match code {
        POWER_ON => 0,
        _ => refusals,
    };
    match code {
        DEEP_SLEEP => (tag, count, latched, kept),
        _ => (tag, count.saturating_add(1), code, kept),
    }
}

/// What this boot does to the flash counter: `None` when nothing should be
/// written, `Some((count, reason))` when it should.
///
/// Pure, for the same reason [`latch`] is: the decision is testable on the host
/// and the flash access is not. A deep-sleep wake writes nothing -- it is the
/// steady state and happens every two minutes, which no sector should be asked
/// to absorb.
pub const fn record_boot(stored: Option<(u32, u32)>, code: u32) -> Option<(u32, u32)> {
    if code == DEEP_SLEEP {
        return None;
    }
    match stored {
        // A blank or unreadable record is not a history. This boot is the first
        // one that can be counted, which is the same rule `latch` uses.
        None => Some((1, code)),
        Some((count, _)) if count >= MAX_RECORDED_BOOTS => None,
        Some((count, _)) => Some((count.saturating_add(1), code)),
    }
}

/// Encode the counter for flash.
pub fn encode_boot_log(count: u32, reason: u32) -> [u8; BOOT_LOG_LEN] {
    let mut b = [0u8; BOOT_LOG_LEN];
    b[0..4].copy_from_slice(&BOOT_MAGIC.to_le_bytes());
    b[4] = BOOT_VERSION;
    b[8..12].copy_from_slice(&count.to_le_bytes());
    b[12..16].copy_from_slice(&reason.to_le_bytes());
    let crc = crate::config::crc32(&b[0..16]);
    b[16..20].copy_from_slice(&crc.to_le_bytes());
    b
}

/// Decode it. `None` for a blank sector, an erased one, a stale version or a
/// bad CRC -- all of which read as "no history", never as a wrong number.
pub fn decode_boot_log(b: &[u8; BOOT_LOG_LEN]) -> Option<(u32, u32)> {
    if u32::from_le_bytes([b[0], b[1], b[2], b[3]]) != BOOT_MAGIC || b[4] != BOOT_VERSION {
        return None;
    }
    let crc = u32::from_le_bytes([b[16], b[17], b[18], b[19]]);
    if crc != crate::config::crc32(&b[0..16]) {
        return None;
    }
    let count = u32::from_le_bytes([b[8], b[9], b[10], b[11]]);
    if count == 0 || count > MAX_RECORDED_BOOTS {
        return None;
    }
    Some((count, u32::from_le_bytes([b[12], b[13], b[14], b[15]])))
}

/// The counter as flash holds it, or `None` if the sector never held one.
#[cfg(feature = "hal")]
pub fn load_boot_log() -> Option<(u32, u32)> {
    use embedded_storage::ReadStorage as _;
    let mut b = [0u8; BOOT_LOG_LEN];
    esp_storage::FlashStorage::new()
        .read(BOOT_LOG_OFFSET, &mut b)
        .ok()?;
    decode_boot_log(&b)
}

/// Count this boot, if it is one worth counting. Call once, early, beside
/// [`crate::state::note_reset`].
///
/// Deliberately best-effort: a node that cannot write this sector should still
/// boot and publish. The number is a diagnostic, and a diagnostic that can stop
/// a node from running is worse than no diagnostic.
#[cfg(feature = "hal")]
pub fn note_boot(code: u32) {
    use embedded_storage::Storage as _;
    let Some((count, reason)) = record_boot(load_boot_log(), code) else {
        return;
    };
    if esp_storage::FlashStorage::new()
        .write(BOOT_LOG_OFFSET, &encode_boot_log(count, reason))
        .is_err()
    {
        log::warn!("could not record this boot; the counter will lag");
    }
}

/// This boot's reset cause, as the raw discriminant. `None` when the hardware
/// reports something `esp_hal` does not recognise, which is reported as zero
/// rather than guessed at.
#[cfg(feature = "hal")]
pub fn code() -> u32 {
    esp_hal::reset::reset_reason().map_or(0, |reason| reason as u32)
}

/// Format a code for publishing: a bare integer, since it is an enumeration
/// rather than a measurement.
pub fn write_code(out: &mut String<16>, code: u32) {
    let _ = write!(out, "{code}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_implausible_count_is_discarded_even_when_the_tag_matches() {
        // The case the tag alone did not catch: the node published
        // `reset_count 3319124736`, i.e. garbage that had been incremented, so
        // the tag had compared equal on a boot that could not have written it.
        let (tag, count, latched, _) = latch(EPOCH_TAG, 3_319_124_735, 21, 9, 0x07);
        assert_eq!((tag, count, latched), (EPOCH_TAG, 1, 0x07));
    }

    #[test]
    fn a_plausible_count_still_climbs() {
        let (_, count, _, _) = latch(EPOCH_TAG, MAX_PLAUSIBLE_COUNT - 1, 0, 0, 0x07);
        assert_eq!(
            count, MAX_PLAUSIBLE_COUNT,
            "the bound must not clamp real history"
        );
    }

    #[test]
    fn an_untagged_region_is_treated_as_empty_rather_than_counted_on() {
        // The bug this guards: RTC RAM is not zeroed on power-up, so the count
        // started from garbage and the first publish read 3319124735.
        let garbage = 0xC5D3_1FFF;
        let (tag, count, latched, _) = latch(garbage, garbage, garbage, garbage, 0x0F);
        assert_eq!(tag, EPOCH_TAG);
        assert_eq!(count, 1, "the first countable boot, not garbage + 1");
        assert_eq!(latched, 0x0F);
    }

    #[test]
    fn an_untagged_region_woken_from_sleep_starts_at_zero() {
        let (tag, count, latched, refusals) = latch(0xDEAD_BEEF, 7, 9, 5, DEEP_SLEEP);
        assert_eq!((tag, count, latched, refusals), (EPOCH_TAG, 0, 0, 0));
    }

    #[test]
    fn deep_sleep_wakes_are_not_counted() {
        let before = (EPOCH_TAG, 4, 0x07, 2);
        let after = latch(before.0, before.1, before.2, before.3, DEEP_SLEEP);
        assert_eq!(after, before, "the steady state must not move the counter");
    }

    #[test]
    fn anything_else_counts_and_replaces_the_latched_cause() {
        let (_, count, latched, _) = latch(EPOCH_TAG, 4, 0x07, 0, 0x0F);
        assert_eq!((count, latched), (5, 0x0F));
    }

    #[test]
    fn refused_joins_survive_deep_sleep() {
        // The bug: a battery node cold-boots every few seconds, so a task-local
        // counter reset before FALLBACK_AFTER could ever be reached and the
        // protection against a mistyped passphrase never fired.
        let (_, _, _, refusals) = latch(EPOCH_TAG, 1, 21, 2, DEEP_SLEEP);
        assert_eq!(
            refusals, 2,
            "a wake must not forgive the stored credentials"
        );
    }

    #[test]
    fn a_power_cycle_forgives_them() {
        // wifi.rs's stated intent: the likeliest reason for a run of failures is
        // an access point that was down, so removing power asks for another try.
        let (_, _, _, refusals) = latch(EPOCH_TAG, 1, 21, 3, POWER_ON);
        assert_eq!(refusals, 0);
    }

    #[test]
    fn a_watchdog_reset_does_not_forgive_them() {
        // That is the node failing, not someone asking for another try.
        let (_, _, _, refusals) = latch(EPOCH_TAG, 1, 21, 3, 0x07);
        assert_eq!(refusals, 3);
    }

    #[test]
    fn a_code_is_published_as_a_bare_integer() {
        let mut s = String::<16>::new();
        write_code(&mut s, 0x0F);
        assert_eq!(
            s.as_str(),
            "15",
            "decimal, not hex — the archiver stores doubles"
        );
    }

    #[test]
    fn the_two_entities_carry_no_unit_or_device_class() {
        // Home Assistant validates `dev_cla` against its own list and rejects an
        // empty string outright; the payload builder omits empty members, and
        // these two rely on that.
        for d in DESCRIPTORS {
            assert!(d.unit.is_empty(), "{} should have no unit", d.key);
            assert!(
                d.device_class.is_empty(),
                "{} should have no device class",
                d.key
            );
        }
    }

    #[test]
    fn a_deep_sleep_wake_never_touches_the_flash() {
        // Every two minutes, for years. The sector would not survive being told
        // about it, and there is nothing to tell: a wake is the steady state.
        assert_eq!(record_boot(Some((7, POWER_ON)), DEEP_SLEEP), None);
        assert_eq!(record_boot(None, DEEP_SLEEP), None);
    }

    #[test]
    fn a_blank_sector_starts_the_history_at_one() {
        assert_eq!(record_boot(None, POWER_ON), Some((1, POWER_ON)));
    }

    #[test]
    fn every_other_boot_counts_and_records_why() {
        assert_eq!(record_boot(Some((4, POWER_ON)), 0x07), Some((5, 0x07)));
    }

    #[test]
    fn the_counter_stops_writing_before_it_wears_the_sector_out() {
        // The failure this number describes -- a board restarting over and over
        // -- is the one that would write most, so the write has to stop while
        // the number still reads.
        assert_eq!(
            record_boot(Some((MAX_RECORDED_BOOTS, POWER_ON)), POWER_ON),
            None
        );
        assert_eq!(
            record_boot(Some((MAX_RECORDED_BOOTS - 1, POWER_ON)), POWER_ON),
            Some((MAX_RECORDED_BOOTS, POWER_ON))
        );
    }

    #[test]
    fn the_blob_survives_a_round_trip() {
        let bytes = encode_boot_log(42, 0x0F);
        assert_eq!(decode_boot_log(&bytes), Some((42, 0x0F)));
    }

    #[test]
    fn a_damaged_blob_reads_as_no_history_rather_than_a_wrong_number() {
        let mut bytes = encode_boot_log(42, POWER_ON);
        bytes[9] ^= 0xFF; // flip a byte of the count
        assert_eq!(decode_boot_log(&bytes), None);
        // An erased sector is all ones, a blank one all zeros; neither is ours.
        assert_eq!(decode_boot_log(&[0xFF; BOOT_LOG_LEN]), None);
        assert_eq!(decode_boot_log(&[0x00; BOOT_LOG_LEN]), None);
    }
}

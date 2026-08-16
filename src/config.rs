//! Runtime configuration persisted in flash (NVS sector) and tunable live from
//! Home Assistant over retained MQTT.
//!
//! Historically the tuning knobs (`PRESENCE_THRESHOLD`, poll intervals) were
//! compile-time constants and the gram calibration (`offset` / `scale_factor`)
//! lived in the Home Assistant template. This module moves all of that onto the
//! controller so it can be changed **without reflashing**: HA publishes each
//! value to a retained `birds/scale/config/<key>` topic, the firmware reads them
//! whenever it is already online for a publish, and stores the result here.
//!
//! Persistence is a single fixed-layout blob in one 4 KiB flash sector, guarded
//! by a magic + version + CRC-32 so a blank or corrupt sector falls back to
//! [`Config::DEFAULT`]. RTC RAM ([`crate::state`]) is deliberately *not* used for
//! this: it is wiped on a cold power-on (battery swap), and calibration must
//! survive that.
//!
//! The blob encoding and [`Config::apply`] are pure and always compiled; only
//! the flash access itself sits behind the `hal` feature, so the layout and the
//! parsing can be unit-tested on the host (see the `tests` module below).

use core::time::Duration as CoreDuration;

#[cfg(feature = "hal")]
use embedded_storage::{ReadStorage, Storage};
#[cfg(feature = "hal")]
use esp_storage::FlashStorage;
use heapless::String;

/// Flash byte-offset of the config blob. Matches the `nvs` partition in
/// espflash's default table; we only touch the first sector of it.
const FLASH_OFFSET: u32 = 0x9000;

/// Second sector of the same partition, holding the node-identity override
/// (see [`crate::node`]). Deliberately a *separate* blob: provisioning a board
/// must never be able to disturb the calibration, and the identity is written
/// once in a board's life while the tuning changes all the time.
const NODE_OFFSET: u32 = 0xA000;

/// `"BIRD"` little-endian — marks an initialised blob.
const MAGIC: u32 = 0x4449_5242;
/// Bump when the on-flash layout changes; an old version reverts to defaults.
const VERSION: u8 = 3;
/// Serialised length: magic(4) + version(1) + pad(3) + eight 4-byte fields + crc(4).
const BLOB_LEN: usize = 4 + 4 + 4 * 8 + 4;

/// All runtime-tunable settings. `f32` calibration fields are compared bitwise
/// for change detection, which is exactly what we want (a re-sent identical
/// value is a no-op and skips the flash write).
#[derive(Clone, Copy, PartialEq)]
pub struct Config {
    /// Raw HX711 reading corresponding to 0 g (the tare zero).
    pub offset: i32,
    /// Raw HX711 ticks per gram.
    pub scale_factor: f32,
    /// Weight, in grams, that counts as "a bird landed".
    pub threshold_grams: f32,
    /// Deep-sleep seconds between polls while the house is empty.
    pub idle_secs: u32,
    /// Deep-sleep seconds between publishes while a bird is present.
    pub active_secs: u32,
    /// Last numeric tare token seen, so a replayed one is not mistaken for a
    /// second press. `0` for the discovered button, which has no token (see
    /// [`Config::apply`]).
    pub tare_token: u32,
    /// When `false`, the firmware never deep-sleeps: it stays awake and keeps
    /// Wi-Fi up in a loop. Meant for bench testing on USB where deep sleep just
    /// churns the serial monitor. Only consulted on battery nodes — a mains node
    /// stays awake regardless (see [`crate::node::PowerProfile`]).
    pub deep_sleep: bool,
    /// Seconds between periodic "heartbeat" publishes: even with no visitor, the
    /// firmware brings Wi-Fi up this often and publishes temperature + weight so
    /// Home Assistant keeps a fresh reading. Realised as a whole number of idle
    /// wake-ups (see [`Config::heartbeat_wakes`]).
    pub heartbeat_secs: u32,
}

impl Config {
    /// Factory defaults, used on first boot or a corrupt sector. Chosen to match
    /// the historical compile-time constants, except the threshold which is now
    /// expressed in grams.
    pub const DEFAULT: Config = Config {
        offset: 8_388_608, // mid-scale; overwritten by the first tare / calibration
        scale_factor: 420.0,
        threshold_grams: 10.0,
        idle_secs: 2,
        active_secs: 10,
        tare_token: 0,
        // Battery behaviour by default. A mains node ignores the flag entirely
        // (see `node::PowerProfile`), so this needs no per-node value — which
        // also keeps `DEFAULT` a plain constant now that the node identity is
        // resolved at runtime from flash.
        deep_sleep: true,
        heartbeat_secs: 600, // 10 min
    };

    /// The presence threshold expressed in raw HX711 ticks, i.e. what `main`
    /// compares the load delta against. Clamped to at least 1 tick so a bad
    /// (zero/negative) calibration can never make everything look "present".
    pub fn threshold_ticks(&self) -> i32 {
        let ticks = self.threshold_grams * self.scale_factor;
        if ticks.is_finite() && ticks >= 1.0 {
            ticks as i32
        } else {
            1
        }
    }

    pub fn idle_interval(&self) -> CoreDuration {
        CoreDuration::from_secs(self.idle_secs.max(1) as u64)
    }

    pub fn active_interval(&self) -> CoreDuration {
        CoreDuration::from_secs(self.active_secs.max(1) as u64)
    }

    /// Number of idle wake-ups that make up one heartbeat period: after this
    /// many empty-poll cycles, bring Wi-Fi up and publish temperature + weight
    /// even without a visitor. At least 1, so a misconfigured (tiny) interval
    /// still fires every cycle rather than never.
    pub fn heartbeat_wakes(&self) -> u32 {
        (self.heartbeat_secs / self.idle_secs.max(1)).max(1)
    }

    /// Convert a raw HX711 reading to grams using the stored calibration, and
    /// format it as a fixed-point decimal string with one fractional digit
    /// (float-free formatting, matching the DS18B20 path). A non-positive /
    /// non-finite scale factor falls back to the default so we never divide by
    /// zero or emit `inf`.
    pub fn write_grams(&self, buf: &mut heapless::String<16>, raw: i32) {
        let scale = if self.scale_factor.is_finite() && self.scale_factor > 0.0 {
            self.scale_factor
        } else {
            Config::DEFAULT.scale_factor
        };
        let scaled = (raw - self.offset) as f32 * 10.0 / scale;
        // Round to nearest tenth without needing `f32::round` (libm-free).
        let tenths = if scaled >= 0.0 {
            (scaled + 0.5) as i32
        } else {
            (scaled - 0.5) as i32
        };
        if tenths < 0 {
            let _ = buf.push('-');
        }
        let mag = tenths.unsigned_abs();
        use core::fmt::Write;
        let _ = write!(buf, "{}.{}", mag / 10, mag % 10);
    }

    /// Apply one `key=value` pair received from a `birds/scale/config/<key>`
    /// topic. Returns `true` if it parsed and actually changed a field.
    ///
    /// `tare` is special: it re-zeros the scale by adopting `tare_ref` (the
    /// caller passes the current empty-house baseline) as the gram offset.
    pub fn apply(&mut self, key: &str, value: &str, tare_ref: i32) -> bool {
        let before = *self;
        match key {
            "offset" => {
                if let Ok(v) = value.parse::<i32>() {
                    self.offset = v;
                }
            }
            "scale_factor" => {
                if let Ok(v) = value.parse::<f32>() {
                    if v.is_finite() && v > 0.0 {
                        self.scale_factor = v;
                    }
                }
            }
            "threshold" => {
                if let Ok(v) = value.parse::<f32>() {
                    if v.is_finite() && v >= 0.0 {
                        self.threshold_grams = v;
                    }
                }
            }
            "idle_interval" => {
                if let Ok(v) = value.parse::<u32>() {
                    self.idle_secs = v;
                }
            }
            "active_interval" => {
                if let Ok(v) = value.parse::<u32>() {
                    self.active_secs = v;
                }
            }
            "heartbeat_interval" => {
                if let Ok(v) = value.parse::<u32>() {
                    self.heartbeat_secs = v;
                }
            }
            "tare" => {
                if !value.is_empty() {
                    // Two ways to ask for a re-zero, both landing here:
                    //
                    // * a discovered Home Assistant *button*, whose payload is a
                    //   constant — every press looks identical, so the caller
                    //   deletes the retained message once we have acted on it;
                    // * a numeric token (the older automation that published a
                    //   timestamp), where a repeat of the same value is a
                    //   replay, not a new press. Remembering the token means
                    //   that keeps working, and doubles as a backstop if the
                    //   button's retained message ever fails to clear.
                    let token = value.parse::<u32>().unwrap_or(0);
                    if token == 0 || token != self.tare_token {
                        self.tare_token = token;
                        self.offset = tare_ref;
                    }
                }
            }
            "deep_sleep" => match value {
                "1" | "true" | "on" | "ON" => self.deep_sleep = true,
                "0" | "false" | "off" | "OFF" => self.deep_sleep = false,
                _ => {}
            },
            _ => {}
        }
        *self != before
    }

    // --- Serialisation ------------------------------------------------------
    fn to_bytes(self) -> [u8; BLOB_LEN] {
        let mut b = [0u8; BLOB_LEN];
        b[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        b[4] = VERSION;
        // b[5..8] padding stays zero
        b[8..12].copy_from_slice(&self.offset.to_le_bytes());
        b[12..16].copy_from_slice(&self.scale_factor.to_le_bytes());
        b[16..20].copy_from_slice(&self.threshold_grams.to_le_bytes());
        b[20..24].copy_from_slice(&self.idle_secs.to_le_bytes());
        b[24..28].copy_from_slice(&self.active_secs.to_le_bytes());
        b[28..32].copy_from_slice(&self.tare_token.to_le_bytes());
        b[32..36].copy_from_slice(&(self.deep_sleep as u32).to_le_bytes());
        b[36..40].copy_from_slice(&self.heartbeat_secs.to_le_bytes());
        let crc = crc32(&b[0..40]);
        b[40..44].copy_from_slice(&crc.to_le_bytes());
        b
    }

    fn from_bytes(b: &[u8; BLOB_LEN]) -> Option<Config> {
        if u32::from_le_bytes(b[0..4].try_into().ok()?) != MAGIC || b[4] != VERSION {
            return None;
        }
        if u32::from_le_bytes(b[40..44].try_into().ok()?) != crc32(&b[0..40]) {
            return None;
        }
        Some(Config {
            offset: i32::from_le_bytes(b[8..12].try_into().ok()?),
            scale_factor: f32::from_le_bytes(b[12..16].try_into().ok()?),
            threshold_grams: f32::from_le_bytes(b[16..20].try_into().ok()?),
            idle_secs: u32::from_le_bytes(b[20..24].try_into().ok()?),
            active_secs: u32::from_le_bytes(b[24..28].try_into().ok()?),
            tare_token: u32::from_le_bytes(b[28..32].try_into().ok()?),
            deep_sleep: u32::from_le_bytes(b[32..36].try_into().ok()?) != 0,
            heartbeat_secs: u32::from_le_bytes(b[36..40].try_into().ok()?),
        })
    }
}

/// Load the config from flash, or [`Config::DEFAULT`] if the sector is blank or
/// corrupt. Call this at boot, before the radio is up (flash access is happier
/// with Wi-Fi idle).
#[cfg(feature = "hal")]
pub fn load() -> Config {
    let mut flash = FlashStorage::new();
    let mut buf = [0u8; BLOB_LEN];
    match flash.read(FLASH_OFFSET, &mut buf) {
        Ok(()) => Config::from_bytes(&buf).unwrap_or(Config::DEFAULT),
        Err(_) => Config::DEFAULT,
    }
}

/// Persist the config to flash. Only call this when it actually changed — the
/// underlying `Storage::write` does a full 4 KiB read-erase-write cycle, so it
/// is both slow and finite-wear. `esp-storage` guards the operation with a
/// critical section, so it is safe to run with the radio still up just before
/// deep sleep. Errors are returned for logging; a failed write simply means the
/// change is lost, not corruption.
#[cfg(feature = "hal")]
pub fn store(config: &Config) -> Result<(), &'static str> {
    let mut flash = FlashStorage::new();
    flash
        .write(FLASH_OFFSET, &config.to_bytes())
        .map_err(|_| "nvs write")
}

// --- Node identity blob ------------------------------------------------------
// A board is normally built for one node (`NODE=` at compile time), but it can
// be *provisioned* to another without a rebuild; that override lives here. The
// layout is magic + version + length + the name + CRC-32, same discipline as the
// config blob above: anything that doesn't check out reads back as "no
// override", so a blank or half-written sector simply falls back to the
// build-time identity.

/// `"NODE"` little-endian.
const NODE_MAGIC: u32 = 0x4544_4F4E;
const NODE_VERSION: u8 = 1;
/// Longest node name we can store (`schlafzimmer` is 12).
pub const NODE_NAME_MAX: usize = 16;
/// magic(4) + version(1) + len(1) + pad(2) + name(16) + crc(4).
const NODE_BLOB_LEN: usize = 4 + 4 + NODE_NAME_MAX + 4;

/// Decode an identity blob. `None` for anything that does not check out — a
/// blank sector, an erased one (all `0xFF`), a stale version, a bad CRC or a
/// length that does not fit — which the caller reads as "no override".
fn decode_node_name(b: &[u8; NODE_BLOB_LEN]) -> Option<String<NODE_NAME_MAX>> {
    if u32::from_le_bytes(b[0..4].try_into().ok()?) != NODE_MAGIC || b[4] != NODE_VERSION {
        return None;
    }
    if u32::from_le_bytes(b[24..28].try_into().ok()?) != crc32(&b[0..24]) {
        return None;
    }
    let len = b[5] as usize;
    if len == 0 || len > NODE_NAME_MAX {
        return None;
    }
    let name = core::str::from_utf8(&b[8..8 + len]).ok()?;
    String::try_from(name).ok()
}

/// Encode an identity blob. Rejects a name that cannot be stored rather than
/// truncating it, since a truncated name would name a different node (or none).
fn encode_node_name(name: &str) -> Result<[u8; NODE_BLOB_LEN], &'static str> {
    if name.is_empty() || name.len() > NODE_NAME_MAX {
        return Err("node name length");
    }
    let mut b = [0u8; NODE_BLOB_LEN];
    b[0..4].copy_from_slice(&NODE_MAGIC.to_le_bytes());
    b[4] = NODE_VERSION;
    b[5] = name.len() as u8;
    b[8..8 + name.len()].copy_from_slice(name.as_bytes());
    let crc = crc32(&b[0..24]);
    b[24..28].copy_from_slice(&crc.to_le_bytes());
    Ok(b)
}

/// The provisioned node name, or `None` when this board runs as the identity it
/// was built with.
#[cfg(feature = "hal")]
pub fn load_node_name() -> Option<String<NODE_NAME_MAX>> {
    let mut flash = FlashStorage::new();
    let mut b = [0u8; NODE_BLOB_LEN];
    flash.read(NODE_OFFSET, &mut b).ok()?;
    decode_node_name(&b)
}

/// Persist a node name, so the next boot comes up as that node. Callers should
/// have validated the name first — an unknown one would leave the board falling
/// back to its build-time identity on every boot.
#[cfg(feature = "hal")]
pub fn store_node_name(name: &str) -> Result<(), &'static str> {
    let b = encode_node_name(name)?;
    FlashStorage::new()
        .write(NODE_OFFSET, &b)
        .map_err(|_| "node nvs write")
}

/// Drop the override, returning the board to its build-time identity.
#[cfg(feature = "hal")]
pub fn clear_node_name() -> Result<(), &'static str> {
    FlashStorage::new()
        .write(NODE_OFFSET, &[0u8; NODE_BLOB_LEN])
        .map_err(|_| "node nvs erase")
}

/// Standard CRC-32 (IEEE, reflected, poly `0xEDB88820`).
const fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    let mut i = 0;
    while i < data.len() {
        crc ^= data[i] as u32;
        let mut j = 0;
        while j < 8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            j += 1;
        }
        i += 1;
    }
    !crc
}

// Pin the CRC against the canonical check vector at compile time (same tactic as
// the sensor drivers, since the host test harness can't link this crate).
const _: () = {
    assert!(crc32(b"123456789") == 0xCBF4_3926);
    assert!(crc32(&[]) == 0);
};

#[cfg(test)]
mod tests {
    use super::*;

    /// A config that differs from `DEFAULT` in every field, so a serialisation
    /// bug that drops or transposes one shows up.
    fn sample() -> Config {
        Config {
            offset: -12_345,
            scale_factor: 512.5,
            threshold_grams: 7.5,
            idle_secs: 3,
            active_secs: 11,
            tare_token: 99,
            deep_sleep: false,
            heartbeat_secs: 1234,
        }
    }

    // --- Config blob --------------------------------------------------------

    #[test]
    fn config_blob_round_trips() {
        for cfg in [Config::DEFAULT, sample()] {
            let decoded = Config::from_bytes(&cfg.to_bytes()).expect("valid blob");
            assert!(decoded == cfg);
        }
    }

    #[test]
    fn config_blob_rejects_blank_and_erased_sectors() {
        // A never-written sector reads as zeroes on some parts and as 0xFF
        // (erased flash) on others; both must fall back to defaults.
        assert!(Config::from_bytes(&[0x00; BLOB_LEN]).is_none());
        assert!(Config::from_bytes(&[0xFF; BLOB_LEN]).is_none());
    }

    #[test]
    fn config_blob_rejects_a_stale_version() {
        let mut b = sample().to_bytes();
        b[4] = VERSION.wrapping_add(1);
        assert!(Config::from_bytes(&b).is_none());
    }

    #[test]
    fn config_blob_rejects_any_single_bit_flip() {
        // Every byte the CRC covers, and every bit of it. A flip must never
        // decode as a *different valid* config — that would silently move the
        // calibration.
        let good = sample().to_bytes();
        for byte in 0..BLOB_LEN {
            for bit in 0..8 {
                let mut b = good;
                b[byte] ^= 1 << bit;
                assert!(
                    Config::from_bytes(&b).is_none(),
                    "bit {bit} of byte {byte} decoded despite corruption"
                );
            }
        }
    }

    #[test]
    fn config_blob_rejects_a_half_written_sector() {
        // Power lost mid-write: a prefix of the new blob, the rest still erased.
        let good = sample().to_bytes();
        for kept in 1..BLOB_LEN {
            let mut b = [0xFFu8; BLOB_LEN];
            b[..kept].copy_from_slice(&good[..kept]);
            assert!(Config::from_bytes(&b).is_none(), "accepted {kept} bytes");
        }
    }

    // --- Config::apply ------------------------------------------------------

    #[test]
    fn apply_parses_every_key() {
        let mut cfg = Config::DEFAULT;
        assert!(cfg.apply("offset", "-500", 0));
        assert_eq!(cfg.offset, -500);
        assert!(cfg.apply("scale_factor", "123.5", 0));
        assert_eq!(cfg.scale_factor, 123.5);
        assert!(cfg.apply("threshold", "42", 0));
        assert_eq!(cfg.threshold_grams, 42.0);
        assert!(cfg.apply("idle_interval", "7", 0));
        assert_eq!(cfg.idle_secs, 7);
        assert!(cfg.apply("active_interval", "17", 0));
        assert_eq!(cfg.active_secs, 17);
        assert!(cfg.apply("heartbeat_interval", "900", 0));
        assert_eq!(cfg.heartbeat_secs, 900);
        assert!(cfg.apply("deep_sleep", "0", 0));
        assert!(!cfg.deep_sleep);
    }

    #[test]
    fn apply_reports_no_change_for_a_repeat() {
        // What keeps a retained message, re-delivered on every connect, from
        // rewriting flash for ever.
        let mut cfg = Config::DEFAULT;
        assert!(cfg.apply("idle_interval", "5", 0));
        assert!(!cfg.apply("idle_interval", "5", 0));
    }

    #[test]
    fn apply_ignores_unparseable_and_unknown_input() {
        let mut cfg = Config::DEFAULT;
        for (key, value) in [
            ("offset", "not-a-number"),
            ("idle_interval", "-1"), // u32
            ("idle_interval", ""),   //
            ("scale_factor", "0"),   // would divide by zero
            ("scale_factor", "-3"),  // would invert the reading
            ("scale_factor", "NaN"), // parses, but is not finite
            ("scale_factor", "inf"), //
            ("threshold", "-1"),     // negative grams
            ("deep_sleep", "maybe"), //
            ("no_such_key", "1"),    //
            ("", "1"),               //
        ] {
            assert!(!cfg.apply(key, value, 0), "{key}={value:?} was accepted");
        }
        assert!(cfg == Config::DEFAULT);
    }

    #[test]
    fn apply_accepts_every_boolean_spelling() {
        let mut cfg = Config::DEFAULT;
        for value in ["0", "false", "off", "OFF"] {
            cfg.deep_sleep = true;
            assert!(cfg.apply("deep_sleep", value, 0));
            assert!(!cfg.deep_sleep);
        }
        for value in ["1", "true", "on", "ON"] {
            cfg.deep_sleep = false;
            assert!(cfg.apply("deep_sleep", value, 0));
            assert!(cfg.deep_sleep);
        }
    }

    #[test]
    fn tare_adopts_the_baseline() {
        let mut cfg = Config::DEFAULT;
        // The discovered button sends a constant payload, so every press must
        // count — it is the caller that stops the retained message repeating.
        assert!(cfg.apply("tare", "tare", 4242));
        assert_eq!(cfg.offset, 4242);
        assert!(cfg.apply("tare", "tare", 99));
        assert_eq!(cfg.offset, 99);
    }

    #[test]
    fn tare_ignores_a_replayed_token_but_honours_a_new_one() {
        // The older automation published a timestamp; the same one arriving
        // again is the broker replaying, not a second press.
        let mut cfg = Config::DEFAULT;
        assert!(cfg.apply("tare", "1000", 4242));
        assert_eq!(cfg.offset, 4242);
        assert!(!cfg.apply("tare", "1000", 77));
        assert_eq!(cfg.offset, 4242);
        assert!(cfg.apply("tare", "1001", 77));
        assert_eq!(cfg.offset, 77);
    }

    #[test]
    fn tare_ignores_an_empty_payload() {
        // How a retained message is deleted — it must not read as a press.
        let mut cfg = Config::DEFAULT;
        assert!(!cfg.apply("tare", "", 4242));
        assert_eq!(cfg.offset, Config::DEFAULT.offset);
    }

    // --- Derived values -----------------------------------------------------

    #[test]
    fn threshold_ticks_never_collapses_to_zero() {
        // A zero threshold would make every sample look like a bird.
        let mut cfg = Config::DEFAULT;
        cfg.threshold_grams = 0.0;
        assert_eq!(cfg.threshold_ticks(), 1);
        cfg.threshold_grams = 10.0;
        cfg.scale_factor = f32::NAN;
        assert_eq!(cfg.threshold_ticks(), 1);
        cfg.scale_factor = 100.0;
        assert_eq!(cfg.threshold_ticks(), 1000);
    }

    #[test]
    fn heartbeat_wakes_is_at_least_one() {
        let mut cfg = Config::DEFAULT;
        // A zero interval would divide by zero; it clamps to one second, so the
        // heartbeat still lands on its configured period.
        cfg.idle_secs = 0;
        assert_eq!(cfg.heartbeat_wakes(), cfg.heartbeat_secs);
        cfg.idle_secs = 2;
        cfg.heartbeat_secs = 600;
        assert_eq!(cfg.heartbeat_wakes(), 300);
        cfg.heartbeat_secs = 1; // shorter than one wake
        assert_eq!(cfg.heartbeat_wakes(), 1);
    }

    #[test]
    fn write_grams_formats_fixed_point() {
        let mut cfg = Config::DEFAULT;
        cfg.offset = 0;
        cfg.scale_factor = 100.0;
        for (raw, expected) in [
            (0, "0.0"),
            (1000, "10.0"),
            (1005, "10.1"), // rounds away from zero
            (-1005, "-10.1"),
            (-50, "-0.5"),
        ] {
            let mut buf = String::new();
            cfg.write_grams(&mut buf, raw);
            assert_eq!(buf.as_str(), expected, "raw {raw}");
        }
    }

    #[test]
    fn write_grams_survives_a_broken_calibration() {
        // A zero scale factor must not produce `inf` or `NaN` on the wire.
        let mut cfg = Config::DEFAULT;
        cfg.offset = 0;
        cfg.scale_factor = 0.0;
        let mut buf = String::new();
        cfg.write_grams(&mut buf, 4200);
        assert!(buf
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == '-'));
    }

    // --- Node identity blob -------------------------------------------------

    #[test]
    fn node_blob_round_trips_every_fleet_name() {
        for (name, _) in crate::node::FLEET {
            let blob = encode_node_name(name).expect("encodable");
            assert_eq!(decode_node_name(&blob).as_deref(), Some(*name));
        }
    }

    #[test]
    fn node_blob_rejects_unstorable_names() {
        assert!(encode_node_name("").is_err());
        assert!(encode_node_name(&"x".repeat(NODE_NAME_MAX + 1)).is_err());
        // Exactly full is fine — the boundary the fleet must stay inside.
        assert!(encode_node_name(&"x".repeat(NODE_NAME_MAX)).is_ok());
    }

    #[test]
    fn node_blob_rejects_blank_erased_and_corrupt_sectors() {
        assert!(decode_node_name(&[0x00; NODE_BLOB_LEN]).is_none());
        assert!(decode_node_name(&[0xFF; NODE_BLOB_LEN]).is_none());

        let good = encode_node_name("kueche").unwrap();
        for byte in 0..NODE_BLOB_LEN {
            for bit in 0..8 {
                let mut b = good;
                b[byte] ^= 1 << bit;
                assert!(
                    decode_node_name(&b).is_none(),
                    "bit {bit} of byte {byte} decoded despite corruption"
                );
            }
        }
    }

    #[test]
    fn node_blob_rejects_a_half_written_sector() {
        let good = encode_node_name("schlafzimmer").unwrap();
        for kept in 1..NODE_BLOB_LEN {
            let mut b = [0xFFu8; NODE_BLOB_LEN];
            b[..kept].copy_from_slice(&good[..kept]);
            assert!(decode_node_name(&b).is_none(), "accepted {kept} bytes");
        }
    }

    #[test]
    fn node_blob_rejects_a_length_that_overruns_the_slot() {
        // A length field pointing past the name field, with the CRC recomputed
        // so only the bounds check stands between it and a panic.
        let mut b = encode_node_name("bad").unwrap();
        b[5] = NODE_NAME_MAX as u8 + 1;
        let crc = crc32(&b[0..24]);
        b[24..28].copy_from_slice(&crc.to_le_bytes());
        assert!(decode_node_name(&b).is_none());
    }
}

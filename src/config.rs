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

use core::time::Duration as CoreDuration;

use embedded_storage::{ReadStorage, Storage};
use esp_storage::FlashStorage;

/// Flash byte-offset of the config blob. Matches the `nvs` partition in
/// espflash's default table; we only touch the first sector of it.
const FLASH_OFFSET: u32 = 0x9000;

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
    /// Monotonic tare token; a new value from HA triggers a re-zero.
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
        // Battery nodes sleep by default; mains nodes ignore this flag anyway.
        deep_sleep: crate::node::NODE.power.is_battery(),
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
    /// `tare` is special: any *new* token re-zeros the scale by adopting
    /// `tare_ref` (the caller passes the current empty-house baseline) as the
    /// gram offset.
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
                if let Ok(token) = value.parse::<u32>() {
                    if token != self.tare_token {
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
pub fn store(config: &Config) -> Result<(), &'static str> {
    let mut flash = FlashStorage::new();
    flash
        .write(FLASH_OFFSET, &config.to_bytes())
        .map_err(|_| "nvs write")
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

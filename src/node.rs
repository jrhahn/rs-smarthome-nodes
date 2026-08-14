//! Per-node identity, sensor selection and power profile (#12, #17).
//!
//! One firmware image serves the whole fleet: which sensors are populated, what
//! the node is called, which MQTT namespace it publishes under and whether it
//! runs on battery or mains are all picked here, at build time, from the `NODE`
//! environment variable:
//!
//! ```text
//! NODE=kueche cargo run --release
//! ```
//!
//! An unknown name fails the build (const-eval panic) rather than silently
//! flashing the wrong personality onto a board. The default is `draussen`, the
//! original bird-feeder scale, whose topics are kept byte-for-byte compatible
//! with the pre-platform firmware.
//!
//! Build-time selection is deliberate for now (issue #12: "cargo features to
//! start; NVS later") — the sensor set decides which peripherals get
//! initialised, so it is not something a running node can change anyway.

use core::fmt::Write as _;

use heapless::String;

use crate::sensors::scd41::Mode as Scd41Mode;

/// How a node is powered, which decides whether it deep-sleeps between samples
/// or stays associated and loops (#17).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PowerProfile {
    /// Deep-sleep between samples; wakes cold and re-runs `main` (bird scale).
    Battery,
    /// Stays awake, keeps Wi-Fi up, samples on a fixed cadence. Required for the
    /// SDS011 fan and for CO₂ continuity.
    Mains,
}

impl PowerProfile {
    pub const fn is_battery(self) -> bool {
        matches!(self, PowerProfile::Battery)
    }
}

/// One sensor slot on a node: whether it is populated, plus how its readings are
/// named. `prefix` disambiguates keys when two sensors emit the same quantity
/// (the outdoor node has both a DS18B20 and an SHT31-D temperature), and `label`
/// does the same for the human-readable Home Assistant entity name.
#[derive(Clone, Copy)]
pub struct Slot {
    pub enabled: bool,
    pub prefix: &'static str,
    pub label: &'static str,
}

impl Slot {
    /// Populated, using each descriptor's plain key and name.
    pub const fn on() -> Slot {
        Slot {
            enabled: true,
            prefix: "",
            label: "",
        }
    }

    /// Populated, with its keys/names disambiguated, e.g. `("air_", "Luft")`
    /// turns `temperature` / "Temperatur" into `air_temperature` / "Luft
    /// Temperatur".
    pub const fn on_as(prefix: &'static str, label: &'static str) -> Slot {
        Slot {
            enabled: true,
            prefix,
            label,
        }
    }

    /// Not populated on this node.
    pub const fn off() -> Slot {
        Slot {
            enabled: false,
            prefix: "",
            label: "",
        }
    }
}

/// Everything that makes one board a specific node in the fleet.
#[derive(Clone, Copy)]
pub struct NodeConfig {
    /// Slug used in topics, unique ids and the client id, e.g. `"kueche"`.
    pub id: &'static str,
    /// Human-readable device name shown in Home Assistant, e.g. `"Küche"`.
    pub name: &'static str,
    /// Topic namespace: state topics are `<namespace>/<id>/<key>`.
    pub namespace: &'static str,
    pub power: PowerProfile,
    /// Mains nodes: seconds between sample+publish rounds. Battery nodes use the
    /// runtime idle/active intervals from [`crate::config::Config`] instead.
    pub sample_secs: u64,
    pub scale: Slot,
    pub ds18b20: Slot,
    pub sht31: Slot,
    pub scd41: Slot,
    pub sds011: Slot,
    /// Extra topic the weight is mirrored to, for a node whose Home Assistant
    /// entities predate MQTT discovery (the bird scale's `birds/scale/state`).
    pub legacy_weight_topic: Option<&'static str>,
}

impl NodeConfig {
    /// Does this node need the I²C bus brought up?
    pub const fn uses_i2c(&self) -> bool {
        self.sht31.enabled || self.scd41.enabled
    }

    /// Does this node need the UART brought up?
    pub const fn uses_uart(&self) -> bool {
        self.sds011.enabled
    }

    /// Battery nodes take one single-shot CO₂ sample per wake; mains nodes let
    /// the SCD41 run continuously, which is what its self-calibration expects.
    pub const fn scd41_mode(&self) -> Scd41Mode {
        if self.power.is_battery() {
            Scd41Mode::SingleShot
        } else {
            Scd41Mode::Periodic
        }
    }

    /// State topic for one reading: `<namespace>/<id>/<prefix><key>`.
    pub fn state_topic(&self, prefix: &str, key: &str) -> String<80> {
        let mut t = String::new();
        let _ = write!(t, "{}/{}/{}{}", self.namespace, self.id, prefix, key);
        t
    }

    /// Whether this node backs its Home Assistant availability with an MQTT
    /// last-will.
    ///
    /// Only mains nodes do. A battery node is *meant* to be disconnected almost
    /// all the time — it wakes, publishes, and drops the link again — so a will
    /// would mark it offline seconds after every reading. Those nodes rely on
    /// the discovery `expire_after` instead (see [`crate::discovery`]).
    pub const fn uses_lwt(&self) -> bool {
        !self.power.is_battery()
    }

    /// Retained topic carrying `online` / `offline` for a node with a last-will.
    pub fn availability_topic(&self) -> String<80> {
        self.state_topic("", "status")
    }

    /// Prefix under which Home Assistant publishes retained tuning values.
    pub fn config_prefix(&self) -> String<64> {
        let mut t = String::new();
        let _ = write!(t, "{}/{}/config/", self.namespace, self.id);
        t
    }

    /// Wildcard the firmware subscribes to while it is online.
    pub fn config_wildcard(&self) -> String<64> {
        let mut t = self.config_prefix();
        let _ = t.push('#');
        t
    }

    /// MQTT client id. Unique per node so two boards never fight over a session.
    pub fn client_id(&self) -> String<32> {
        let mut t = String::new();
        let _ = write!(t, "rs-{}", self.id);
        t
    }
}

// --- The fleet ---------------------------------------------------------------

/// Outdoor bird feeder: load cell + soil/air probe + SHT31-D, on battery.
/// Keeps the historical `birds/scale/...` topics so the existing Home Assistant
/// entities and retained config values survive the platform migration.
const DRAUSSEN: NodeConfig = NodeConfig {
    id: "scale",
    name: "Draußen",
    namespace: "birds",
    power: PowerProfile::Battery,
    sample_secs: 60,
    scale: Slot::on(),
    ds18b20: Slot::on(),
    // The probe already owns the plain `temperature` key, so the SHT31-D's
    // air measurements are published under `air_*`.
    sht31: Slot::on_as("air_", "Luft"),
    scd41: Slot::off(),
    sds011: Slot::off(),
    legacy_weight_topic: Some("birds/scale/state"),
};

const SCHLAFZIMMER: NodeConfig = NodeConfig {
    id: "schlafzimmer",
    name: "Schlafzimmer",
    namespace: "smarthome",
    power: PowerProfile::Mains,
    sample_secs: 60,
    scale: Slot::off(),
    ds18b20: Slot::off(),
    sht31: Slot::off(),
    scd41: Slot::on(),
    sds011: Slot::off(),
    legacy_weight_topic: None,
};

const WOHNZIMMER: NodeConfig = NodeConfig {
    id: "wohnzimmer",
    name: "Wohnzimmer",
    namespace: "smarthome",
    power: PowerProfile::Mains,
    sample_secs: 60,
    scale: Slot::off(),
    ds18b20: Slot::off(),
    sht31: Slot::off(),
    scd41: Slot::on(),
    sds011: Slot::off(),
    legacy_weight_topic: None,
};

/// Kitchen air quality. The SDS011 fan is duty-cycled, so samples are rare by
/// design: 15 minutes between rounds keeps the ~8000 h fan life plausible.
const KUECHE: NodeConfig = NodeConfig {
    id: "kueche",
    name: "Küche",
    namespace: "smarthome",
    power: PowerProfile::Mains,
    sample_secs: 900,
    scale: Slot::off(),
    ds18b20: Slot::off(),
    sht31: Slot::off(),
    scd41: Slot::off(),
    sds011: Slot::on(),
    legacy_weight_topic: None,
};

const BAD: NodeConfig = NodeConfig {
    id: "bad",
    name: "Bad",
    namespace: "smarthome",
    power: PowerProfile::Mains,
    sample_secs: 120,
    scale: Slot::off(),
    ds18b20: Slot::off(),
    sht31: Slot::on(),
    scd41: Slot::off(),
    sds011: Slot::off(),
    legacy_weight_topic: None,
};

/// The node this image is built for.
pub const NODE: NodeConfig = select(match option_env!("NODE") {
    Some(s) => s,
    None => "draussen",
});

/// Map the `NODE` build-time name onto a config. Unknown names panic during
/// const-eval, i.e. the build fails with the typo instead of producing a
/// plausible-looking image for the wrong room.
const fn select(id: &str) -> NodeConfig {
    if str_eq(id, "draussen") {
        DRAUSSEN
    } else if str_eq(id, "schlafzimmer") {
        SCHLAFZIMMER
    } else if str_eq(id, "wohnzimmer") {
        WOHNZIMMER
    } else if str_eq(id, "kueche") {
        KUECHE
    } else if str_eq(id, "bad") {
        BAD
    } else {
        panic!("unknown NODE; expected one of: draussen, schlafzimmer, wohnzimmer, kueche, bad")
    }
}

/// `str` equality usable in const context (`==` on `&str` is not const in 1.83).
const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

const _: () = {
    assert!(str_eq("bad", "bad"));
    assert!(!str_eq("bad", "bat"));
    assert!(!str_eq("bad", "bads"));
    // The fleet's power profiles decide which sensors are even legal: the
    // SDS011 fan and continuous CO₂ both need mains.
    assert!(!KUECHE.power.is_battery());
    assert!(!SCHLAFZIMMER.power.is_battery());
    assert!(DRAUSSEN.power.is_battery());
};

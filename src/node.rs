//! Per-node identity, sensor selection and power profile (#12, #17).
//!
//! One firmware image serves the whole fleet: which sensors are populated, what
//! the node is called, which MQTT namespace it publishes under and whether it
//! runs on battery or mains all come from the table below.
//!
//! Which entry is used is decided in two steps, at boot:
//!
//! 1. the identity the image was **built** for — `NODE=kueche cargo run
//!    --release`, defaulting to `draussen`, the original bird-feeder scale. An
//!    unknown name fails the build (const-eval panic) rather than silently
//!    flashing the wrong personality onto a board;
//! 2. a **provisioned** name stored in flash, which overrides it. That is what
//!    lets one generic image be flashed to every board and each one told what it
//!    is afterwards, over MQTT, without a rebuild — see [`provision_topic`].
//!
//! Because the sensor set decides which peripherals get initialised, the
//! identity is read once at boot ([`init`]) and then fixed for the run; a node
//! that is re-provisioned while running reboots into its new self.

use core::fmt::Write as _;
use core::ptr::{addr_of, addr_of_mut};

use heapless::String;
use log::{info, warn};

use crate::config;
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

/// Names accepted by `NODE=` and by provisioning, for error messages.
pub const KNOWN_NODES: &str = "draussen, schlafzimmer, wohnzimmer, kueche, bad";

/// The node this image was **built** for — the fallback when flash carries no
/// provisioned identity. Use [`active`] for the identity actually in force.
pub const BUILT_AS: NodeConfig = select(match option_env!("NODE") {
    Some(s) => s,
    None => "draussen",
});

/// Look a node up by name. Shared by the build-time selection and by runtime
/// provisioning, so both accept exactly the same set of names.
pub const fn by_name(id: &str) -> Option<NodeConfig> {
    if str_eq(id, "draussen") {
        Some(DRAUSSEN)
    } else if str_eq(id, "schlafzimmer") {
        Some(SCHLAFZIMMER)
    } else if str_eq(id, "wohnzimmer") {
        Some(WOHNZIMMER)
    } else if str_eq(id, "kueche") {
        Some(KUECHE)
    } else if str_eq(id, "bad") {
        Some(BAD)
    } else {
        None
    }
}

/// Map the `NODE` build-time name onto a config. Unknown names panic during
/// const-eval, i.e. the build fails with the typo instead of producing a
/// plausible-looking image for the wrong room.
const fn select(id: &str) -> NodeConfig {
    match by_name(id) {
        Some(cfg) => cfg,
        None => {
            panic!("unknown NODE; expected one of: draussen, schlafzimmer, wohnzimmer, kueche, bad")
        }
    }
}

// --- The identity in force ---------------------------------------------------

/// The node this board is running as. Written once by [`init`] before anything
/// reads it, then only read — the same single-context, single-word discipline
/// [`crate::state`] uses for its RTC-RAM cells.
static mut ACTIVE: NodeConfig = BUILT_AS;

/// The identity in force. Cheap: [`NodeConfig`] is `Copy` and lives in RAM.
pub fn active() -> NodeConfig {
    unsafe { addr_of!(ACTIVE).read() }
}

/// Resolve the identity for this boot: a provisioned name in flash wins over
/// the one the image was built with. Call once, early in `main`, before any
/// peripheral is set up — the sensor set decides which buses come up at all.
///
/// A stored name that is not in the table is reported and ignored rather than
/// obeyed, so a bad provisioning message degrades to the build-time identity
/// instead of a board that does nothing.
pub fn init() {
    let Some(stored) = config::load_node_name() else {
        return;
    };
    match by_name(&stored) {
        Some(cfg) => {
            unsafe { addr_of_mut!(ACTIVE).write(cfg) };
            info!("provisioned as node '{}' (from flash)", stored);
        }
        None => warn!(
            "flash names unknown node '{}'; falling back to build-time '{}'. Expected one of: {}",
            stored, BUILT_AS.id, KNOWN_NODES
        ),
    }
}

/// Topic a board listens on to be told what it is:
/// `smarthome/provision/<mac>`, with the MAC as lowercase hex, no separators.
///
/// Keyed by MAC rather than by node name for the obvious chicken-and-egg
/// reason — a board that does not yet know which node it is still knows its own
/// MAC. Publish **retained** so a node picks its identity up whenever it next
/// comes online:
///
/// ```text
/// mosquitto_pub -h <broker> -r -t smarthome/provision/a1b2c3d4e5f6 -m kueche
/// ```
///
/// The payload `default` clears the override, returning the board to the
/// identity it was flashed with.
pub fn provision_topic(mac: [u8; 6]) -> String<64> {
    let mut t = String::new();
    let _ = write!(t, "{}", PROVISION_PREFIX);
    for byte in mac {
        let _ = write!(t, "{:02x}", byte);
    }
    t
}

/// Fleet-wide provisioning namespace — deliberately not per-node, since the
/// point is to reach a board whose identity is not settled yet.
pub const PROVISION_PREFIX: &str = "smarthome/provision/";

/// Payload that clears a provisioned identity.
pub const PROVISION_RESET: &str = "default";

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
    // Every name must round-trip through the lookup, and nothing else may.
    assert!(by_name("kueche").is_some());
    assert!(by_name("Kueche").is_none());
    assert!(by_name("").is_none());
    // Node names have to fit the flash slot they are provisioned into.
    assert!("schlafzimmer".len() <= crate::config::NODE_NAME_MAX);
    // The fleet's power profiles decide which sensors are even legal: the
    // SDS011 fan and continuous CO₂ both need mains.
    assert!(!KUECHE.power.is_battery());
    assert!(!SCHLAFZIMMER.power.is_battery());
    assert!(DRAUSSEN.power.is_battery());
};

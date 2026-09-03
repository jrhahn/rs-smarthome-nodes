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
use core::ptr::addr_of;
#[cfg(feature = "hal")]
use core::ptr::addr_of_mut;

use heapless::String;
#[cfg(feature = "hal")]
use log::info;
use log::warn;

#[cfg(feature = "hal")]
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
/// (the bedroom's SCD41 and SHT31-D both report temperature and humidity), and
/// `label` does the same for the human-readable Home Assistant entity name. It
/// also just *names* things where there is nothing to collide with — the outdoor
/// node's `battery_voltage` is a plain voltage until the prefix says whose.
#[derive(Clone, Copy)]
pub struct Slot {
    pub enabled: bool,
    pub prefix: &'static str,
    pub label: &'static str,
    /// Keys that keep their plain form even though the slot carries a prefix.
    ///
    /// The prefix exists to stop two sensors on one node claiming the same
    /// entity. A quantity only this sensor measures has nothing to collide
    /// with, and renaming it would throw away its Home Assistant history for
    /// nothing — `co2` on a node whose SCD41 shares the bus with an SHT31 is
    /// exactly that case: the SHT31 takes over `temperature` and `humidity`,
    /// while the CO₂ curve carries on under the id it has always had.
    pub unprefixed: &'static [&'static str],
    /// Seconds between this sensor's own measurements, or `0` to run it on
    /// every round like everything else.
    ///
    /// One node can carry sensors whose natural cadences are an order of
    /// magnitude apart: an SCD41 wants a reading a minute, while the SDS011's
    /// fan is a ~8000 h consumable that must not spin more than about four
    /// times an hour. Before this existed the node had a single `sample_secs`,
    /// so the two had to be traded off against each other — the fan dragged
    /// the whole node down to its own cadence. Now [`NodeConfig::sample_secs`]
    /// is the *base* round, and a slot may sit out rounds it does not need.
    ///
    /// Rounded up to a whole number of base rounds (see
    /// [`Slot::rounds_between`]): the loop only wakes on the base cadence, so
    /// a period between two multiples would otherwise drift.
    pub period_secs: u32,
    /// Whether this sensor's readings are corrected for ambient humidity, and
    /// its uncorrected values published alongside them (see
    /// [`crate::sensors::sds011::compensate`]). Only the SDS011 honours it, and
    /// only a node that also carries an SHT31 may set it — a const assert below
    /// enforces that, since the correction has nothing to work from otherwise.
    pub compensated: bool,
}

impl Slot {
    /// Populated, using each descriptor's plain key and name.
    pub const fn on() -> Slot {
        Slot {
            enabled: true,
            prefix: "",
            label: "",
            unprefixed: &[],
            period_secs: 0,
            compensated: false,
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
            unprefixed: &[],
            period_secs: 0,
            compensated: false,
        }
    }

    /// Exempt these keys from the slot's prefix (see [`Slot::unprefixed`]).
    pub const fn keeping(self, unprefixed: &'static [&'static str]) -> Slot {
        Slot { unprefixed, ..self }
    }

    /// Run this sensor on its own, slower cadence (see [`Slot::period_secs`]).
    pub const fn every(self, period_secs: u32) -> Slot {
        Slot {
            period_secs,
            ..self
        }
    }

    /// Correct this sensor against the node's SHT31 and publish the raw values
    /// too (see [`Slot::compensated`]).
    pub const fn compensated(self) -> Slot {
        Slot {
            compensated: true,
            ..self
        }
    }

    /// Not populated on this node.
    pub const fn off() -> Slot {
        Slot {
            enabled: false,
            prefix: "",
            label: "",
            unprefixed: &[],
            period_secs: 0,
            compensated: false,
        }
    }

    /// The key prefix that applies to `key` — empty for an exempted one.
    pub fn prefix_for(&self, key: &str) -> &'static str {
        if self.is_exempt(key) {
            ""
        } else {
            self.prefix
        }
    }

    /// The entity-name label that applies to `key`, same rule as
    /// [`Slot::prefix_for`] so the id and the display name never disagree.
    pub fn label_for(&self, key: &str) -> &'static str {
        if self.is_exempt(key) {
            ""
        } else {
            self.label
        }
    }

    fn is_exempt(&self, key: &str) -> bool {
        self.unprefixed.contains(&key)
    }

    /// How many base rounds this slot sits out between measurements, given the
    /// node's base cadence. Always at least 1, so a slot with no period of its
    /// own — or one shorter than a single round — simply runs every time.
    ///
    /// Rounds **up**: with a 60 s base and a 900 s period that is every 15th
    /// round; with a 100 s period it is every second round (200 s), because
    /// running early would spin the fan more often than asked, and the fan is
    /// the whole reason the knob exists.
    pub const fn rounds_between(&self, base_secs: u64) -> u32 {
        if self.period_secs == 0 {
            return 1;
        }
        let base = if base_secs == 0 { 1 } else { base_secs };
        let rounds = (self.period_secs as u64).div_ceil(base);
        if rounds == 0 {
            1
        } else {
            rounds as u32
        }
    }

    /// The seconds this slot actually publishes on, i.e. its period rounded to
    /// whole base rounds. What Home Assistant must expire it against.
    pub const fn effective_secs(&self, base_secs: u64) -> u64 {
        let base = if base_secs == 0 { 1 } else { base_secs };
        base * self.rounds_between(base_secs) as u64
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
    /// Cell-voltage sense through the external divider (see [`crate::battery`]).
    /// Not a sensor on a bus but a property of how the board is powered, so it
    /// only ever belongs on a node with [`PowerProfile::Battery`] — and never
    /// alongside `ds18b20`, which wants the same pin.
    pub battery: Slot,
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

/// Outdoor bird feeder: load cell + SHT31-D + cell-voltage sense, on battery.
/// Keeps the historical `birds/scale/...` topics so the existing Home Assistant
/// entities and retained config values survive the platform migration.
const DRAUSSEN: NodeConfig = NodeConfig {
    id: "scale",
    name: "Draußen",
    namespace: "birds",
    power: PowerProfile::Battery,
    sample_secs: 60,
    scale: Slot::on(),
    // The DS18B20 is gone from this node, and it is the battery sense that
    // displaced it: both want D2, the only ADC1 pad the HX711 has not taken.
    ds18b20: Slot::off(),
    // The `air_` prefix outlives its original reason — it was there to keep the
    // probe's plain `temperature` key to itself — because renaming the entity
    // now would orphan its Home Assistant history, and the SHT31-D really is
    // measuring the air rather than the feeder.
    sht31: Slot::on_as("air_", "Luft"),
    scd41: Slot::off(),
    sds011: Slot::off(),
    battery: Slot::on_as("battery_", "Batterie"),
    legacy_weight_topic: Some("birds/scale/state"),
};

/// Bedroom air. The SHT31-D is not redundant with the SCD41's built-in RH/T:
/// the SCD4x datasheet specifies its humidity at ±6 %RH (±9 outside 15–35 °C /
/// 20–65 %RH) against the SHT31-D's ±2 %, because it sits on a die that heats
/// itself for the CO₂ measurement. Measured side by side on 2026-08-26 the two
/// disagreed by 15 points. So the SHT31-D owns `temperature` and `humidity`,
/// and the SCD41's own pair is published under `scd41_` — still worth having,
/// because calibrating its temperature offset against a reference is exactly
/// Sensirion's field procedure and needs both numbers visible. `co2` keeps its
/// plain key: nothing collides with it, and its history is worth preserving.
const SCHLAFZIMMER: NodeConfig = NodeConfig {
    id: "schlafzimmer",
    name: "Schlafzimmer",
    namespace: "smarthome",
    power: PowerProfile::Mains,
    sample_secs: 60,
    scale: Slot::off(),
    ds18b20: Slot::off(),
    sht31: Slot::on(),
    scd41: Slot::on_as("scd41_", "SCD41").keeping(&["co2"]),
    sds011: Slot::off(),
    battery: Slot::off(),
    legacy_weight_topic: None,
};

/// Same build as [`SCHLAFZIMMER`], same reasoning.
const WOHNZIMMER: NodeConfig = NodeConfig {
    id: "wohnzimmer",
    name: "Wohnzimmer",
    namespace: "smarthome",
    power: PowerProfile::Mains,
    sample_secs: 60,
    scale: Slot::off(),
    ds18b20: Slot::off(),
    sht31: Slot::on(),
    scd41: Slot::on_as("scd41_", "SCD41").keeping(&["co2"]),
    sds011: Slot::off(),
    battery: Slot::off(),
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
    battery: Slot::off(),
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
    battery: Slot::off(),
    legacy_weight_topic: None,
};

/// The fleet, keyed by the name `NODE=` and provisioning accept. The single
/// source of truth: [`by_name`] walks it, so a node added here is immediately
/// selectable both ways.
pub const FLEET: &[(&str, NodeConfig)] = &[
    ("draussen", DRAUSSEN),
    ("schlafzimmer", SCHLAFZIMMER),
    ("wohnzimmer", WOHNZIMMER),
    ("kueche", KUECHE),
    ("bad", BAD),
];

// D2 / GPIO4 carries either the DS18B20's 1-Wire line or the battery divider's
// tap on ADC1 — the pin cannot be both, and `main` hands it to whichever slot is
// enabled. A node asking for both would compile, then quietly give the pin to
// the probe and leave someone wondering why the voltage never arrives; fail the
// build here instead, where the table that caused it is on screen.
const _: () = {
    let mut i = 0;
    while i < FLEET.len() {
        assert!(
            !(FLEET[i].1.ds18b20.enabled && FLEET[i].1.battery.enabled),
            "a node cannot carry both a DS18B20 and a battery divider: they share D2"
        );
        i += 1;
    }
};

// Humidity compensation reads the node's own SHT31 (see `Slot::compensated`).
// A node asking for it without one would build, then publish a "corrected"
// value that is simply the raw one — the most misleading outcome available, and
// exactly the class of silent-but-wrong the fleet has been bitten by before.
// The flag is also meaningless on any slot but the SDS011, so say that here
// rather than let it sit in a table looking effective.
const _: () = {
    let mut i = 0;
    while i < FLEET.len() {
        let n = FLEET[i].1;
        assert!(
            !(n.sds011.compensated && !n.sht31.enabled),
            "humidity compensation needs an SHT31 on the same node to read from"
        );
        assert!(
            !(n.scale.compensated
                || n.ds18b20.compensated
                || n.sht31.compensated
                || n.scd41.compensated
                || n.battery.compensated),
            "only the SDS011 slot honours humidity compensation"
        );
        i += 1;
    }
};

/// The same names as one string, for error messages. Spelled out rather than
/// built from [`FLEET`] because it is used in a const-eval `panic!`, which takes
/// a literal; a test keeps the two in step.
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
    let mut i = 0;
    while i < FLEET.len() {
        if str_eq(id, FLEET[i].0) {
            return Some(FLEET[i].1);
        }
        i += 1;
    }
    None
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
#[cfg(feature = "hal")]
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

// --- Provisioning requests ---------------------------------------------------

/// What a retained provisioning message asks this board to become.
#[derive(Debug, PartialEq, Eq)]
pub enum Provision {
    /// Run as this node from the next boot (already checked against the table).
    Become(String<{ crate::config::NODE_NAME_MAX }>),
    /// Drop the stored override and go back to the built-in identity.
    Reset,
}

/// Interpret a provisioning payload, or `None` if there is nothing to do.
///
/// "Nothing to do" is the common case and matters: the message is retained, so
/// it is re-delivered on *every* connect. Only an actual change may reach flash,
/// or a board would rewrite its identity sector for the rest of its life.
///
/// `current` is the identity in force and `has_override` says whether flash
/// carries one — both passed in rather than read here, so the decision itself
/// stays pure and testable.
pub fn provision_request(
    value: &str,
    current: &NodeConfig,
    has_override: bool,
) -> Option<Provision> {
    // A zero-length payload is how a retained message is cleared, not a request.
    if value.is_empty() {
        return None;
    }

    if value == PROVISION_RESET {
        return has_override.then_some(Provision::Reset);
    }

    match by_name(value) {
        // Compared by node id, not by name: the outdoor node answers to
        // `draussen` but its id — and its topics — say `scale`.
        Some(cfg) if cfg.id == current.id => None,
        Some(_) => Some(Provision::Become(String::try_from(value).ok()?)),
        None => {
            warn!(
                "provisioning asked for unknown node '{}'; expected one of: {}",
                value, KNOWN_NODES
            );
            None
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NODE_NAME_MAX;

    #[test]
    fn an_exempt_key_keeps_its_plain_form() {
        // The prefix is there to stop two sensors claiming one entity. `co2`
        // has no rival on the node, so prefixing it would only cost its Home
        // Assistant history.
        let slot = Slot::on_as("scd41_", "SCD41").keeping(&["co2"]);
        assert_eq!(slot.prefix_for("co2"), "");
        assert_eq!(slot.label_for("co2"), "");
        assert_eq!(slot.prefix_for("temperature"), "scd41_");
        assert_eq!(slot.label_for("temperature"), "SCD41");
    }

    #[test]
    fn the_key_and_the_name_are_exempted_together() {
        // An id that says `co2` under a name that says "SCD41 CO₂" would be a
        // confusing half-rename, so both sides consult the same rule.
        for slot in [
            Slot::on(),
            Slot::on_as("air_", "Luft"),
            Slot::on_as("scd41_", "SCD41").keeping(&["co2"]),
        ] {
            for key in ["co2", "temperature", "humidity"] {
                assert_eq!(
                    slot.prefix_for(key).is_empty(),
                    slot.label_for(key).is_empty(),
                    "{key} is prefixed on only one side"
                );
            }
        }
    }

    #[test]
    fn a_node_carrying_both_i2c_sensors_gives_the_room_readings_to_the_sht31() {
        // The SCD4x specifies its humidity at ±6 %RH against the SHT31-D's ±2 %,
        // so the plain `temperature`/`humidity` entities — the ones a dashboard
        // reaches for — must come from the SHT31-D.
        for name in ["schlafzimmer", "wohnzimmer"] {
            let node = by_name(name).unwrap();
            assert!(node.sht31.enabled, "{name} has no SHT31-D");
            assert!(node.scd41.enabled, "{name} has no SCD41");
            assert_eq!(node.sht31.prefix_for("humidity"), "");
            assert_ne!(node.scd41.prefix_for("humidity"), "");
            // ...while CO₂, which only one of them measures, stays put.
            assert_eq!(node.scd41.prefix_for("co2"), "");
        }
    }

    #[test]
    fn every_fleet_name_resolves_and_nothing_else_does() {
        for (name, cfg) in FLEET {
            assert_eq!(by_name(name).expect("in the table").id, cfg.id);
        }
        for name in ["", "Draussen", "draussen ", "küche", "kuche", "scale"] {
            assert!(by_name(name).is_none(), "{name:?} resolved");
        }
    }

    #[test]
    fn known_nodes_lists_exactly_the_fleet() {
        // The message a mistyped `NODE=` prints. It has to be a literal (it is
        // used in a const-eval panic), so nothing but this test keeps it honest.
        let listed: Vec<&str> = KNOWN_NODES.split(", ").collect();
        let actual: Vec<&str> = FLEET.iter().map(|(name, _)| *name).collect();
        assert_eq!(listed, actual);
    }

    #[test]
    fn node_ids_and_names_are_unique() {
        // Two nodes sharing an id would share topics, a Home Assistant device
        // and an MQTT client id — they would fight over the broker session.
        for (i, (name_a, a)) in FLEET.iter().enumerate() {
            for (name_b, b) in &FLEET[i + 1..] {
                assert_ne!(a.id, b.id, "{name_a} and {name_b} share an id");
                assert_ne!(name_a, name_b);
            }
        }
    }

    #[test]
    fn every_fleet_name_fits_the_flash_slot() {
        // A name that does not fit could be built but never provisioned.
        for (name, _) in FLEET {
            assert!(name.len() <= NODE_NAME_MAX, "{name} is too long to store");
        }
    }

    #[test]
    fn topics_are_built_from_the_identity() {
        let scale = by_name("draussen").unwrap();
        assert_eq!(
            scale.state_topic("", "weight").as_str(),
            "birds/scale/weight"
        );
        assert_eq!(
            scale.state_topic("air_", "temperature").as_str(),
            "birds/scale/air_temperature"
        );
        assert_eq!(scale.config_prefix().as_str(), "birds/scale/config/");
        assert_eq!(scale.config_wildcard().as_str(), "birds/scale/config/#");
        assert_eq!(scale.client_id().as_str(), "rs-scale");
        assert_eq!(scale.availability_topic().as_str(), "birds/scale/status");
    }

    #[test]
    fn no_topic_is_truncated() {
        // The heapless buffers are fixed; a longer node would silently lose the
        // tail of its topic and publish somewhere unexpected.
        for (_, node) in FLEET {
            let longest_key = "heartbeat_interval";
            assert!(node.state_topic("air_", longest_key).ends_with(longest_key));
            assert!(node.config_wildcard().ends_with('#'));
            assert!(node.client_id().starts_with("rs-"));
        }
    }

    #[test]
    fn the_power_profile_decides_sleep_and_availability() {
        for (name, node) in FLEET {
            // A battery node is offline by design between readings, so a last
            // will would declare it dead after every single publish.
            assert_eq!(
                node.uses_lwt(),
                !node.power.is_battery(),
                "{name} availability does not match its power profile"
            );
            // Single-shot CO₂ on battery; periodic is what ASC expects on mains.
            assert_eq!(
                node.scd41_mode() == crate::sensors::scd41::Mode::SingleShot,
                node.power.is_battery(),
                "{name} SCD41 mode does not match its power profile"
            );
        }
    }

    #[test]
    fn bus_setup_follows_the_populated_sensors() {
        for (name, node) in FLEET {
            assert_eq!(
                node.uses_i2c(),
                node.sht31.enabled || node.scd41.enabled,
                "{name} I²C"
            );
            assert_eq!(node.uses_uart(), node.sds011.enabled, "{name} UART");
        }
    }

    #[test]
    fn the_fan_and_continuous_co2_never_land_on_battery() {
        // Both rule out deep sleep: the SDS011's fan has to spin up per sample,
        // and the SCD41's self-calibration assumes it keeps running.
        for (name, node) in FLEET {
            if node.sds011.enabled {
                assert!(!node.power.is_battery(), "{name} runs a fan on battery");
            }
        }
    }

    #[test]
    fn provision_topic_is_keyed_by_mac() {
        let topic = provision_topic([0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6]);
        assert_eq!(topic.as_str(), "smarthome/provision/a1b2c3d4e5f6");
        // Leading zeroes must survive, or two boards could share a topic.
        let topic = provision_topic([0x00, 0x01, 0x02, 0x03, 0x04, 0x05]);
        assert_eq!(topic.as_str(), "smarthome/provision/000102030405");
        assert!(topic.starts_with(PROVISION_PREFIX));
    }

    #[test]
    fn only_the_scale_keeps_a_legacy_topic() {
        for (name, node) in FLEET {
            match node.legacy_weight_topic {
                Some(topic) => {
                    assert_eq!(*name, "draussen");
                    assert!(node.scale.enabled, "{name} mirrors a weight it never reads");
                    assert_eq!(topic, "birds/scale/state");
                }
                None => assert!(*name != "draussen"),
            }
        }
    }

    #[test]
    fn slots_disambiguate_colliding_keys() {
        // The bedroom has two temperature and humidity sources. Without a prefix
        // they would publish to the same topics and one would overwrite the
        // other.
        let bedroom = by_name("schlafzimmer").unwrap();
        assert!(bedroom.scd41.enabled && bedroom.sht31.enabled);
        assert_ne!(bedroom.scd41.prefix, bedroom.sht31.prefix);
    }

    #[test]
    fn nothing_double_books_the_one_pin_two_things_want() {
        // The runtime half of the const assert above: D2 belongs to the DS18B20
        // or to the battery divider, never both.
        for (name, node) in FLEET {
            assert!(
                !(node.ds18b20.enabled && node.battery.enabled),
                "{name} claims D2 twice"
            );
        }
    }

    #[test]
    fn only_a_battery_node_senses_a_cell() {
        // A mains node has no cell to read, so the entity would sit at whatever
        // the divider that is not fitted happens to leave on the pin.
        for (name, node) in FLEET {
            if node.battery.enabled {
                assert!(
                    node.power.is_battery(),
                    "{name} senses a battery it does not run on"
                );
            }
        }
    }

    #[test]
    fn the_outdoor_sht31_keeps_the_prefix_the_probe_gave_it() {
        // `air_` was introduced to leave the plain `temperature` key to the
        // DS18B20, and the DS18B20 is gone. The prefix stays regardless:
        // renaming would orphan the history behind `birds/scale/air_temperature`
        // in Home Assistant, and the sensor really does measure the air.
        let scale = by_name("draussen").unwrap();
        assert!(!scale.ds18b20.enabled);
        assert_eq!(scale.sht31.prefix, "air_");
        assert_eq!(
            scale
                .state_topic(scale.battery.prefix_for("voltage"), "voltage")
                .as_str(),
            "birds/scale/battery_voltage"
        );
    }

    // --- Provisioning -------------------------------------------------------

    #[test]
    fn an_empty_payload_is_not_a_request() {
        // How a retained message is deleted. Acting on it would re-provision
        // every board whose provisioning was just cleared.
        let scale = by_name("draussen").unwrap();
        assert_eq!(provision_request("", &scale, true), None);
        assert_eq!(provision_request("", &scale, false), None);
    }

    #[test]
    fn reset_only_does_something_when_there_is_an_override() {
        let scale = by_name("draussen").unwrap();
        assert_eq!(
            provision_request(PROVISION_RESET, &scale, true),
            Some(Provision::Reset)
        );
        // Nothing stored: the message is retained and arrives on every connect,
        // so obeying it would erase the same sector for ever.
        assert_eq!(provision_request(PROVISION_RESET, &scale, false), None);
    }

    #[test]
    fn being_told_what_it_already_is_changes_nothing() {
        let scale = by_name("draussen").unwrap();
        assert_eq!(provision_request("draussen", &scale, false), None);
        assert_eq!(provision_request("draussen", &scale, true), None);
    }

    #[test]
    fn a_different_node_is_adopted() {
        let scale = by_name("draussen").unwrap();
        for (name, _) in FLEET.iter().filter(|(n, _)| *n != "draussen") {
            assert_eq!(
                provision_request(name, &scale, false),
                Some(Provision::Become(String::try_from(*name).unwrap())),
                "{name}"
            );
        }
    }

    #[test]
    fn an_unknown_name_is_ignored() {
        let scale = by_name("draussen").unwrap();
        for value in ["kuche", "Draussen", "scale", "  ", "0"] {
            assert_eq!(provision_request(value, &scale, false), None, "{value}");
        }
    }

    #[test]
    fn the_node_is_matched_by_id_not_by_name() {
        // `draussen` is the name; `scale` is the id its topics use. A board
        // already running as it must not re-provision itself in a loop.
        let scale = by_name("draussen").unwrap();
        assert_eq!(scale.id, "scale");
        assert_eq!(provision_request("draussen", &scale, false), None);
    }

    #[test]
    fn a_slot_with_no_period_of_its_own_runs_every_round() {
        for base in [1, 60, 120, 900] {
            assert_eq!(Slot::on().rounds_between(base), 1);
            assert_eq!(Slot::on().effective_secs(base), base);
        }
    }

    #[test]
    fn a_slow_slot_rounds_up_to_whole_base_rounds() {
        // The loop only wakes on the base cadence, so a period that is not a
        // multiple of it has to land somewhere. Up, never down: the fan is the
        // reason the knob exists, and running it early defeats the purpose.
        let base = 60;
        assert_eq!(Slot::on().every(900).rounds_between(base), 15);
        assert_eq!(Slot::on().every(900).effective_secs(base), 900);
        assert_eq!(Slot::on().every(100).rounds_between(base), 2);
        assert_eq!(Slot::on().every(100).effective_secs(base), 120);
        // A period shorter than one round cannot be honoured; every round is
        // the closest the loop can get, and it is never *slower* than asked.
        assert_eq!(Slot::on().every(5).rounds_between(base), 1);
        assert_eq!(Slot::on().every(60).rounds_between(base), 1);
        assert_eq!(Slot::on().every(61).rounds_between(base), 2);
    }

    #[test]
    fn a_zero_base_never_divides_by_zero() {
        // `sample_secs` is clamped to at least 1 by the loop, but the table is
        // hand-written and a 0 there must not take the arithmetic with it.
        assert_eq!(Slot::on().every(900).rounds_between(0), 900);
        assert_eq!(Slot::on().effective_secs(0), 1);
    }

    #[test]
    fn every_fleet_period_is_a_whole_number_of_base_rounds() {
        // Otherwise the node publishes on a cadence nobody wrote down: the
        // table says 900 s and the sensor actually runs every 960.
        for (name, node) in FLEET {
            for (what, slot) in [
                ("sht31", node.sht31),
                ("scd41", node.scd41),
                ("sds011", node.sds011),
            ] {
                if slot.period_secs == 0 {
                    continue;
                }
                assert_eq!(
                    slot.effective_secs(node.sample_secs),
                    slot.period_secs as u64,
                    "{name}/{what}: {} s does not divide into {} s rounds",
                    slot.period_secs,
                    node.sample_secs
                );
            }
        }
    }
}

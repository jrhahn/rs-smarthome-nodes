//! Home Assistant MQTT auto-discovery (#16).
//!
//! On every connect the firmware publishes one **retained** config message per
//! reading to `homeassistant/sensor/<node>/<key>/config`. Home Assistant creates
//! the entities from those, groups them under a single device (identified by the
//! node id) and remembers them across restarts — so adding a node to the fleet
//! no longer means hand-declaring entities in the home-server nix config.
//!
//! Both directions are discovered. The readings become `sensor` entities; the
//! calibration and tuning knobs the firmware reads back off
//! `<namespace>/<node>/config/<key>` become `number` / `switch` / `button`
//! entities (see [`controls`]), so the Home Assistant side of a node needs no
//! hand-written YAML at all.
//!
//! This module only *builds* the topic/payload pairs; `main` owns the MQTT
//! client and does the publishing, which keeps the rust-mqtt types out of here.
//!
//! **A newly discovered entity loses its first reading.** In the round that
//! announces it, the config message and the state publish leave a few
//! milliseconds apart, and Home Assistant has not finished creating the entity
//! when the state arrives. State topics are QoS0 and not retained, so that
//! reading is simply dropped and the entity sits at `unavailable` until the
//! next round. Nothing is broken and nothing needs fixing — `expire_after` is
//! several rounds wide, so the entity never expires over it — but it is worth
//! knowing before diagnosing the node: an entity that stays unavailable while
//! its siblings publish is new, not silent. Its siblings are unaffected
//! precisely because Home Assistant already knows them.
//!
//! Seen when the RSSI entity arrived: the bath and kitchen nodes both logged
//! `rssi = ...` and `Published ... to smarthome/<node>/rssi` on their first
//! round after flashing, while Home Assistant showed `unavailable` until the
//! second. On a battery node, where rounds are ten minutes apart, that gap is
//! ten minutes long.
//!
//! Payloads use Home Assistant's abbreviated keys (`stat_t`, `dev_cla`, …) and
//! the `~` base-topic shorthand, mostly to keep them inside the small MQTT
//! buffers a no_std node can afford.

use core::fmt::Write as _;

use heapless::{String, Vec};

use crate::battery;
use crate::config::Config;
use crate::ds18b20;
use crate::node::{NodeConfig, Slot};
use crate::reset_reason;
use crate::rssi;
use crate::sensors::{scale, scd41, sds011, sgp41, sht31, EntityDescriptor};

/// Upper bound on entities a node can expose (weight, probe temperature,
/// SHT31 ×2, SCD41 ×3, SDS011 ×2, cell voltage, link RSSI, reset diagnostics
/// ×2), with headroom.
///
/// Raised twice now, both times by a per-node diagnostic rather than a sensor:
/// from 12 when the RSSI entity arrived, and from 14 for the reset pair, which
/// took the living room with the gas sensor on to exactly 14 of 14. `entities`
/// pushes with `let _ =`, which drops silently, so hitting the cap would lose
/// whatever sorts last — the battery divider — with nothing logged.
/// `no_node_overflows_the_entity_vec` is what turns that into a failing test
/// rather than a missing sensor, and it earned its keep here.
pub const MAX_ENTITIES: usize = 16;
/// Upper bound on command entities (the calibration and tuning knobs).
pub const MAX_CONTROLS: usize = 12;

/// Room for one discovery payload. The longest today is ~390 B (a `number`
/// control on a node with a last will and a long name); the headroom is for the
/// device block, which grows with the node's name. An over-long payload is
/// dropped rather than truncated, so this only ever costs an entity.
pub const PAYLOAD_MAX: usize = 448;

/// A rendered discovery payload.
pub type Payload = String<PAYLOAD_MAX>;

/// Discovery prefix Home Assistant listens on (its default).
pub const PREFIX: &str = "homeassistant";
/// Manufacturer/model reported for the device card in Home Assistant.
const MANUFACTURER: &str = "rs-smarthome-nodes";
const MODEL: &str = "ESP32-C3";

/// Availability payloads. These are Home Assistant's defaults, so the discovery
/// config does not have to name them.
pub const PAYLOAD_ONLINE: &[u8] = b"online";
pub const PAYLOAD_OFFLINE: &[u8] = b"offline";

/// Never let a node be declared missing sooner than this, however tight the
/// configured publish interval is — one lost packet must not blank the entity.
const MIN_EXPIRY_SECS: u32 = 120;
/// Missed publish rounds tolerated before Home Assistant calls a node missing.
/// One miss is routine (a lost packet, a failed Wi-Fi join); three in a row is
/// a node that has actually stopped.
const MISSED_ROUNDS: u32 = 3;

/// How Home Assistant learns that a node has gone quiet. The two mechanisms are
/// complementary, so a mains node uses both:
///
/// * `lwt` — the broker publishes `offline` to the node's availability topic
///   when the connection drops unexpectedly. Immediate, but silent if the node
///   dies while it is (legitimately) not connected.
/// * `expire_after` — Home Assistant invalidates the values itself when nothing
///   arrives in time. Slower, but catches the node that dies between rounds,
///   which is *every* dead battery node.
pub struct Availability {
    pub lwt: bool,
    /// Seconds between the node's *base* publish rounds. Not itself an expiry:
    /// a slot running on its own slower cadence is expired against that
    /// instead (see [`Availability::expire_for`]).
    pub period_secs: u32,
}

/// The availability policy for this node, given the live config (a battery node
/// publishes on its heartbeat, a mains node on its fixed cadence).
pub fn availability(node: &NodeConfig, cfg: &Config) -> Availability {
    let period = if node.power.is_battery() {
        cfg.heartbeat_secs
    } else {
        // Clamped to u32 seconds; no node samples anywhere near that rarely.
        node.sample_secs.min(u32::MAX as u64) as u32
    };
    Availability {
        lwt: node.uses_lwt(),
        period_secs: period,
    }
}

impl Availability {
    /// How long Home Assistant should wait before invalidating one slot's
    /// entities.
    ///
    /// Per slot rather than per node, because a slot may publish on its own
    /// slower cadence: the SDS011's fan runs a few times an hour while the
    /// SCD41 beside it reports every minute. Expiring the PM entities against
    /// the *node's* round would blank them within three minutes of every
    /// reading — they would spend almost all their life "unavailable", which
    /// looks exactly like a broken sensor.
    pub fn expire_for(&self, slot: Slot) -> u32 {
        let period = slot
            .effective_secs(self.period_secs as u64)
            .min(u32::MAX as u64) as u32;
        period.saturating_mul(MISSED_ROUNDS).max(MIN_EXPIRY_SECS)
    }
}

/// One Home Assistant entity: a descriptor plus the node-level naming from its
/// [`Slot`].
pub struct Entity {
    pub slot: Slot,
    pub desc: &'static EntityDescriptor,
}

/// Every entity this node exposes, in publish order.
pub fn entities(node: &NodeConfig) -> Vec<Entity, MAX_ENTITIES> {
    let mut out = Vec::new();
    for (slot, descriptors) in [
        (node.scale, scale::DESCRIPTORS),
        (node.ds18b20, ds18b20::DESCRIPTORS),
        (node.sht31, sht31::DESCRIPTORS),
        (node.scd41, scd41::DESCRIPTORS),
        (node.sds011, sds011::descriptors(node.sds011.compensated)),
        (node.sgp41, sgp41::descriptors(node.sgp41.nox)),
        (node.battery, battery::DESCRIPTORS),
        // Not behind a `NodeConfig` field, unlike every sensor above. A node
        // that cannot reach Wi-Fi cannot publish at all, so there is no
        // configuration to express: the slot is on for everyone, and `Slot::on`
        // means "every round", which is exactly the cadence a link property
        // has. Its `expire_after` therefore follows the node's base round.
        (Slot::on(), rssi::DESCRIPTORS),
        // Same reasoning as the RSSI above: not a sensor, a property of the
        // board, and every node has one. Diagnostics rather than readings, but
        // the cost of carrying them is two small numbers per publish.
        (Slot::on(), reset_reason::DESCRIPTORS),
    ] {
        if !slot.enabled {
            continue;
        }
        for desc in descriptors {
            let _ = out.push(Entity { slot, desc });
        }
    }
    out
}

// FNV-1a, 32-bit. Chosen because it is eight lines and needs no state: this
// runs on a chip whose whole job here is to notice that something changed.
const FNV_OFFSET: u32 = 0x811c_9dc5;
const FNV_PRIME: u32 = 0x0100_0193;

fn fnv_str(mut hash: u32, s: &str) -> u32 {
    for byte in s.as_bytes() {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// A digest of every discovery message this node would publish right now —
/// every topic and every payload, entities and controls alike.
///
/// Stored in RTC RAM after a successful announce (`state::discovery_tag`, which
/// is HAL-gated) so the next connect can tell "already done" from "something
/// changed". It
/// covers the payloads and not just the topic list on purpose: `expire_after`
/// is baked into them, so a changed publish cadence re-announces by itself
/// rather than needing a special case at the call site.
///
/// Never returns 0, which is reserved for "nothing announced yet".
pub fn announcement_tag(node: &NodeConfig, avail: &Availability) -> u32 {
    let mut hash = FNV_OFFSET;
    for entity in entities(node) {
        hash = fnv_str(hash, config_topic(node, &entity).as_str());
        if let Some(payload) = config_payload(node, &entity, avail) {
            hash = fnv_str(hash, payload.as_str());
        }
    }
    for control in controls(node) {
        hash = fnv_str(hash, control_topic(node, control).as_str());
        if let Some(payload) = control_payload(node, control, avail) {
            hash = fnv_str(hash, payload.as_str());
        }
    }
    if hash == 0 {
        1
    } else {
        hash
    }
}

/// `homeassistant/sensor/<node>/<prefix><key>/config`.
pub fn config_topic(node: &NodeConfig, entity: &Entity) -> String<96> {
    let mut t = String::new();
    let _ = write!(
        t,
        "{}/sensor/{}/{}{}/config",
        PREFIX,
        node.id,
        entity.slot.prefix_for(entity.desc.key),
        entity.desc.key
    );
    t
}

/// A `"key":"value",` pair that disappears entirely when the value is empty.
///
/// Written as a `Display` rather than built into a string so it costs no buffer
/// of its own on a node that is already counting bytes.
struct OptionalMember(&'static str, &'static str);

impl core::fmt::Display for OptionalMember {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.1.is_empty() {
            Ok(())
        } else {
            write!(f, "\"{}\":\"{}\",", self.0, self.1)
        }
    }
}

/// The retained discovery payload for one entity, or `None` if it would not fit
/// the buffer. Truncated JSON would be worse than no entity at all: Home
/// Assistant would keep re-reading a broken retained config on every restart.
pub fn config_payload(node: &NodeConfig, entity: &Entity, avail: &Availability) -> Option<Payload> {
    let (slot, desc) = (entity.slot, entity.desc);
    let mut p = String::new();
    write!(
        p,
        "{{\"~\":\"{ns}/{id}\",\
         \"name\":\"{label}{sep}{name}\",\
         \"uniq_id\":\"{id}_{prefix}{key}\",\
         \"stat_t\":\"~/{prefix}{key}\",\
         {unit}{dev_cla}\
         \"stat_cla\":\"{stat_cla}\",\
         \"exp_aft\":{expire},{avty}",
        expire = avail.expire_for(slot),
        // The availability topic is relative to `~`, and only exists on a node
        // whose last-will keeps it honest.
        avty = if avail.lwt {
            "\"avty_t\":\"~/status\","
        } else {
            ""
        },
        ns = node.namespace,
        id = node.id,
        label = slot.label_for(desc.key),
        // A slot label like "Luft" prefixes the entity name; an empty label must
        // not leave a leading space behind.
        sep = if slot.label_for(desc.key).is_empty() {
            ""
        } else {
            " "
        },
        name = desc.name,
        prefix = slot.prefix_for(desc.key),
        key = desc.key,
        // Both are omitted when empty rather than sent as `""`. A count has
        // neither a unit nor a device class, and Home Assistant validates
        // `dev_cla` against its own list: an empty string is not in it, so the
        // entity would be rejected outright instead of rendering as a plain
        // number. An absent key is the way to say "none".
        unit = OptionalMember("unit_of_meas", desc.unit),
        dev_cla = OptionalMember("dev_cla", desc.device_class),
        stat_cla = desc.state_class,
    )
    .ok()?;
    write_device(&mut p, node).ok()?;
    Some(p)
}

/// The device block every entity of this node carries, plus the payload's
/// closing brace. Identical across entities — that sameness is exactly what
/// makes Home Assistant group them all under one device card — so it is written
/// in one place rather than repeated in each format string.
fn write_device(p: &mut Payload, node: &NodeConfig) -> core::fmt::Result {
    write!(
        p,
        "\"dev\":{{\"ids\":[\"{}\"],\"name\":\"{}\",\"mf\":\"{}\",\"mdl\":\"{}\"}}}}",
        node.id, node.name, MANUFACTURER, MODEL
    )
}

// --- Command entities --------------------------------------------------------

/// A knob that flows the *other* way: Home Assistant publishes to
/// `<namespace>/<node>/config/<key>` and the firmware picks it up the next time
/// it is online (see [`crate::config::Config::apply`]).
///
/// These used to be hand-declared in `home-assistant/configuration.yaml`, which
/// meant every node in the fleet needed its own copy-pasted block. Discovering
/// them keeps the whole Home Assistant side of a node generated.
///
/// All command topics are published **retained**: a battery node is asleep when
/// the slider moves, so the broker has to hold the value until it next connects.
pub struct Control {
    /// Home Assistant component — `number`, `switch` or `button`.
    pub component: &'static str,
    /// Config-topic suffix, i.e. the key [`crate::config::Config::apply`]
    /// matches on.
    pub key: &'static str,
    /// Entity name. Home Assistant prefixes the device name, so this is just the
    /// knob ("Auslöseschwelle", not "Meisenknödel Auslöseschwelle").
    pub name: &'static str,
    /// Whether the entity reads its state back off its own command topic.
    ///
    /// The node never echoes its stored config, so without this the sliders
    /// would sit at "unknown" until someone moved them. Since the commands are
    /// retained, the command topic already *is* the last value set — pointing
    /// `stat_t` at it makes the controls show that value again after a Home
    /// Assistant restart. A button has no state, so it opts out.
    pub reads_back: bool,
    /// Component-specific JSON members, each with a trailing comma. Pre-rendered
    /// because they are pure constants: building `"step":0.1` at runtime would
    /// pull in float formatting for no gain, and this way the ranges read like
    /// the YAML they replace.
    pub spec: &'static str,
}

/// The one knob every node has, because the digest can be wrong and nothing
/// else can tell it so.
///
/// [`announcement_tag`] records what this board believes the broker holds, and
/// it is stored on the strength of a publish having been *sent*. If the broker
/// never received one -- or a retained config was cleared by hand afterwards --
/// the node has no way to notice, and no amount of power-cycling helps: RTC RAM
/// on this board has twice now survived several seconds without power.
///
/// That is exactly what happened to the outdoor node: ten of fourteen
/// announcements on the broker, the digest recording all fourteen, and the
/// calibration controls therefore unreachable with no path back short of
/// changing the entity set in the firmware. Hence a button.
const REANNOUNCE_CONTROLS: &[Control] = &[Control {
    component: "button",
    key: "reannounce",
    name: "Discovery neu ankündigen",
    reads_back: false,
    spec: "\"pl_prs\":\"reannounce\",",
}];

/// Knobs that only exist on a node carrying a load cell.
const SCALE_CONTROLS: &[Control] = &[
    Control {
        component: "number",
        key: "threshold",
        name: "Auslöseschwelle",
        reads_back: true,
        spec: "\"min\":0,\"max\":500,\"step\":1,\"unit_of_meas\":\"g\",\"mode\":\"box\",",
    },
    Control {
        component: "number",
        key: "scale_factor",
        name: "Kalibrierfaktor",
        reads_back: true,
        spec: "\"min\":1,\"max\":100000,\"step\":0.1,\"mode\":\"box\",",
    },
    Control {
        component: "number",
        key: "offset",
        name: "Tara-Offset",
        reads_back: true,
        spec: "\"min\":-8388608,\"max\":8388607,\"step\":1,\"mode\":\"box\",",
    },
    // A button's payload is a constant, so the firmware cannot use it to tell
    // one press from the next; instead it *consumes* the retained message after
    // taring (see `main`). Any non-empty payload means "tare now".
    Control {
        component: "button",
        key: "tare",
        name: "Tarieren",
        reads_back: false,
        spec: "\"pl_prs\":\"tare\",",
    },
    // The visit counter lives in RTC RAM, which is exactly the memory that
    // cannot be cleared from a distance: a reflash keeps it by design, and this
    // board has twice been seen holding RTC RAM through several seconds without
    // power (see `REANNOUNCE_CONTROLS`), so "unplug it" is not a reset either.
    // Without a button the only reliable way back to zero would be a firmware
    // change, which is a poor answer to "start counting again from today".
    Control {
        component: "button",
        key: "reset_visits",
        name: "Zähler zurücksetzen",
        reads_back: false,
        spec: "\"pl_prs\":\"reset\",",
    },
];

/// Knobs that only exist on a node carrying an SCD41.
///
/// The offset is a *calibration*, read off a comparison against a trusted
/// hygrometer rather than computed, so it belongs on the device card next to the
/// readings it corrects — not in a rebuild. Sensirion's own procedure is
/// `new = old + (scd41_reading - reference_reading)`, which is why the range
/// allows more than the 4 °C default: a badly ventilated enclosure needs more,
/// an open breakout in moving air needs much less.
const SCD41_CONTROLS: &[Control] = &[Control {
    component: "number",
    key: "scd41_temp_offset",
    name: "Temperatur-Offset",
    reads_back: true,
    spec: "\"min\":0,\"max\":20,\"step\":0.05,\"unit_of_meas\":\"°C\",\"mode\":\"box\",",
}];

/// Knobs that only exist on a node whose SDS011 corrects for humidity.
///
/// κ is a property of the aerosol in the room, not of the sensor, so the only
/// way to arrive at the right value is to compare corrected against raw over a
/// few humid days and move the slider. That is the same argument as the SCD41's
/// offset, and it lands in the same place on the device card. `0` disables the
/// correction without touching the firmware.
const SDS011_CONTROLS: &[Control] = &[Control {
    component: "number",
    key: "sds011_kappa",
    name: "Feuchte-Korrektur κ",
    reads_back: true,
    spec: "\"min\":0,\"max\":1,\"step\":0.01,\"mode\":\"box\",",
}];

/// Knobs that only do anything on a battery node — the cadences it spends its
/// cell at. Any node on a cable samples on its build-time cadence, whether it
/// sleeps between rounds or not, so exposing these would put three dead
/// controls on its device card. The `deep_sleep` switch is deliberately not
/// among them; see [`SLEEP_CONTROLS`].
const BATTERY_CONTROLS: &[Control] = &[
    Control {
        component: "number",
        key: "idle_interval",
        name: "Idle-Intervall",
        reads_back: true,
        spec: "\"min\":1,\"max\":3600,\"step\":1,\"unit_of_meas\":\"s\",\"mode\":\"box\",",
    },
    Control {
        component: "number",
        key: "active_interval",
        name: "Aktiv-Intervall",
        reads_back: true,
        spec: "\"min\":1,\"max\":3600,\"step\":1,\"unit_of_meas\":\"s\",\"mode\":\"box\",",
    },
    Control {
        component: "number",
        key: "heartbeat_interval",
        name: "Heartbeat-Intervall",
        reads_back: true,
        spec: "\"min\":10,\"max\":86400,\"step\":10,\"unit_of_meas\":\"s\",\"mode\":\"box\",",
    },
];

/// The knob every sleeping node gets, whatever it is powered from.
///
/// Kept apart from [`BATTERY_CONTROLS`] because the three intervals above are
/// about spending a cell, while this one is about being reachable: turning it
/// off is how a node is held awake on the bench, where deep sleep just churns
/// the serial monitor. A duty-cycled mains node needs that escape hatch exactly
/// as much as a battery one, and would otherwise need a reflash to get it.
const SLEEP_CONTROLS: &[Control] = &[Control {
    component: "switch",
    key: "deep_sleep",
    name: "Deep Sleep",
    reads_back: true,
    spec: "\"pl_on\":\"1\",\"pl_off\":\"0\",",
}];

/// Every command entity this node exposes.
pub fn controls(node: &NodeConfig) -> Vec<&'static Control, MAX_CONTROLS> {
    let mut out = Vec::new();
    for control in REANNOUNCE_CONTROLS
        .iter()
        .chain(SCALE_CONTROLS.iter().filter(|_| node.scale.enabled))
        .chain(SCD41_CONTROLS.iter().filter(|_| node.scd41.enabled))
        .chain(SDS011_CONTROLS.iter().filter(|_| node.sds011.compensated))
        .chain(BATTERY_CONTROLS.iter().filter(|_| node.power.is_battery()))
        .chain(SLEEP_CONTROLS.iter().filter(|_| node.power.deep_sleeps()))
    {
        let _ = out.push(control);
    }
    out
}

/// `homeassistant/<component>/<node>/<key>/config`.
pub fn control_topic(node: &NodeConfig, control: &Control) -> String<96> {
    let mut t = String::new();
    let _ = write!(
        t,
        "{}/{}/{}/{}/config",
        PREFIX, control.component, node.id, control.key
    );
    t
}

/// The retained discovery payload for one command entity, or `None` if it would
/// not fit (same reasoning as [`config_payload`]).
///
/// Command entities carry no `exp_aft` — they have no state to expire — but they
/// do follow the node's availability, so the controls grey out along with the
/// readings when a mains node drops off.
pub fn control_payload(
    node: &NodeConfig,
    control: &Control,
    avail: &Availability,
) -> Option<Payload> {
    let mut p = String::new();
    write!(
        p,
        "{{\"~\":\"{ns}/{id}\",\
         \"name\":\"{name}\",\
         \"uniq_id\":\"{id}_{key}\",\
         \"cmd_t\":\"~/config/{key}\",",
        ns = node.namespace,
        id = node.id,
        name = control.name,
        key = control.key,
    )
    .ok()?;
    // Written separately rather than as one format string, because the state
    // topic is conditional *and* interpolated.
    if control.reads_back {
        write!(p, "\"stat_t\":\"~/config/{}\",", control.key).ok()?;
    }
    write!(
        p,
        "\"ret\":true,\"ent_cat\":\"config\",{spec}{avty}",
        spec = control.spec,
        avty = if avail.lwt {
            "\"avty_t\":\"~/status\","
        } else {
            ""
        },
    )
    .ok()?;
    write_device(&mut p, node).ok()?;
    Some(p)
}

const _: () = {
    // The worst case is one node carrying every group at once; `controls`
    // returns a fixed-capacity Vec, so overflowing this would silently drop the
    // last entities rather than fail to build. No real node carries all of
    // them — but the bound has to hold for the table someone writes next, not
    // for the one in the file today, and that is cheaper to keep true than to
    // keep reasoning about. `REANNOUNCE_CONTROLS` is in the sum because every
    // node has it; it was missing while the headroom made that harmless.
    assert!(
        REANNOUNCE_CONTROLS.len()
            + SCALE_CONTROLS.len()
            + SCD41_CONTROLS.len()
            + SDS011_CONTROLS.len()
            + BATTERY_CONTROLS.len()
            + SLEEP_CONTROLS.len()
            <= MAX_CONTROLS
    );
};

#[cfg(test)]
mod tests {
    // Deliberately not a glob import: `super::String` / `super::Vec` are the
    // heapless ones, and the tests want the std types.
    use super::{
        announcement_tag, availability, config_payload, config_topic, control_payload,
        control_topic, controls, entities, Availability, Config, NodeConfig, Slot,
        BATTERY_CONTROLS, MAX_ENTITIES, MIN_EXPIRY_SECS, MISSED_ROUNDS, PREFIX, SCALE_CONTROLS,
        SCD41_CONTROLS, SDS011_CONTROLS, SLEEP_CONTROLS,
    };
    use crate::node::FLEET;
    use serde_json::Value;

    /// Expand Home Assistant's `~` shorthand the way it does, so a topic can be
    /// compared against what the firmware actually publishes to.
    fn expand(topic: &str, node: &NodeConfig) -> String {
        topic.replace('~', &format!("{}/{}", node.namespace, node.id))
    }

    fn parse(payload: &str) -> Value {
        serde_json::from_str(payload).unwrap_or_else(|e| panic!("{e}: {payload}"))
    }

    /// Every discovery message a node sends, as (topic, parsed payload).
    fn announcements(node: &NodeConfig, avail: &Availability) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        for entity in entities(node) {
            let payload = config_payload(node, &entity, avail).expect("payload fits");
            out.push((config_topic(node, &entity).to_string(), parse(&payload)));
        }
        for control in controls(node) {
            let payload = control_payload(node, control, avail).expect("payload fits");
            out.push((control_topic(node, control).to_string(), parse(&payload)));
        }
        out
    }

    /// A node with the load cell, an SHT31 behind a prefix and the battery
    /// divider all switched on.
    ///
    /// For the tests that are about the *schema* rather than about a
    /// particular board. Slots move as hardware is wired up — the outdoor node
    /// currently has every one of them off — so a test that needs a scale, a
    /// labelled slot and a divider at once builds one here instead of
    /// borrowing whichever fleet member happens to carry them today. That used
    /// to be `draussen`, and these tests broke when it was retired.
    fn a_fully_populated_node() -> NodeConfig {
        NodeConfig {
            scale: Slot::on(),
            ds18b20: Slot::off(),
            sht31: Slot::on_as("air_", "Luft"),
            battery: Slot::on_as("battery_", "Batterie"),
            ..crate::node::by_name("terrasse").unwrap()
        }
    }

    fn availability_of(node: &NodeConfig) -> Availability {
        availability(node, &Config::DEFAULT)
    }

    // --- Payloads -----------------------------------------------------------

    #[test]
    fn every_payload_is_valid_json_and_fits() {
        // `config_payload` returns None rather than truncating, so "fits" is
        // what `expect` checks; the parse is what catches a malformed join.
        for (_, node) in FLEET {
            let avail = availability_of(node);
            let messages = announcements(node, &avail);
            assert!(!messages.is_empty());
            for (_, payload) in messages {
                assert!(payload.is_object());
            }
        }
    }

    #[test]
    fn state_topics_match_what_the_firmware_publishes() {
        // The whole point of discovery: if these drift, Home Assistant
        // subscribes to a topic nothing ever publishes to and the entity sits
        // at "unknown" for ever.
        for (_, node) in FLEET {
            let avail = availability_of(node);
            for entity in entities(node) {
                let payload = parse(&config_payload(node, &entity, &avail).unwrap());
                let announced = expand(payload["stat_t"].as_str().unwrap(), node);
                let published =
                    node.state_topic(entity.slot.prefix_for(entity.desc.key), entity.desc.key);
                assert_eq!(announced.as_str(), published.as_str());
            }
        }
    }

    #[test]
    fn command_topics_match_the_subscription_and_the_config_keys() {
        // Likewise for the other direction: the command topic has to fall under
        // the wildcard the node subscribes to, and the key has to be one
        // `Config::apply` actually understands.
        for (_, node) in FLEET {
            let avail = availability_of(node);
            let prefix = node.config_prefix();
            for control in controls(node) {
                let payload = parse(&control_payload(node, control, &avail).unwrap());
                let topic = expand(payload["cmd_t"].as_str().unwrap(), node);
                assert!(
                    topic.starts_with(prefix.as_str()),
                    "{topic} is outside {prefix}"
                );
                assert_eq!(&topic[prefix.len()..], control.key);
                // Two values, because one of them may happen to equal the
                // default and `apply` reports "changed", not "understood".
                //
                // Three controls are not config values at all, and are acted
                // on by `main`'s drain instead: `reannounce` forgets the
                // discovery digest, `reset_visits` zeroes the visit counter --
                // both RTC RAM, nothing for `Config` to store -- and `tare`
                // needs a burst of fresh readings off the load cell, which is a
                // measurement rather than an assignment and needs a bus
                // `Config` cannot reach. Anything else reaching this list
                // without `apply` knowing it would be a control that silently
                // does nothing.
                if matches!(control.key, "reannounce" | "reset_visits" | "tare") {
                    continue;
                }
                let mut probe = Config::DEFAULT;
                assert!(
                    probe.apply(control.key, "1") || probe.apply(control.key, "0"),
                    "{} is not a key Config::apply knows",
                    control.key
                );
            }
        }
    }

    #[test]
    fn topics_and_unique_ids_do_not_collide() {
        // Two entities sharing either one would silently overwrite each other in
        // Home Assistant — exactly what the bedroom's two temperature sources
        // would do without their slot prefix.
        for (name, node) in FLEET {
            let avail = availability_of(node);
            let messages = announcements(node, &avail);
            for (i, (topic_a, a)) in messages.iter().enumerate() {
                for (topic_b, b) in &messages[i + 1..] {
                    assert_ne!(topic_a, topic_b, "{name} repeats a config topic");
                    assert_ne!(a["uniq_id"], b["uniq_id"], "{name} repeats a uniq_id");
                    if let (Some(x), Some(y)) = (a.get("stat_t"), b.get("stat_t")) {
                        assert_ne!(x, y, "{name} repeats a state topic");
                    }
                }
            }
        }
    }

    #[test]
    fn the_bedroom_publishes_the_sht31_as_the_rooms_temperature_and_humidity() {
        // The concrete result of the two-sensor arrangement, spelled out the way
        // Home Assistant sees it: the SHT31-D owns the plain entities, the
        // SCD41's own pair is clearly marked as the sensor's own, and CO₂ keeps
        // the id its history is filed under.
        let node = crate::node::by_name("schlafzimmer").unwrap();
        let avail = availability_of(&node);
        let topics: Vec<String> = announcements(&node, &avail)
            .iter()
            .map(|(t, _)| t.clone())
            .collect();

        for expected in [
            "homeassistant/sensor/schlafzimmer/temperature/config",
            "homeassistant/sensor/schlafzimmer/humidity/config",
            "homeassistant/sensor/schlafzimmer/co2/config",
            "homeassistant/sensor/schlafzimmer/scd41_temperature/config",
            "homeassistant/sensor/schlafzimmer/scd41_humidity/config",
        ] {
            assert!(topics.contains(&expected.to_string()), "missing {expected}");
        }
        assert!(!topics.contains(&"homeassistant/sensor/schlafzimmer/scd41_co2/config".to_string()));

        // And the state topic the firmware publishes to has to agree with the
        // one the discovery payload points at.
        for entity in entities(&node) {
            let published =
                node.state_topic(entity.slot.prefix_for(entity.desc.key), entity.desc.key);
            let payload = parse(&config_payload(&node, &entity, &avail).unwrap());
            assert_eq!(
                expand(payload["stat_t"].as_str().unwrap(), &node),
                published.as_str()
            );
        }
    }

    #[test]
    fn trading_the_probe_for_the_divider_announces_a_voltage_and_no_probe() {
        // D2 carries either a DS18B20 or the battery divider, never both. The
        // concrete result of choosing the divider: one entity under the
        // battery's own name, and nothing left announcing a probe that is not
        // soldered to anything. Prefixing the SHT31 is what frees the plain
        // `temperature` key, so it must not reappear.
        let scale = a_fully_populated_node();
        let avail = availability_of(&scale);
        let topics: Vec<String> = announcements(&scale, &avail)
            .iter()
            .map(|(t, _)| t.clone())
            .collect();

        assert!(
            topics.contains(&"homeassistant/sensor/terrasse/battery_voltage/config".to_string())
        );
        assert!(
            topics.contains(&"homeassistant/sensor/terrasse/air_temperature/config".to_string())
        );
        assert!(!topics.contains(&"homeassistant/sensor/terrasse/temperature/config".to_string()));

        let entity = entities(&scale)
            .into_iter()
            .find(|e| e.desc.key == "voltage")
            .expect("a node with the divider exposes a cell voltage");
        let payload = parse(&config_payload(&scale, &entity, &avail).unwrap());
        assert_eq!(payload["dev_cla"], "voltage");
        assert_eq!(payload["unit_of_meas"], "V");
        assert_eq!(payload["name"], "Batterie Spannung");
        assert_eq!(
            expand(payload["stat_t"].as_str().unwrap(), &scale),
            "smarthome/terrasse/battery_voltage"
        );
    }

    #[test]
    fn every_entity_belongs_to_the_same_device() {
        // The `dev` block is what groups the entities into one device card, so
        // it has to be byte-identical across a node's messages.
        for (_, node) in FLEET {
            let avail = availability_of(node);
            let messages = announcements(node, &avail);
            let device = messages[0].1["dev"].clone();
            assert_eq!(device["ids"], serde_json::json!([node.id]));
            assert_eq!(device["name"], node.name);
            for (_, payload) in &messages {
                assert_eq!(payload["dev"], device);
                assert!(payload["uniq_id"].as_str().unwrap().starts_with(node.id));
            }
        }
    }

    #[test]
    fn discovery_topics_follow_the_home_assistant_layout() {
        for (_, node) in FLEET {
            let avail = availability_of(node);
            for (topic, _) in announcements(node, &avail) {
                let parts: Vec<&str> = topic.split('/').collect();
                assert_eq!(parts[0], PREFIX);
                assert!(matches!(
                    parts[1],
                    "sensor" | "number" | "switch" | "button"
                ));
                assert_eq!(parts[2], node.id);
                assert_eq!(parts[4], "config");
                assert_eq!(parts.len(), 5);
            }
        }
    }

    // --- Availability -------------------------------------------------------

    #[test]
    fn only_readings_expire() {
        // A control has no state to go stale; `exp_aft` on one would just make
        // the slider vanish.
        for (_, node) in FLEET {
            let avail = availability_of(node);
            for entity in entities(node) {
                let payload = parse(&config_payload(node, &entity, &avail).unwrap());
                assert!(payload["exp_aft"].is_number());
            }
            for control in controls(node) {
                let payload = parse(&control_payload(node, control, &avail).unwrap());
                assert!(payload.get("exp_aft").is_none());
            }
        }
    }

    #[test]
    fn the_availability_topic_appears_only_where_there_is_a_will() {
        for (name, node) in FLEET {
            let avail = availability_of(node);
            for (_, payload) in announcements(node, &avail) {
                match payload.get("avty_t") {
                    Some(topic) => {
                        assert!(
                            node.uses_lwt(),
                            "{name} announces availability it never publishes"
                        );
                        let topic = expand(topic.as_str().unwrap(), node);
                        assert_eq!(topic.as_str(), node.availability_topic().as_str());
                    }
                    None => assert!(
                        !node.uses_lwt(),
                        "{name} has a will but no availability topic"
                    ),
                }
            }
        }
    }

    #[test]
    fn expiry_allows_three_missed_rounds_with_a_floor() {
        let mut cfg = Config::DEFAULT;
        cfg.heartbeat_secs = 600;
        for (name, node) in FLEET {
            let avail = availability(node, &cfg);
            let period = if node.power.is_battery() {
                cfg.heartbeat_secs
            } else {
                node.sample_secs as u32
            };
            for entity in entities(node) {
                assert_eq!(
                    avail.expire_for(entity.slot),
                    (period * entity.slot.rounds_between(node.sample_secs) * MISSED_ROUNDS)
                        .max(MIN_EXPIRY_SECS),
                    "{name}/{}",
                    entity.desc.key
                );
                assert!(avail.expire_for(entity.slot) >= MIN_EXPIRY_SECS);
            }
        }
    }

    #[test]
    fn a_compensated_sds011_announces_both_the_corrected_and_the_raw_values() {
        let plain = NodeConfig {
            sht31: Slot::off(),
            sds011: Slot::on(),
            ..crate::node::by_name("kueche").unwrap()
        };
        let corrected = NodeConfig {
            sds011: Slot::on().compensated(),
            sht31: Slot::on(),
            ..crate::node::by_name("kueche").unwrap()
        };
        // Filtered to the sensors under test: every node also announces its
        // link RSSI and its reset diagnostics, which are not what this is about.
        let keys = |n: &NodeConfig| -> Vec<&str> {
            entities(n)
                .iter()
                .map(|e| e.desc.key)
                .filter(|k| !matches!(*k, "rssi" | "reset_reason" | "reset_count"))
                .collect()
        };
        assert_eq!(keys(&plain), ["pm25", "pm10"]);
        assert_eq!(
            keys(&corrected),
            [
                "temperature",
                "humidity",
                "pm25",
                "pm10",
                "pm25_raw",
                "pm10_raw"
            ]
        );
        // And the κ slider appears with the correction, not with the sensor.
        let control_keys =
            |n: &NodeConfig| -> Vec<&str> { controls(n).iter().map(|c| c.key).collect() };
        assert!(!control_keys(&plain).contains(&"sds011_kappa"));
        assert!(control_keys(&corrected).contains(&"sds011_kappa"));
    }

    #[test]
    fn a_slow_slot_is_expired_against_its_own_cadence() {
        // The whole point of the per-slot period: a sensor that publishes every
        // 15 minutes must not be expired against the node's 1-minute round, or
        // it spends 14 of every 15 minutes marked unavailable.
        let node = NodeConfig {
            sample_secs: 60,
            sds011: Slot::on().every(900),
            ..crate::node::by_name("kueche").unwrap()
        };
        let avail = availability(&node, &Config::DEFAULT);
        assert_eq!(avail.expire_for(node.sds011), 900 * MISSED_ROUNDS);
        assert_eq!(
            avail.expire_for(node.sht31),
            MIN_EXPIRY_SECS.max(60 * MISSED_ROUNDS)
        );
    }

    #[test]
    fn a_fast_heartbeat_does_not_expire_below_the_floor() {
        // One lost packet must never blank an entity, however tight the poll.
        let mut cfg = Config::DEFAULT;
        cfg.heartbeat_secs = 1;
        let scale = a_fully_populated_node();
        assert_eq!(
            availability(&scale, &cfg).expire_for(scale.scale),
            MIN_EXPIRY_SECS
        );
    }

    // --- Controls -----------------------------------------------------------

    #[test]
    fn controls_follow_what_the_node_actually_is() {
        for (name, node) in FLEET {
            let keys: Vec<&str> = controls(node).iter().map(|c| c.key).collect();
            for control in SCALE_CONTROLS {
                assert_eq!(
                    keys.contains(&control.key),
                    node.scale.enabled,
                    "{name}: {}",
                    control.key
                );
            }
            for control in SCD41_CONTROLS {
                assert_eq!(
                    keys.contains(&control.key),
                    node.scd41.enabled,
                    "{name}: {}",
                    control.key
                );
            }
            for control in SDS011_CONTROLS {
                // Gated on `compensated`, not on the SDS011 merely being
                // present: κ tunes a correction, so on an uncorrected node the
                // slider would be a knob wired to nothing.
                assert_eq!(
                    keys.contains(&control.key),
                    node.sds011.compensated,
                    "{name}: {}",
                    control.key
                );
            }
            for control in BATTERY_CONTROLS {
                assert_eq!(
                    keys.contains(&control.key),
                    node.power.is_battery(),
                    "{name}: {}",
                    control.key
                );
            }
            // Not `is_battery`: the switch follows sleeping, so `kueche` and
            // `bad` get it while running off a cable.
            for control in SLEEP_CONTROLS {
                assert_eq!(
                    keys.contains(&control.key),
                    node.power.deep_sleeps(),
                    "{name}: {}",
                    control.key
                );
            }
        }
    }

    #[test]
    fn a_node_with_nothing_to_tune_has_only_the_re_announce_button() {
        // Better a near-empty Configuration section than controls that do
        // nothing. The one thing every node keeps is the re-announce button,
        // because a digest that disagrees with the broker is otherwise
        // unrecoverable — see `REANNOUNCE_CONTROLS`.
        //
        // No node in the fleet is bare any more: `kueche` and `bad` were, until
        // they started duty-cycling and picked up the `deep_sleep` switch with
        // it. The rule is still worth stating for the next one added.
        for (name, node) in FLEET {
            if !node.power.deep_sleeps() && !node.scale.enabled && !node.scd41.enabled {
                let keys: Vec<&str> = controls(node).iter().map(|c| c.key).collect();
                assert_eq!(keys, vec!["reannounce"], "{name} exposes dead controls");
            }
        }
    }

    #[test]
    fn an_scd41_node_gets_the_offset_knob_and_nothing_else() {
        // The regression this guards: the temperature offset is the *only*
        // reason a plain mains sensor node has a Configuration section at all,
        // so it must not drag the battery or load-cell knobs in with it.
        let bedroom = crate::node::by_name("schlafzimmer").unwrap();
        let keys: Vec<&str> = controls(&bedroom).iter().map(|c| c.key).collect();
        assert_eq!(keys, vec!["reannounce", "scd41_temp_offset"]);
    }

    #[test]
    fn each_component_carries_what_its_schema_needs() {
        // A populated node so every component the fleet can emit is exercised
        // — the `button` branch in particular, which only the scale's `tare`
        // reaches.
        let scale = a_fully_populated_node();
        let avail = availability_of(&scale);
        for control in controls(&scale) {
            let payload = parse(&control_payload(&scale, control, &avail).unwrap());
            assert_eq!(payload["ret"], true, "commands must be retained");
            assert_eq!(payload["ent_cat"], "config");
            match control.component {
                "number" => {
                    for key in ["min", "max", "step"] {
                        assert!(payload[key].is_number(), "{} lacks {key}", control.key);
                    }
                    assert!(payload["min"].as_f64() <= payload["max"].as_f64());
                }
                "switch" => {
                    assert!(payload["pl_on"].is_string() && payload["pl_off"].is_string());
                }
                "button" => {
                    assert!(payload["pl_prs"].is_string());
                    // A press has no state to read back.
                    assert!(payload.get("stat_t").is_none());
                }
                other => panic!("unhandled component {other}"),
            }
        }
    }

    #[test]
    fn controls_read_their_state_back_off_their_command_topic() {
        // Why the sliders show the last value set instead of "unknown", and why
        // that survives a Home Assistant restart.
        let scale = a_fully_populated_node();
        let avail = availability_of(&scale);
        for control in controls(&scale) {
            let payload = parse(&control_payload(&scale, control, &avail).unwrap());
            match payload.get("stat_t") {
                Some(state) => assert_eq!(state, &payload["cmd_t"]),
                None => assert_eq!(control.component, "button"),
            }
        }
    }

    #[test]
    fn a_slot_label_never_leaves_a_stray_space() {
        // A prefixed slot labels its entities ("Luft Temperatur"); an unnamed
        // one must not produce " Temperatur". No fleet member carries a label
        // right now, so the populated node is included explicitly — dropping
        // it would leave the labelled branch untested.
        let populated = a_fully_populated_node();
        for node in FLEET.iter().map(|(_, n)| n).chain([&populated]) {
            let avail = availability_of(node);
            for entity in entities(node) {
                let payload = parse(&config_payload(node, &entity, &avail).unwrap());
                let name = payload["name"].as_str().unwrap();
                assert_eq!(name.trim(), name, "{name:?} is padded");
                assert!(!name.contains("  "));
            }
        }
    }

    // --- The gas sensor ----------------------------------------------------

    #[test]
    fn the_gas_sensor_announces_nox_only_where_it_is_declared() {
        let voc_only = NodeConfig {
            sgp41: Slot::on(),
            ..crate::node::by_name("wohnzimmer").unwrap()
        };
        let with_nox = NodeConfig {
            sgp41: Slot::on().with_nox(),
            ..voc_only
        };
        let keys = |n: &NodeConfig| -> Vec<&'static str> {
            entities(n).iter().map(|e| e.desc.key).collect()
        };
        assert!(keys(&voc_only).contains(&"voc_index"));
        assert!(
            !keys(&voc_only).contains(&"nox_index"),
            "an SGP40 must not be given a channel it does not have"
        );
        assert!(keys(&with_nox).contains(&"nox_index"));
        // And correcting a wrong declaration re-announces by itself.
        assert_ne!(
            announcement_tag(&voc_only, &availability_of(&voc_only)),
            announcement_tag(&with_nox, &availability_of(&with_nox)),
        );
    }

    /// `entities` pushes into a fixed-capacity Vec and **discards** the push
    /// error, so an overflow silently drops whatever comes last in slot order —
    /// the battery divider. Nothing would log it and no test would catch it
    /// except this one. The living room with the gas sensor on is 14 of 16.
    #[test]
    fn no_node_overflows_the_entity_vec() {
        for (_, node) in FLEET {
            let with_gas = NodeConfig {
                sgp41: Slot::on().with_nox(),
                ..*node
            };
            let used = entities(&with_gas).len();
            assert!(
                used < MAX_ENTITIES,
                "{} would use {} of {} entity slots",
                node.id,
                used,
                MAX_ENTITIES
            );
        }
    }

    #[test]
    fn a_count_omits_the_unit_and_device_class_instead_of_sending_empties() {
        let node = crate::node::by_name("terrasse").unwrap();
        let avail = availability_of(&node);
        let all = entities(&node);
        let entity = all
            .iter()
            .find(|e| e.desc.key == "visits")
            .expect("the visit counter is announced");
        let payload = config_payload(&node, entity, &avail).expect("fits");

        // `"dev_cla":""` is not a valid device class, and Home Assistant drops
        // the whole entity over it rather than falling back to none.
        assert!(!payload.contains("dev_cla"), "{payload}");
        assert!(!payload.contains("unit_of_meas"), "{payload}");
        assert!(
            payload.contains("\"stat_cla\":\"total_increasing\""),
            "{payload}"
        );

        // The omission must not have eaten a separator on the way out.
        let parsed = parse(&payload);
        assert_eq!(parsed["stat_t"], "~/visits");
        assert_eq!(parsed["name"], "Vögel gesamt");
    }

    #[test]
    fn a_battery_node_announces_both_the_voltage_and_the_level() {
        let node = crate::node::by_name("terrasse").unwrap();
        let keys: Vec<&str> = entities(&node)
            .iter()
            .filter(|e| e.slot.prefix == "battery_")
            .map(|e| e.desc.key)
            .collect();
        assert_eq!(keys, ["voltage", "percent"]);
        // The percentage is the estimate; the measurement has to stay beside
        // it, or the only number left would be the one that was guessed.
        let avail = availability_of(&node);
        let topics: Vec<String> = entities(&node)
            .iter()
            .map(|e| config_topic(&node, e).to_string())
            .collect();
        assert!(
            topics.contains(&"homeassistant/sensor/terrasse/battery_voltage/config".to_string())
        );
        assert!(
            topics.contains(&"homeassistant/sensor/terrasse/battery_percent/config".to_string())
        );
        for entity in entities(&node) {
            assert!(config_payload(&node, &entity, &avail).is_some());
        }
    }

    // --- The announcement digest -------------------------------------------

    /// The exact fault this replaced a boolean for. `wohnzimmer` announced five
    /// entities while its SDS011 slot was off, then had the slot switched on
    /// and was reflashed. The old "have I announced yet" bit still said yes, so
    /// `pm25`, `pm10` and their raw pair published to the broker for six days
    /// with nothing in Home Assistant subscribed to them. A digest cannot say
    /// yes to a question about a different entity set.
    #[test]
    fn switching_a_sensor_on_changes_the_digest() {
        let before = NodeConfig {
            sds011: Slot::off(),
            ..crate::node::by_name("wohnzimmer").unwrap()
        };
        let after = NodeConfig {
            sds011: Slot::on().compensated().every(900),
            ..before
        };
        assert_ne!(
            announcement_tag(&before, &availability_of(&before)),
            announcement_tag(&after, &availability_of(&after)),
        );
    }

    #[test]
    fn an_unchanged_node_keeps_its_digest() {
        // Otherwise every connect would re-announce, which is the airtime the
        // stored tag exists to avoid.
        for (_, node) in FLEET {
            let avail = availability_of(node);
            assert_eq!(
                announcement_tag(node, &avail),
                announcement_tag(node, &avail),
                "{} is not stable",
                node.id
            );
        }
    }

    /// `expire_after` is baked into the payloads, so this is what lets the
    /// cadence change re-announce without a special case beside the flash write.
    #[test]
    fn a_changed_cadence_changes_the_digest() {
        let node = crate::node::by_name("terrasse").unwrap();
        let slow = Config {
            heartbeat_secs: 600,
            ..Config::DEFAULT
        };
        let fast = Config {
            heartbeat_secs: 60,
            ..Config::DEFAULT
        };
        assert_ne!(
            announcement_tag(&node, &availability(&node, &slow)),
            announcement_tag(&node, &availability(&node, &fast)),
        );
    }

    #[test]
    fn the_digest_is_never_the_nothing_announced_sentinel() {
        // Zero means "RTC RAM holds nothing"; a real node must never collide
        // with it, or its first announce would be skipped.
        for (_, node) in FLEET {
            assert_ne!(
                announcement_tag(node, &availability_of(node)),
                0,
                "{}",
                node.id
            );
        }
    }

    /// Two different nodes must not share a tag, or moving a board between
    /// identities would keep the previous one's entities.
    #[test]
    fn different_nodes_have_different_digests() {
        let mut seen: Vec<u32> = Vec::new();
        for (_, node) in FLEET {
            let tag = announcement_tag(node, &availability_of(node));
            assert!(!seen.contains(&tag), "{} collides", node.id);
            seen.push(tag);
        }
    }
}

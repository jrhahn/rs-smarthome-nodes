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
//! Payloads use Home Assistant's abbreviated keys (`stat_t`, `dev_cla`, …) and
//! the `~` base-topic shorthand, mostly to keep them inside the small MQTT
//! buffers a no_std node can afford.

use core::fmt::Write as _;

use heapless::{String, Vec};

use crate::config::Config;
use crate::ds18b20;
use crate::node::{self, Slot};
use crate::sensors::{scale, scd41, sds011, sht31, EntityDescriptor};

/// Upper bound on entities a node can expose (weight, probe temperature,
/// SHT31 ×2, SCD41 ×3, SDS011 ×2), with headroom.
pub const MAX_ENTITIES: usize = 12;
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
    pub expire_after: u32,
}

/// The availability policy for this node, given the live config (a battery node
/// publishes on its heartbeat, a mains node on its fixed cadence).
pub fn availability(cfg: &Config) -> Availability {
    let node = node::active();
    let period = if node.power.is_battery() {
        cfg.heartbeat_secs
    } else {
        // Clamped to u32 seconds; no node samples anywhere near that rarely.
        node.sample_secs.min(u32::MAX as u64) as u32
    };
    Availability {
        lwt: node.uses_lwt(),
        expire_after: period.saturating_mul(MISSED_ROUNDS).max(MIN_EXPIRY_SECS),
    }
}

/// One Home Assistant entity: a descriptor plus the node-level naming from its
/// [`Slot`].
pub struct Entity {
    pub slot: Slot,
    pub desc: &'static EntityDescriptor,
}

/// Every entity this node exposes, in publish order.
pub fn entities() -> Vec<Entity, MAX_ENTITIES> {
    let node = node::active();
    let mut out = Vec::new();
    for (slot, descriptors) in [
        (node.scale, scale::DESCRIPTORS),
        (node.ds18b20, ds18b20::DESCRIPTORS),
        (node.sht31, sht31::DESCRIPTORS),
        (node.scd41, scd41::DESCRIPTORS),
        (node.sds011, sds011::DESCRIPTORS),
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

/// `homeassistant/sensor/<node>/<prefix><key>/config`.
pub fn config_topic(entity: &Entity) -> String<96> {
    let node = node::active();
    let mut t = String::new();
    let _ = write!(
        t,
        "{}/sensor/{}/{}{}/config",
        PREFIX, node.id, entity.slot.prefix, entity.desc.key
    );
    t
}

/// The retained discovery payload for one entity, or `None` if it would not fit
/// the buffer. Truncated JSON would be worse than no entity at all: Home
/// Assistant would keep re-reading a broken retained config on every restart.
pub fn config_payload(entity: &Entity, avail: &Availability) -> Option<Payload> {
    let node = node::active();
    let (slot, desc) = (entity.slot, entity.desc);
    let mut p = String::new();
    write!(
        p,
        "{{\"~\":\"{ns}/{id}\",\
         \"name\":\"{label}{sep}{name}\",\
         \"uniq_id\":\"{id}_{prefix}{key}\",\
         \"stat_t\":\"~/{prefix}{key}\",\
         \"unit_of_meas\":\"{unit}\",\
         \"dev_cla\":\"{dev_cla}\",\
         \"stat_cla\":\"{stat_cla}\",\
         \"exp_aft\":{expire},{avty}",
        expire = avail.expire_after,
        // The availability topic is relative to `~`, and only exists on a node
        // whose last-will keeps it honest.
        avty = if avail.lwt {
            "\"avty_t\":\"~/status\","
        } else {
            ""
        },
        ns = node.namespace,
        id = node.id,
        label = slot.label,
        // A slot label like "Luft" prefixes the entity name; an empty label must
        // not leave a leading space behind.
        sep = if slot.label.is_empty() { "" } else { " " },
        name = desc.name,
        prefix = slot.prefix,
        key = desc.key,
        unit = desc.unit,
        dev_cla = desc.device_class,
        stat_cla = desc.state_class,
    )
    .ok()?;
    write_device(&mut p).ok()?;
    Some(p)
}

/// The device block every entity of this node carries, plus the payload's
/// closing brace. Identical across entities — that sameness is exactly what
/// makes Home Assistant group them all under one device card — so it is written
/// in one place rather than repeated in each format string.
fn write_device(p: &mut Payload) -> core::fmt::Result {
    let node = node::active();
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
];

/// Knobs that only do anything on a battery node. A mains node samples on its
/// build-time cadence and never sleeps, so exposing these would put four dead
/// controls on its device card.
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
    Control {
        component: "switch",
        key: "deep_sleep",
        name: "Deep Sleep",
        reads_back: true,
        spec: "\"pl_on\":\"1\",\"pl_off\":\"0\",",
    },
];

/// Every command entity this node exposes.
pub fn controls() -> Vec<&'static Control, MAX_CONTROLS> {
    let node = node::active();
    let mut out = Vec::new();
    for control in SCALE_CONTROLS
        .iter()
        .filter(|_| node.scale.enabled)
        .chain(BATTERY_CONTROLS.iter().filter(|_| node.power.is_battery()))
    {
        let _ = out.push(control);
    }
    out
}

/// `homeassistant/<component>/<node>/<key>/config`.
pub fn control_topic(control: &Control) -> String<96> {
    let node = node::active();
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
pub fn control_payload(control: &Control, avail: &Availability) -> Option<Payload> {
    let node = node::active();
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
    write_device(&mut p).ok()?;
    Some(p)
}

const _: () = {
    assert!(SCALE_CONTROLS.len() + BATTERY_CONTROLS.len() <= MAX_CONTROLS);
};

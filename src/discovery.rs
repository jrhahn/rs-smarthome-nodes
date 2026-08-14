//! Home Assistant MQTT auto-discovery (#16).
//!
//! On every connect the firmware publishes one **retained** config message per
//! reading to `homeassistant/sensor/<node>/<key>/config`. Home Assistant creates
//! the entities from those, groups them under a single device (identified by the
//! node id) and remembers them across restarts — so adding a node to the fleet
//! no longer means hand-declaring entities in the home-server nix config.
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
pub fn config_payload(entity: &Entity, avail: &Availability) -> Option<String<384>> {
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
         \"exp_aft\":{expire},{avty}\
         \"dev\":{{\"ids\":[\"{id}\"],\"name\":\"{dev_name}\",\"mf\":\"{mf}\",\"mdl\":\"{mdl}\"}}}}",
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
        dev_name = node.name,
        mf = MANUFACTURER,
        mdl = MODEL,
    )
    .ok()?;
    Some(p)
}

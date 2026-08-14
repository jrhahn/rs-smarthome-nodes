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

use crate::ds18b20;
use crate::node::{Slot, NODE};
use crate::sensors::{scale, scd41, sds011, sht31, EntityDescriptor};

/// Upper bound on entities a node can expose (weight, probe temperature,
/// SHT31 ×2, SCD41 ×3, SDS011 ×2), with headroom.
pub const MAX_ENTITIES: usize = 12;

/// Discovery prefix Home Assistant listens on (its default).
pub const PREFIX: &str = "homeassistant";
/// Manufacturer/model reported for the device card in Home Assistant.
const MANUFACTURER: &str = "rs-smarthome-nodes";
const MODEL: &str = "ESP32-C3";

/// One Home Assistant entity: a descriptor plus the node-level naming from its
/// [`Slot`].
pub struct Entity {
    pub slot: Slot,
    pub desc: &'static EntityDescriptor,
}

/// Every entity this node exposes, in publish order.
pub fn entities() -> Vec<Entity, MAX_ENTITIES> {
    let mut out = Vec::new();
    for (slot, descriptors) in [
        (NODE.scale, scale::DESCRIPTORS),
        (NODE.ds18b20, ds18b20::DESCRIPTORS),
        (NODE.sht31, sht31::DESCRIPTORS),
        (NODE.scd41, scd41::DESCRIPTORS),
        (NODE.sds011, sds011::DESCRIPTORS),
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
    let mut t = String::new();
    let _ = write!(
        t,
        "{}/sensor/{}/{}{}/config",
        PREFIX, NODE.id, entity.slot.prefix, entity.desc.key
    );
    t
}

/// The retained discovery payload for one entity, or `None` if it would not fit
/// the buffer. Truncated JSON would be worse than no entity at all: Home
/// Assistant would keep re-reading a broken retained config on every restart.
pub fn config_payload(entity: &Entity) -> Option<String<384>> {
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
         \"dev\":{{\"ids\":[\"{id}\"],\"name\":\"{dev_name}\",\"mf\":\"{mf}\",\"mdl\":\"{mdl}\"}}}}",
        ns = NODE.namespace,
        id = NODE.id,
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
        dev_name = NODE.name,
        mf = MANUFACTURER,
        mdl = MODEL,
    )
    .ok()?;
    Some(p)
}

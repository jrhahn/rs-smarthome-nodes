//! The MQTT side: what the fleet's topics mean, and the task that follows them.
//!
//! The firmware's topic layout (`src/node.rs`, `src/discovery.rs`) is the whole
//! schema this service has:
//!
//! ```text
//! smarthome/<node>/<key>              a reading, `{"v":<number>,"t":<unix_ms>}`
//! smarthome/<node>/status             online / offline (the last will)
//! smarthome/<node>/config/<key>       retained knobs, Home Assistant -> node
//! smarthome/provision/<mac>           retained identity, operator -> node
//! homeassistant/sensor/<node>/<key>/config   retained discovery metadata
//! ```
//!
//! Only the first two are measurements. The `config` topics carry settings
//! rather than observations, and `provision` is an instruction; storing either
//! as a time series would put a slider's position next to a CO2 reading and
//! call them the same kind of thing.
//!
//! The discovery topics are subscribed to but never written: they are where the
//! units and display names come from, so adding a sensor to the firmware gives
//! this dashboard its axis label without a line being changed here.
//!
//! A reading used to be a bare decimal string dated on arrival. It now brings
//! its own timestamp, which is what makes a *retained* reading storable -- and
//! therefore what lets a restart of this service recover the head of every
//! series from the broker instead of losing it. Both shapes are accepted, and
//! will be for as long as there is a node that has not been reflashed; see
//! [`parse_measurement`].

use std::time::Duration;

use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::Settings;
use crate::model::{now_micros, Availability, ChannelMeta, Micros, Reading};
use crate::questdb::Record;
use crate::state::Shared;

/// What a topic turned out to be.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed<'a> {
    Reading {
        node: &'a str,
        sensor: &'a str,
    },
    Status {
        node: &'a str,
    },
    Discovery {
        node: &'a str,
        sensor: &'a str,
    },
    /// Deliberately not ours: a config knob, a provisioning message, a control
    /// entity's discovery, or something else entirely sharing the broker.
    Ignored,
}

/// The node id `smarthome/provision/<mac>` would otherwise look like.
const PROVISION: &str = "provision";

/// The sequence name in `<node>/config/<key>`, which is a knob and not a
/// reading. It is the one four-segment shape that must stay ignored.
const CONFIG: &str = "config";

/// Work out what a topic is, without allocating.
pub fn classify<'a>(namespace: &str, discovery_prefix: &str, topic: &'a str) -> Parsed<'a> {
    let parts: Vec<&str> = topic.split('/').collect();

    if parts.first() == Some(&namespace) {
        // Three segments is this fleet's own shape, `<node>/<key>`.
        //
        // Four is one of two things. `config/<key>` is a knob Home Assistant
        // writes, and stays ignored. Everything else is a meter: the water
        // meters run jomjol's AI-on-the-edge firmware, which puts the name of
        // its number sequence between the node and the field and so arrives as
        // `<node>/main/value`. Those are readings and belong in the table.
        //
        // The sequence name is dropped rather than folded into the sensor id.
        // There is one sequence per device -- a second would be a second dial
        // on the same meter, which neither of these has -- and `value` sits
        // better beside `air_temperature` and `co2` than `main_value` would.
        // Non-numeric siblings like `main/json` cost nothing: they fail to
        // parse downstream and are counted as skipped, which is what that
        // path is for.
        let (node, key) = match parts.len() {
            3 => (parts[1], parts[2]),
            4 if parts[2] != CONFIG => (parts[1], parts[3]),
            _ => return Parsed::Ignored,
        };
        if node.is_empty() || key.is_empty() || node == PROVISION {
            return Parsed::Ignored;
        }
        return if key == "status" {
            Parsed::Status { node }
        } else {
            Parsed::Reading { node, sensor: key }
        };
    }

    if parts.first() == Some(&discovery_prefix) {
        // `homeassistant/sensor/<node>/<key>/config`. The `number`, `switch`
        // and `button` components are the knobs, and have no reading behind
        // them to label.
        if parts.len() == 5 && parts[1] == "sensor" && parts[4] == "config" {
            return Parsed::Discovery {
                node: parts[2],
                sensor: parts[3],
            };
        }
        return Parsed::Ignored;
    }

    Parsed::Ignored
}

/// A bare decimal string, as the firmware wrote it before it had a clock.
///
/// Non-finite values are refused rather than stored. A NaN would be rejected by
/// ILP anyway, and an infinity would poison every rollup bucket it landed in --
/// `min` and `max` would take it, and the mean would never recover.
pub fn parse_value(payload: &[u8]) -> Option<f64> {
    let text = std::str::from_utf8(payload).ok()?.trim();
    let value: f64 = text.parse().ok()?;
    value.is_finite().then_some(value)
}

/// What a reading's payload turned out to say.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    pub value: f64,
    /// When the node says the sensor produced it, if it said. `None` for a node
    /// that has not been reflashed yet, and for one whose time sync failed.
    pub at: Option<Micros>,
}

/// The plausible window for a node-supplied timestamp, in milliseconds:
/// 2025-01-01 to 2100-01-01.
///
/// The same bound the firmware applies before it publishes one (`clock.rs`),
/// repeated here rather than shared because the two are separately deployed:
/// nodes are reflashed one at a time, and this side has to survive whatever a
/// node that has not been reflashed -- or has been flashed with something
/// else -- puts on the topic. A timestamp is a claim, and this is where it stops
/// being taken on trust.
const SANE_FROM_MS: i64 = 1_735_689_600_000;
const SANE_UNTIL_MS: i64 = 4_102_444_800_000;

/// A reading's payload, in either of the two shapes that are live at once.
///
/// The fleet is reflashed node by node, so both forms arrive on the same topics
/// for as long as that takes -- and the older one keeps working for ever, since
/// nothing forces a node to be updated:
///
/// ```text
/// 21.5                          a bare decimal: no clock, dated on arrival
/// {"v":21.5}                    a node whose time sync failed this round
/// {"v":21.5,"t":1789675200123}  taken at that Unix millisecond
/// ```
///
/// An implausible `t` is dropped back to `None` rather than refusing the
/// reading: the value is still good, and a node with a confused clock should
/// cost its readings their precision, not their existence.
pub fn parse_measurement(payload: &[u8]) -> Option<Measurement> {
    // The bare form first: it is what every node sent until this change, it is
    // unambiguous, and it costs one parse rather than a JSON walk.
    if let Some(value) = parse_value(payload) {
        return Some(Measurement { value, at: None });
    }

    let json: serde_json::Value = serde_json::from_slice(payload).ok()?;
    let value = json.get("v").and_then(serde_json::Value::as_f64)?;
    if !value.is_finite() {
        return None;
    }
    let at = json
        .get("t")
        .and_then(serde_json::Value::as_i64)
        .filter(|ms| (SANE_FROM_MS..SANE_UNTIL_MS).contains(ms))
        .map(|ms| ms * 1_000);
    Some(Measurement { value, at })
}

/// The availability payloads, as `discovery::PAYLOAD_ONLINE` / `_OFFLINE`.
pub fn parse_availability(payload: &[u8]) -> Option<Availability> {
    match std::str::from_utf8(payload).ok()?.trim() {
        "online" => Some(Availability::Online),
        "offline" => Some(Availability::Offline),
        _ => None,
    }
}

/// Pull the labels out of a Home Assistant discovery payload.
///
/// Keys are the abbreviated ones the firmware uses to stay inside a no_std
/// node's buffers (`unit_of_meas`, `dev_cla`, ...), with the long spellings
/// accepted too so a hand-written entity on the same broker is not misread as
/// unlabelled.
pub fn parse_discovery(payload: &[u8]) -> Option<ChannelMeta> {
    let json: serde_json::Value = serde_json::from_slice(payload).ok()?;
    let get = |short: &str, long: &str| -> String {
        json.get(short)
            .or_else(|| json.get(long))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    Some(ChannelMeta {
        name: get("name", "name"),
        unit: get("unit_of_meas", "unit_of_measurement"),
        device_class: get("dev_cla", "device_class"),
        node_name: json
            .get("dev")
            .or_else(|| json.get("device"))
            .and_then(|d| d.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

/// Subscribe and keep following the fleet until the process ends.
///
/// rumqttc reconnects on its own; what it does *not* do is re-subscribe, so the
/// subscriptions are issued on every `ConnAck` rather than once at start-up. A
/// broker restart otherwise leaves this connected and permanently silent, which
/// is the failure mode that looks exactly like a quiet house.
pub async fn run(settings: Settings, shared: Shared, tx: mpsc::Sender<Record>) {
    let mut options = MqttOptions::new(
        settings.mqtt.client_id.clone(),
        settings.mqtt.host.clone(),
        settings.mqtt.port,
    );
    options.set_keep_alive(Duration::from_secs(30));
    // Room for the retained discovery burst that arrives on every connect: one
    // message per entity per node, all at once.
    options.set_max_packet_size(64 * 1024, 64 * 1024);
    if !settings.mqtt.user.is_empty() {
        options.set_credentials(settings.mqtt.user.clone(), settings.mqtt.password.clone());
    }

    let (client, mut eventloop) = AsyncClient::new(options, 256);
    let readings_filter = format!("{}/+/+", settings.mqtt.namespace);
    let discovery_filter = format!("{}/sensor/+/+/config", settings.mqtt.discovery_prefix);

    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                info!(
                    broker = %format!("{}:{}", settings.mqtt.host, settings.mqtt.port),
                    "connected to the broker"
                );
                for filter in [&readings_filter, &discovery_filter] {
                    if let Err(e) = client.subscribe(filter, QoS::AtLeastOnce).await {
                        warn!(filter, error = %e, "subscribe failed");
                    } else {
                        info!(filter, "subscribed");
                    }
                }
                shared.set_broker_connected(true);
            }
            Ok(Event::Incoming(Incoming::Publish(p))) => {
                handle(&settings, &shared, &tx, &p.topic, &p.payload, p.retain).await;
            }
            Ok(_) => {}
            Err(e) => {
                shared.set_broker_connected(false);
                warn!(error = %e, "MQTT connection lost; retrying");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn handle(
    settings: &Settings,
    shared: &Shared,
    tx: &mpsc::Sender<Record>,
    topic: &str,
    payload: &[u8],
    retained: bool,
) {
    match classify(
        &settings.mqtt.namespace,
        &settings.mqtt.discovery_prefix,
        topic,
    ) {
        Parsed::Reading { node, sensor } => {
            let Some(measurement) = parse_measurement(payload) else {
                warn!(topic, payload = %String::from_utf8_lossy(payload), "unparseable reading");
                shared.stats().record_skipped();
                return;
            };
            // A retained reading is a value from the past being replayed on
            // connect. Whether that is worth storing turns entirely on whether
            // it can be dated: with a timestamp of its own it is simply a
            // reading that arrived late, and re-ingesting it is a no-op because
            // the table deduplicates on (timestamp, node, sensor). Without one,
            // the only clock available here is "now", which would put a stale
            // number at the head of the series -- so it is still dropped.
            //
            // This is what the retained state topics are *for*: after a restart
            // of this service, the broker replays the last value of every
            // channel, and the head of each series is recovered rather than
            // lost. See `schema::alter_dedup_ddl`.
            let at = match measurement.at {
                Some(at) => at,
                None if retained => {
                    debug!(topic, "ignoring a retained reading that carries no timestamp");
                    shared.stats().record_skipped();
                    return;
                }
                None => now_micros(),
            };
            let reading = Reading {
                node: node.to_string(),
                sensor: sensor.to_string(),
                value: measurement.value,
                at,
            };
            shared.observe(&reading);
            if tx.send(Record::Reading(reading)).await.is_err() {
                warn!("writer is gone; dropping a reading");
            }
        }
        Parsed::Status { node } => {
            let Some(state) = parse_availability(payload) else {
                debug!(topic, "status payload is not online/offline");
                return;
            };
            let online = state == Availability::Online;
            // Retained *is* meaningful here: it is how the broker tells a
            // newly connected listener that a node went offline while nobody
            // was watching. Only the transitions are recorded, so a reconnect
            // does not write a row per restart of this service.
            if shared.set_online(node, online) {
                info!(node, state = state.as_str(), "availability changed");
                let record = Record::Status {
                    node: node.to_string(),
                    online,
                    at: now_micros(),
                };
                if tx.send(record).await.is_err() {
                    warn!("writer is gone; dropping a status change");
                }
            }
        }
        Parsed::Discovery { node, sensor } => {
            if payload.is_empty() {
                // An empty retained payload is how Home Assistant is told an
                // entity is gone.
                shared.forget_meta(node, sensor);
                return;
            }
            match parse_discovery(payload) {
                Some(meta) => shared.set_meta(node, sensor, meta),
                None => debug!(topic, "discovery payload is not JSON"),
            }
        }
        Parsed::Ignored => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify_default(topic: &str) -> Parsed<'_> {
        classify("smarthome", "homeassistant", topic)
    }

    #[test]
    fn a_reading_topic_is_node_and_key() {
        assert_eq!(
            classify_default("smarthome/schlafzimmer/co2"),
            Parsed::Reading {
                node: "schlafzimmer",
                sensor: "co2"
            }
        );
        // The prefixed keys the firmware emits for a second sensor of the same
        // quantity must survive as one identifier, not be split.
        assert_eq!(
            classify_default("smarthome/schlafzimmer/scd41_temperature"),
            Parsed::Reading {
                node: "schlafzimmer",
                sensor: "scd41_temperature"
            }
        );
    }

    #[test]
    fn a_meters_sequence_name_is_dropped() {
        // AI-on-the-edge publishes `<node>/<sequence>/<field>`. The field is
        // the sensor; the sequence name carries nothing this table needs.
        assert_eq!(
            classify_default("smarthome/wasserzaehler_warm/main/value"),
            Parsed::Reading {
                node: "wasserzaehler_warm",
                sensor: "value"
            }
        );
        assert_eq!(
            classify_default("smarthome/wasserzaehler_kalt/main/rate_per_time_unit"),
            Parsed::Reading {
                node: "wasserzaehler_kalt",
                sensor: "rate_per_time_unit"
            }
        );
        // The meter's own three-segment topics keep working unchanged.
        assert_eq!(
            classify_default("smarthome/wasserzaehler_warm/wifiRSSI"),
            Parsed::Reading {
                node: "wasserzaehler_warm",
                sensor: "wifiRSSI"
            }
        );
        // Five segments are still nothing we publish.
        assert_eq!(
            classify_default("smarthome/wasserzaehler_warm/main/value/extra"),
            Parsed::Ignored
        );
    }

    #[test]
    fn the_last_will_is_not_a_reading() {
        assert_eq!(
            classify_default("smarthome/terrasse/status"),
            Parsed::Status { node: "terrasse" }
        );
    }

    #[test]
    fn settings_and_provisioning_are_not_measurements() {
        // Four segments: a knob Home Assistant writes.
        assert_eq!(
            classify_default("smarthome/terrasse/config/scale"),
            Parsed::Ignored
        );
        assert_eq!(
            classify_default("smarthome/terrasse/config/#"),
            Parsed::Ignored
        );
        // Three segments, but `provision` is not a node -- this is the one
        // topic that would otherwise parse as a reading called `<mac>`.
        assert_eq!(
            classify_default("smarthome/provision/a1b2c3d4e5f6"),
            Parsed::Ignored
        );
    }

    #[test]
    fn discovery_is_read_for_sensors_only() {
        assert_eq!(
            classify_default("homeassistant/sensor/bad/humidity/config"),
            Parsed::Discovery {
                node: "bad",
                sensor: "humidity"
            }
        );
        // The knobs discover themselves too, and have no series behind them.
        assert_eq!(
            classify_default("homeassistant/number/terrasse/scale/config"),
            Parsed::Ignored
        );
        assert_eq!(
            classify_default("homeassistant/sensor/bad/humidity/state"),
            Parsed::Ignored
        );
    }

    #[test]
    fn another_applications_topics_are_left_alone() {
        assert_eq!(
            classify_default("zigbee2mqtt/kitchen/light"),
            Parsed::Ignored
        );
        assert_eq!(classify_default(""), Parsed::Ignored);
        assert_eq!(classify_default("smarthome"), Parsed::Ignored);
        assert_eq!(classify_default("smarthome//co2"), Parsed::Ignored);
    }

    #[test]
    fn a_renamed_namespace_is_honoured() {
        assert_eq!(
            classify("haus", "ha", "haus/bad/temperature"),
            Parsed::Reading {
                node: "bad",
                sensor: "temperature"
            }
        );
        assert_eq!(
            classify("haus", "ha", "smarthome/bad/temperature"),
            Parsed::Ignored
        );
    }

    #[test]
    fn values_are_the_decimal_strings_the_firmware_writes() {
        assert_eq!(parse_value(b"412"), Some(412.0));
        assert_eq!(parse_value(b"-17.25"), Some(-17.25));
        assert_eq!(parse_value(b" 55.9 "), Some(55.9));
        assert_eq!(parse_value(b"3.7"), Some(3.7));
    }

    #[test]
    fn a_value_that_is_not_a_number_is_refused() {
        assert_eq!(parse_value(b""), None);
        assert_eq!(parse_value(b"online"), None);
        assert_eq!(parse_value(b"unavailable"), None);
        // Infinity and NaN parse as f64 but cannot be stored or aggregated.
        assert_eq!(parse_value(b"inf"), None);
        assert_eq!(parse_value(b"NaN"), None);
        assert_eq!(parse_value(&[0xff, 0xfe]), None);
    }

    #[test]
    fn a_reading_carries_its_own_timestamp() {
        let m = parse_measurement(br#"{"v":21.5,"t":1789675200123}"#).unwrap();
        assert_eq!(m.value, 21.5);
        // Milliseconds on the wire, microseconds in the database.
        assert_eq!(m.at, Some(1_789_675_200_123_000));
    }

    #[test]
    fn both_payload_shapes_are_live_at_once() {
        // Nodes are reflashed one at a time, so the bare form has to keep
        // working for as long as one node has not been -- which is for ever, in
        // the sense that nothing forces the update.
        let bare = parse_measurement(b"412").unwrap();
        assert_eq!(bare.value, 412.0);
        assert_eq!(bare.at, None, "a bare reading is dated on arrival");

        // And the shape a reflashed node sends when its time sync failed.
        let unstamped = parse_measurement(br#"{"v":412}"#).unwrap();
        assert_eq!(unstamped.value, 412.0);
        assert_eq!(unstamped.at, None);
    }

    #[test]
    fn a_confused_clock_costs_precision_and_not_the_reading() {
        // The value is still good; only the claim about when is refused. Each
        // of these is a real failure mode: an NTP era slip landing in 1900, a
        // zero from a node that never synced, seconds sent where milliseconds
        // are meant, and a garbage register.
        for payload in [
            br#"{"v":21.5,"t":0}"#.as_slice(),
            br#"{"v":21.5,"t":-2208988800000}"#.as_slice(),
            br#"{"v":21.5,"t":1789675200}"#.as_slice(),
            br#"{"v":21.5,"t":999999999999999}"#.as_slice(),
        ] {
            let m = parse_measurement(payload).unwrap();
            assert_eq!(m.value, 21.5, "{}", String::from_utf8_lossy(payload));
            assert_eq!(m.at, None, "{}", String::from_utf8_lossy(payload));
        }
    }

    #[test]
    fn the_plausible_window_is_the_one_the_firmware_applies() {
        // Same bound as `clock::is_plausible` on the node, checked at both
        // edges so the two cannot drift apart unnoticed.
        let at = |ms: i64| {
            parse_measurement(format!(r#"{{"v":1,"t":{ms}}}"#).as_bytes())
                .unwrap()
                .at
        };
        assert_eq!(at(SANE_FROM_MS), Some(SANE_FROM_MS * 1_000));
        assert_eq!(at(SANE_FROM_MS - 1), None);
        assert_eq!(at(SANE_UNTIL_MS), None);
        assert_eq!(at(SANE_UNTIL_MS - 1), Some((SANE_UNTIL_MS - 1) * 1_000));
    }

    #[test]
    fn a_json_payload_without_a_value_is_not_a_reading() {
        assert!(parse_measurement(br#"{"t":1789675200123}"#).is_none());
        assert!(parse_measurement(br#"{"v":"warm"}"#).is_none());
        assert!(parse_measurement(br#"{"v":null}"#).is_none());
        assert!(parse_measurement(b"{").is_none());
        assert!(parse_measurement(b"").is_none());
        // The same non-finite refusal the bare form makes, since an infinity
        // would poison every rollup bucket it landed in. JSON has no literal
        // for one, but a big enough exponent overflows to it.
        assert!(parse_measurement(br#"{"v":1e400}"#).is_none());
    }

    #[test]
    fn availability_is_the_firmwares_two_words() {
        assert_eq!(parse_availability(b"online"), Some(Availability::Online));
        assert_eq!(parse_availability(b"offline"), Some(Availability::Offline));
        assert_eq!(parse_availability(b"ONLINE"), None);
        assert_eq!(parse_availability(b""), None);
    }

    #[test]
    fn discovery_metadata_comes_out_of_the_abbreviated_payload() {
        // Not a byte-string literal: the degree sign is exactly the character
        // this has to get right, and `br"..."` cannot hold it.
        let payload = r#"{"~":"smarthome/schlafzimmer","name":"Luft Temperatur",
            "uniq_id":"schlafzimmer_temperature","stat_t":"~/temperature",
            "unit_of_meas":"°C","dev_cla":"temperature","stat_cla":"measurement",
            "exp_aft":300,"avty_t":"~/status",
            "dev":{"ids":["schlafzimmer"],"name":"Schlafzimmer","mf":"x","mdl":"y"}}"#;
        let meta = parse_discovery(payload.as_bytes()).unwrap();
        assert_eq!(meta.name, "Luft Temperatur");
        assert_eq!(meta.unit, "°C");
        assert_eq!(meta.device_class, "temperature");
        assert_eq!(meta.node_name, "Schlafzimmer");
    }

    #[test]
    fn an_entity_without_a_unit_is_still_metadata() {
        // The visit counter has neither unit nor device class: the firmware
        // omits the keys entirely rather than sending empty strings.
        let payload = br#"{"name":"Besuche","stat_cla":"measurement",
            "dev":{"ids":["terrasse"],"name":"Terrasse"}}"#;
        let meta = parse_discovery(payload).unwrap();
        assert_eq!(meta.name, "Besuche");
        assert_eq!(meta.unit, "");
        assert_eq!(meta.device_class, "");
        assert_eq!(meta.node_name, "Terrasse");
    }

    #[test]
    fn the_long_spellings_are_understood_too() {
        let payload = br#"{"name":"X","unit_of_measurement":"ppm","device_class":"co2",
            "device":{"name":"Anderes"}}"#;
        let meta = parse_discovery(payload).unwrap();
        assert_eq!(meta.unit, "ppm");
        assert_eq!(meta.device_class, "co2");
        assert_eq!(meta.node_name, "Anderes");
    }

    #[test]
    fn a_broken_discovery_payload_is_not_metadata() {
        assert!(parse_discovery(b"{").is_none());
        assert!(parse_discovery(b"").is_none());
    }
}

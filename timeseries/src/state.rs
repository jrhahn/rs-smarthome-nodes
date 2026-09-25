//! What the two halves of the process share: the labels learned from MQTT, the
//! live values, and a few counters for the health endpoint.
//!
//! Everything here is a cache of something that exists elsewhere -- the
//! readings are in QuestDB, the metadata is retained on the broker -- so it is
//! deliberately lossy and never persisted. A restart re-learns all of it: the
//! retained discovery messages arrive within a second of connecting, and the
//! dashboard's tiles come from `LATEST ON` rather than from this map, so an
//! empty cache costs labels for a moment and nothing else.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde::Serialize;

use crate::model::{ChannelMeta, Reading};

/// Counters for `/api/health`. Atomics rather than a lock: they are written
/// from the ingest path on every reading and read once a minute at most.
#[derive(Debug, Default)]
pub struct Stats {
    readings: AtomicU64,
    rows_written: AtomicU64,
    write_errors: AtomicU64,
    rows_dropped: AtomicU64,
    skipped: AtomicU64,
    last_write_ms: AtomicI64,
}

impl Stats {
    pub fn record_reading(&self) {
        self.readings.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_write(&self, rows: u64) {
        self.rows_written.fetch_add(rows, Ordering::Relaxed);
        self.last_write_ms
            .store(crate::model::now_micros() / 1_000, Ordering::Relaxed);
    }

    pub fn record_write_error(&self) {
        self.write_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_dropped(&self, rows: u64) {
        self.rows_dropped.fetch_add(rows, Ordering::Relaxed);
    }

    /// A message that arrived on a topic we own but could not be used: an
    /// unparseable payload, or a retained reading.
    pub fn record_skipped(&self) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        let last = self.last_write_ms.load(Ordering::Relaxed);
        StatsSnapshot {
            readings: self.readings.load(Ordering::Relaxed),
            rows_written: self.rows_written.load(Ordering::Relaxed),
            write_errors: self.write_errors.load(Ordering::Relaxed),
            rows_dropped: self.rows_dropped.load(Ordering::Relaxed),
            skipped: self.skipped.load(Ordering::Relaxed),
            last_write_ms: (last > 0).then_some(last),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct StatsSnapshot {
    pub readings: u64,
    pub rows_written: u64,
    pub write_errors: u64,
    pub rows_dropped: u64,
    pub skipped: u64,
    pub last_write_ms: Option<i64>,
}

#[derive(Default)]
struct Inner {
    meta: HashMap<(String, String), ChannelMeta>,
    live: HashMap<(String, String), (f64, i64)>,
    /// How widely each channel is published; see [`Shared::observe_precision`].
    precision: HashMap<(String, String), u8>,
    online: HashMap<String, bool>,
    views: Vec<String>,
}

/// Cloneable handle to the shared state.
#[derive(Clone)]
pub struct Shared {
    inner: Arc<RwLock<Inner>>,
    stats: Arc<Stats>,
    broker_connected: Arc<AtomicBool>,
}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

impl Shared {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner::default())),
            stats: Arc::new(Stats::default()),
            broker_connected: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }

    pub fn set_broker_connected(&self, connected: bool) {
        self.broker_connected.store(connected, Ordering::Relaxed);
    }

    pub fn broker_connected(&self) -> bool {
        self.broker_connected.load(Ordering::Relaxed)
    }

    /// Remember which rollup views exist, so the read path can route to them.
    pub fn set_views(&self, views: Vec<String>) {
        self.write().views = views;
    }

    pub fn views(&self) -> Vec<String> {
        self.read().views.clone()
    }

    /// Record a reading as it passes through, for the live tiles.
    pub fn observe(&self, r: &Reading) {
        self.stats.record_reading();
        self.write()
            .live
            .insert((r.node.clone(), r.sensor.clone()), (r.value, r.at / 1_000));
    }

    /// Remember how many decimals a channel is published with.
    ///
    /// A high-water mark, and deliberately so. One payload proves only what it
    /// shows: a meter reading `8.230` proves three decimals, and the next round
    /// landing on `8.24` proves two -- taking the maximum stops a tile changing
    /// width between refreshes. Zero is not a claim at all (an integer payload,
    /// or an exponent form), so it is not recorded and cannot pull a channel
    /// back down.
    ///
    /// Like everything else here this is re-learned after a restart, from the
    /// next reading on each channel rather than from a stored width.
    pub fn observe_precision(&self, node: &str, sensor: &str, decimals: u8) {
        if decimals == 0 {
            return;
        }
        let mut inner = self.write();
        let seen = inner
            .precision
            .entry((node.to_string(), sensor.to_string()))
            .or_insert(0);
        *seen = (*seen).max(decimals);
    }

    /// The widest a channel has been published since startup, if it has been
    /// seen at all.
    pub fn precision(&self, node: &str, sensor: &str) -> Option<u8> {
        self.read()
            .precision
            .get(&(node.to_string(), sensor.to_string()))
            .copied()
    }

    pub fn set_meta(&self, node: &str, sensor: &str, meta: ChannelMeta) {
        self.write()
            .meta
            .insert((node.to_string(), sensor.to_string()), meta);
    }

    pub fn forget_meta(&self, node: &str, sensor: &str) {
        self.write()
            .meta
            .remove(&(node.to_string(), sensor.to_string()));
    }

    pub fn meta(&self, node: &str, sensor: &str) -> Option<ChannelMeta> {
        self.read()
            .meta
            .get(&(node.to_string(), sensor.to_string()))
            .cloned()
    }

    /// Every channel that has ever been announced, whether or not it has
    /// published yet.
    pub fn known_channels(&self) -> Vec<(String, String)> {
        self.read().meta.keys().cloned().collect()
    }

    pub fn live(&self, node: &str, sensor: &str) -> Option<(f64, i64)> {
        self.read()
            .live
            .get(&(node.to_string(), sensor.to_string()))
            .copied()
    }

    /// Record a node's availability. Returns whether this *changed* anything --
    /// the broker re-delivers the retained status on every connect, and a row
    /// per reconnect of this service would be noise in a table whose whole
    /// point is transitions.
    pub fn set_online(&self, node: &str, online: bool) -> bool {
        let mut inner = self.write();
        match inner.online.insert(node.to_string(), online) {
            Some(previous) => previous != online,
            None => true,
        }
    }

    /// Seed the availability map from what the database already recorded,
    /// without treating it as a transition.
    ///
    /// Without this, every restart of the service writes a status row per node:
    /// the broker re-delivers the retained last-will on connect, and to an
    /// empty map that looks like news. Seeded, only an actual change is
    /// recorded -- which is what the table is for.
    pub fn seed_online(&self, node: &str, online: bool) {
        self.write().online.insert(node.to_string(), online);
    }

    pub fn online(&self, node: &str) -> Option<bool> {
        self.read().online.get(node).copied()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        // A poisoned lock here means a panic while holding it. Nothing in this
        // module can panic between lock and drop, and taking the data anyway
        // beats taking the whole dashboard down over a counter.
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(node: &str, sensor: &str, value: f64) -> Reading {
        Reading {
            node: node.into(),
            sensor: sensor.into(),
            value,
            at: 1_700_000_000_000_000,
        }
    }

    #[test]
    fn the_last_value_per_channel_is_kept() {
        let s = Shared::new();
        s.observe(&reading("bad", "humidity", 51.0));
        s.observe(&reading("bad", "humidity", 52.0));
        s.observe(&reading("bad", "temperature", 21.0));
        assert_eq!(s.live("bad", "humidity").unwrap().0, 52.0);
        assert_eq!(s.live("bad", "temperature").unwrap().0, 21.0);
        assert!(s.live("bad", "co2").is_none());
        // Milliseconds, not microseconds: what the API hands the browser.
        assert_eq!(s.live("bad", "humidity").unwrap().1, 1_700_000_000_000);
        assert_eq!(s.stats().snapshot().readings, 3);
    }

    #[test]
    fn a_channels_width_is_a_high_water_mark() {
        let shared = Shared::new();
        assert_eq!(shared.precision("wasserzaehler_kalt", "value"), None);

        // Three decimals proven, then a round that happens to land on two: the
        // meter did not get coarser, the reading just ended in a digit that is
        // not zero. A tile that narrowed here would flicker between refreshes.
        shared.observe_precision("wasserzaehler_kalt", "value", 3);
        shared.observe_precision("wasserzaehler_kalt", "value", 2);
        assert_eq!(shared.precision("wasserzaehler_kalt", "value"), Some(3));

        // Zero is "no claim" -- an integer payload or an exponent form -- and
        // must not be mistaken for "no decimals".
        shared.observe_precision("wasserzaehler_kalt", "value", 0);
        assert_eq!(shared.precision("wasserzaehler_kalt", "value"), Some(3));
        shared.observe_precision("terrasse", "uptime", 0);
        assert_eq!(shared.precision("terrasse", "uptime"), None);

        // And it is per channel, not per node.
        assert_eq!(shared.precision("wasserzaehler_warm", "value"), None);
    }

    #[test]
    fn only_a_real_availability_change_is_reported() {
        let s = Shared::new();
        // First sighting counts: it is the transition from "unknown".
        assert!(s.set_online("terrasse", true));
        // The retained re-delivery on every reconnect does not.
        assert!(!s.set_online("terrasse", true));
        assert!(s.set_online("terrasse", false));
        assert!(!s.set_online("terrasse", false));
        assert_eq!(s.online("terrasse"), Some(false));
        assert_eq!(s.online("kueche"), None);
    }

    #[test]
    fn a_seeded_state_makes_the_retained_replay_a_non_event() {
        let s = Shared::new();
        s.seed_online("terrasse", false);
        // What the broker sends on connect, matching what the database says.
        assert!(!s.set_online("terrasse", false));
        // A node that came back while the service was down still counts.
        assert!(s.set_online("terrasse", true));
    }

    #[test]
    fn metadata_arrives_and_can_be_withdrawn() {
        let s = Shared::new();
        let meta = ChannelMeta {
            name: "CO₂".into(),
            unit: "ppm".into(),
            device_class: "carbon_dioxide".into(),
            node_name: "Schlafzimmer".into(),
        };
        s.set_meta("schlafzimmer", "co2", meta.clone());
        assert_eq!(s.meta("schlafzimmer", "co2"), Some(meta));
        assert_eq!(s.known_channels().len(), 1);
        // An empty retained discovery payload means the entity is gone.
        s.forget_meta("schlafzimmer", "co2");
        assert_eq!(s.meta("schlafzimmer", "co2"), None);
        assert!(s.known_channels().is_empty());
    }

    #[test]
    fn counters_add_up() {
        let s = Shared::new();
        let stats = s.stats();
        stats.record_write(10);
        stats.record_write(5);
        stats.record_write_error();
        stats.record_dropped(3);
        stats.record_skipped();
        let snap = stats.snapshot();
        assert_eq!(snap.rows_written, 15);
        assert_eq!(snap.write_errors, 1);
        assert_eq!(snap.rows_dropped, 3);
        assert_eq!(snap.skipped, 1);
        assert!(snap.last_write_ms.is_some());
    }

    #[test]
    fn no_write_yet_is_none_rather_than_1970() {
        assert_eq!(Stats::default().snapshot().last_write_ms, None);
    }

    #[test]
    fn the_handle_shares_one_state() {
        let a = Shared::new();
        let b = a.clone();
        b.set_views(vec!["readings_1m".into()]);
        assert_eq!(a.views(), vec!["readings_1m".to_string()]);
        assert!(!a.broker_connected());
        b.set_broker_connected(true);
        assert!(a.broker_connected());
    }
}

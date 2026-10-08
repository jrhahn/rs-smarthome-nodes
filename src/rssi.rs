//! Signal strength of the link this node publishes over.
//!
//! Every node reaches the broker over Wi-Fi, so unlike a sensor on a bus there
//! is nothing to configure and nothing to detect: if a round got far enough to
//! publish, the association exists and the driver already holds its RSSI. That
//! is what makes this worth having on a fleet spread across a flat — the
//! outdoor node's publishes have already been lost once to a weak link (the
//! 20 s `WIFI_BUDGET` expiring with no way to report why), and a number on the
//! dashboard turns that from a guess into a reading.
//!
//! Getting at it needs the raw binding rather than `esp-wifi`'s own API.
//! `esp-wifi` surfaces signal strength only inside [`AccessPointInfo`], which
//! comes from `scan_n` — a radio sweep that costs hundreds of milliseconds and
//! disturbs the very association we want to measure. `esp_wifi_sta_get_ap_info`
//! reads the value the driver has already stored for the connected AP, and
//! while `esp-wifi` keeps its bindings in a private `binary` module, the same
//! generated bindings are a crate of their own where the symbol is public.
//!
//! Where it is sampled matters: `collect_samples` runs *before* the radio comes
//! up on a battery node, so the reading has to be taken after the connect, in
//! `publish_samples`. See the push there.

use core::fmt::Write as _;

use heapless::String;

use crate::sensors::EntityDescriptor;

/// Home Assistant discovery metadata. `signal_strength` in dBm is a device
/// class Home Assistant knows, so the entity gets the right icon and the
/// history graph gets a sensible axis without any of it being spelled out here.
///
/// Deliberately published as dBm rather than as a bar count or a percentage.
/// The number is the measurement; how many bars that is worth is a question for
/// whatever draws it, and a node that has already gone to sleep cannot be asked
/// to re-map its scale.
pub const DESCRIPTORS: &[EntityDescriptor] = &[EntityDescriptor {
    key: "rssi",
    name: "Signal",
    unit: "dBm",
    device_class: "signal_strength",
    state_class: "measurement",
}];

/// RSSI of the connected access point in dBm, or `None` when the station is not
/// associated.
///
/// The struct is zeroed rather than left uninitialised because the call fills
/// only the fields the driver knows; a non-zero return means it filled nothing,
/// and then there is no reading to report rather than a plausible-looking one.
#[cfg(feature = "hal")]
pub fn read() -> Option<i8> {
    use esp_wifi_sys_esp32c3::include::{esp_wifi_sta_get_ap_info, wifi_ap_record_t};

    // SAFETY: the pointer is to a local, correctly-sized and zeroed record, and
    // the driver only writes within it. Callable only once `esp_wifi` has been
    // initialised, which on this firmware is true from `bring_up_wifi` onwards.
    let mut record: wifi_ap_record_t = unsafe { core::mem::zeroed() };
    let err = unsafe { esp_wifi_sta_get_ap_info(&mut record) };
    if err == 0 {
        Some(record.rssi)
    } else {
        // Logged rather than swallowed: the entity is announced unconditionally,
        // so a silent `None` shows up as a permanently unavailable sensor with
        // nothing anywhere saying why.
        log::warn!("esp_wifi_sta_get_ap_info failed: {}", err);
        None
    }
}

/// Format a reading for MQTT.
///
/// A plain integer in dBm. The sign belongs to the number rather than to the
/// unit -- an RSSI is negative except in the rare very-strong case the binding's
/// own comment warns about, and Home Assistant plots either without help.
pub fn write_dbm(buf: &mut String<16>, dbm: i8) {
    let _ = write!(buf, "{}", dbm);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_carries_the_units_home_assistant_expects() {
        let d = &DESCRIPTORS[0];
        assert_eq!(d.key, "rssi");
        assert_eq!(d.unit, "dBm");
        assert_eq!(d.device_class, "signal_strength");
    }

    #[test]
    fn a_reading_formats_as_a_signed_integer() {
        for (dbm, expected) in [(-42i8, "-42"), (-100, "-100"), (0, "0"), (3, "3")] {
            let mut buf = String::new();
            write_dbm(&mut buf, dbm);
            assert_eq!(buf.as_str(), expected, "dbm {dbm}");
        }
    }

    #[test]
    fn there_is_exactly_one_entity_per_node() {
        // A second one would push the busiest node against MAX_ENTITIES, which
        // `no_node_overflows_the_entity_vec` guards -- but only for the fleet as
        // configured. This pins the intent.
        assert_eq!(DESCRIPTORS.len(), 1);
    }
}

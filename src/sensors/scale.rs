//! Discovery metadata for the HX711 load cell.
//!
//! The load cell is not a plain [`Sensor`](super::Sensor): its reading feeds the
//! tare baseline and the bird-presence edge detection in `main`, and the raw ->
//! grams conversion needs the runtime calibration from [`crate::config`]. So the
//! HX711 keeps its own path and only contributes its Home Assistant descriptor
//! here, so the discovery publisher (#16) can treat it like any other reading.

use super::EntityDescriptor;

pub const DESCRIPTORS: &[EntityDescriptor] = &[EntityDescriptor {
    key: "weight",
    name: "Gewicht",
    unit: "g",
    device_class: "weight",
    state_class: "measurement",
}];

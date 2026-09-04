//! Discovery metadata for the HX711 load cell.
//!
//! The load cell is not a plain [`Sensor`](super::Sensor): its reading feeds the
//! tare baseline and the bird-presence edge detection in `main`, and the raw ->
//! grams conversion needs the runtime calibration from [`crate::config`]. So the
//! HX711 keeps its own path and only contributes its Home Assistant descriptor
//! here, so the discovery publisher (#16) can treat it like any other reading.

use super::EntityDescriptor;

pub const DESCRIPTORS: &[EntityDescriptor] = &[
    EntityDescriptor {
        key: "weight",
        name: "Gewicht",
        unit: "g",
        device_class: "weight",
        state_class: "measurement",
    },
    // How long the load stayed on the cell. Only a visit produces one, so this
    // entity is stale between birds by design — it is the length of the *last*
    // visit, not a live value. `main` watches a visit through while awake
    // (see `crate::presence`), which is what makes the number better than the
    // deep-sleep interval it used to be quantised to.
    EntityDescriptor {
        key: "visit",
        name: "Besuchsdauer",
        unit: "s",
        device_class: "duration",
        state_class: "measurement",
    },
];

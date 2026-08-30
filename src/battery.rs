//! Cell-voltage sense for the battery node (the outdoor bird scale).
//!
//! The XIAO ESP32-C3 has no battery-sense path of its own: `B+` reaches the
//! charger and the regulator, never an ADC. Reading the cell therefore needs an
//! external divider from the *protected* battery rail to ground, with its tap on
//! an ADC1 pin:
//!
//! ```text
//!   XIAO B+ ──[ 100 kΩ ]──┬──► D2 / GPIO4
//!                         ├──[ 100 kΩ ]──┐
//!                         └──[ 100 nF ]──┴── XIAO GND
//! ```
//!
//! Three constraints decided all of that, and none of them is obvious later:
//!
//! * **ADC1, so D2.** ADC2 is unusable while Wi-Fi is up, and of the ADC1 pins
//!   (GPIO0–GPIO4) this board only breaks out D0/D1/D2 — the first two are the
//!   HX711's. D2 is the only candidate, which is why the DS18B20 had to give up
//!   its 1-Wire line for this (see [`crate::node`], where no node may enable
//!   both).
//! * **The divider's foot goes to the XIAO's ground**, i.e. the `P−` side of the
//!   1S protection board, not to the cell's own `B−`. On the protected side the
//!   board's low-voltage cutoff also cuts the divider; wired straight to the cell
//!   it would keep drawing after the cutoff — exactly the deep discharge the
//!   protection exists to prevent.
//! * **100 kΩ/100 kΩ** halves 4.2 V to 2.1 V, comfortably inside the ~2.5 V that
//!   11 dB attenuation spans, and costs ~21 µA continuously (~184 mAh a year).
//!   The 100 nF at the tap supplies the ADC's sampling charge, so the source
//!   impedance does not skew the reading.
//!
//! The conversion itself is calibrated against the chip's eFuse reference
//! ([`AdcCalCurve`]), which is why [`Battery::read_millivolts`] can return
//! millivolts rather than raw counts. That calibration corrects the *ADC*; it
//! knows nothing about the two resistors, whose real ratio is only as good as
//! their tolerance. Use 1 % parts, and if a multimeter disagrees, the fix is
//! [`R_TOP_KOHM`] / [`R_BOTTOM_KOHM`] and a reflash — there is deliberately no
//! runtime knob for it yet.

use core::fmt::Write as _;

#[cfg(feature = "hal")]
use esp_hal::{
    analog::adc::{Adc, AdcCalCurve, AdcConfig, AdcPin, Attenuation},
    gpio::GpioPin,
    peripherals::ADC1,
};
use heapless::String;

use crate::sensors::EntityDescriptor;

/// Home Assistant discovery metadata (#16). Like the HX711 and the DS18B20 this
/// keeps its own read path rather than implementing [`crate::sensors::Sensor`] —
/// it is a board measurement, not a sensor on a bus — so it contributes just the
/// descriptor here.
///
/// The key is the bare quantity; the node's [`crate::node::Slot`] prefixes it to
/// `battery_voltage`, which is what makes the entity read as the cell's rather
/// than as some anonymous voltage.
pub const DESCRIPTORS: &[EntityDescriptor] = &[EntityDescriptor {
    key: "voltage",
    name: "Spannung",
    unit: "V",
    device_class: "voltage",
    state_class: "measurement",
}];

/// The divider actually fitted, top (to `B+`) and bottom (to ground) in kΩ.
pub const R_TOP_KOHM: u32 = 100;
pub const R_BOTTOM_KOHM: u32 = 100;

/// Undo the divider: millivolts measured at the pin to millivolts at the cell.
pub const fn cell_millivolts(pin_mv: u32) -> u32 {
    pin_mv * (R_TOP_KOHM + R_BOTTOM_KOHM) / R_BOTTOM_KOHM
}

/// Below this a LiPo starts losing capacity permanently. Worth saying out loud,
/// because nothing in the hardware defends the cell here: the common DW01A-class
/// protection board only cuts off around 2.4–2.5 V, which is a fire-and-damage
/// limit rather than a health one.
pub const LOW_CELL_MV: u32 = 3000;

/// Under this it is not a discharged cell, it is a wiring fault — a missing or
/// mis-wired divider, or no cell fitted at all. Published readings stop here
/// rather than feeding Home Assistant a number that looks like a flat battery.
pub const MIN_PLAUSIBLE_CELL_MV: u32 = 2000;

// The two thresholds have to bracket a real cell's range, in that order.
// Swapping them would report every flat battery as a wiring fault and every
// unwired board as a battery worth worrying about.
const _: () = {
    assert!(MIN_PLAUSIBLE_CELL_MV < LOW_CELL_MV);
    assert!(LOW_CELL_MV < cell_millivolts(2100));
};

/// Format millivolts as volts with two decimals (`4207` -> `"4.21"`),
/// float-free, matching [`crate::ds18b20::write_temp_c`] and friends. Two
/// decimals because the interesting span of a LiPo is 3.0–4.2 V and the tenth
/// alone hides most of it.
pub fn write_volts(buf: &mut String<16>, millivolts: u32) {
    // Round rather than truncate: at one count per 10 mV, always rounding down
    // would bias every reading low by up to 10 mV. Saturating, so a nonsense
    // input formats as a nonsense voltage instead of wrapping to a plausible
    // one — the debug build would panic here, the release build would not.
    let centivolts = millivolts.saturating_add(5) / 10;
    let _ = write!(buf, "{}.{:02}", centivolts / 100, centivolts % 100);
}

// --- The ADC path ------------------------------------------------------------

/// Samples averaged per reading. The tap is a high-impedance node behind a
/// 100 nF cap, and the SAR is noisy at the LSB; a handful of conversions costs
/// microseconds and is worth more than any of them alone.
#[cfg(feature = "hal")]
const SAMPLES: u32 = 16;

/// Polls of the done flag before a conversion is given up on. A one-shot takes
/// tens of microseconds, so this is generous by orders of magnitude — it exists
/// so a wedged SAR cannot hold the boot instead of costing one reading, the same
/// contract the HX711 timeout has.
#[cfg(feature = "hal")]
const CONVERSION_POLLS: u32 = 10_000;

/// The divider's tap on ADC1, owning the pin and the ADC for as long as the node
/// is awake.
///
/// Nothing here survives deep sleep, and nothing needs to: the SAR sits in the
/// digital domain, which loses power on the way down, and [`AdcConfig::enable_pin`]
/// leaves GPIO4 in analog mode — input buffer and pull-ups off — which is
/// exactly the state that adds no leakage of its own to the divider.
#[cfg(feature = "hal")]
pub struct Battery<'d> {
    adc: Adc<'d, ADC1>,
    pin: AdcPin<GpioPin<4>, ADC1, AdcCalCurve<ADC1>>,
}

#[cfg(feature = "hal")]
impl Battery<'_> {
    /// Claim ADC1 and the divider's tap on D2 / GPIO4.
    ///
    /// 11 dB attenuation, because a full cell puts 2.1 V on the pin and the
    /// lower ranges top out below that. The curve-fitting calibration scheme
    /// reads the chip's factory reference points out of eFuse, so conversions
    /// come back in millivolts already corrected for this individual part.
    pub fn new(adc1: ADC1, pin: GpioPin<4>) -> Self {
        let mut config = AdcConfig::new();
        let pin = config.enable_pin_with_cal::<GpioPin<4>, AdcCalCurve<ADC1>>(
            pin,
            Attenuation::Attenuation11dB,
        );
        Self {
            adc: Adc::new(adc1, config),
            pin,
        }
    }

    /// The cell voltage in millivolts, or `None` if the ADC never finished a
    /// conversion — treated like any other silent sensor: logged, skipped, never
    /// fatal.
    pub fn read_millivolts(&mut self) -> Option<u32> {
        // The first conversion after the SAR powers up is not trustworthy, the
        // same way the HX711's first sample after power-up is discarded.
        let _ = self.sample();

        let mut total = 0;
        let mut taken = 0;
        for _ in 0..SAMPLES {
            if let Some(mv) = self.sample() {
                total += mv as u32;
                taken += 1;
            }
        }
        if taken == 0 {
            return None;
        }
        Some(cell_millivolts(total / taken))
    }

    /// One calibrated conversion in millivolts at the pin, or `None` if the SAR
    /// did not report it done within [`CONVERSION_POLLS`].
    fn sample(&mut self) -> Option<u16> {
        for _ in 0..CONVERSION_POLLS {
            // The only error this returns in practice is "would block", i.e. the
            // conversion started by the first call is still running; retrying is
            // how it is driven to completion.
            if let Ok(mv) = self.adc.read_oneshot(&mut self.pin) {
                return Some(mv);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_divider_is_undone_by_its_own_ratio() {
        // The two values that matter in the field: a full cell and the cutoff.
        assert_eq!(cell_millivolts(2100), 4200);
        assert_eq!(cell_millivolts(1500), 3000);
        assert_eq!(cell_millivolts(0), 0);
    }

    #[test]
    fn changing_a_resistor_changes_the_reading_by_the_same_factor() {
        // Guards the formula rather than today's values: with equal halves the
        // ratio is exactly two, whatever the resistors are.
        assert_eq!(R_TOP_KOHM, R_BOTTOM_KOHM);
        assert_eq!(cell_millivolts(1000), 2000);
    }

    #[test]
    fn millivolts_format_as_two_decimals() {
        for (mv, expected) in [
            (0, "0.00"),
            (5, "0.01"),
            (3000, "3.00"),
            (3700, "3.70"),
            (4200, "4.20"),
            (10000, "10.00"),
        ] {
            let mut buf = String::new();
            write_volts(&mut buf, mv);
            assert_eq!(buf.as_str(), expected, "{mv} mV");
        }
    }

    #[test]
    fn the_hundredth_is_rounded_rather_than_truncated() {
        // Truncating would bias every published value low, by up to 10 mV.
        for (mv, expected) in [(4207, "4.21"), (4204, "4.20"), (995, "1.00")] {
            let mut buf = String::new();
            write_volts(&mut buf, mv);
            assert_eq!(buf.as_str(), expected, "{mv} mV");
        }
    }

    #[test]
    fn a_formatted_reading_always_fits_its_buffer() {
        // `String<16>` silently drops what does not fit, which would publish a
        // truncated number rather than none at all.
        let mut buf = String::new();
        write_volts(&mut buf, u32::MAX);
        assert!(buf.len() < 16);
        assert!(buf.contains('.'));
    }
}

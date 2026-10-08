//! Bench rig for the `solarleuchte` lamp: the breathing fade, driven the way
//! the finished node will drive it.
//!
//! LEDC on `D8` into the XY-MOS module's `TRIG/PWM`, which carries its own
//! 100 Ω gate resistor and 100 kΩ pulldown. No expander, no I²C: see
//! [`docs/solarleuchte.md`](../docs/solarleuchte.md) for why the PCA9685 that
//! an earlier rig used came back out of the circuit once the panel was known.
//!
//! **With no string attached the module's own indicator LED is the load** --
//! it hangs off the trigger net, so it breathes along and proves the whole
//! path from GPIO to gate without anything else connected.
//!
//! ```bash
//! cargo run --release --example lamp_bench
//! ```
//!
//! What to look for, in this order: that it breathes at all; that the turn at
//! the top is round rather than kinked; and above all **the bottom**, where
//! the two D4184 in parallel take ~0.8 µs to cross their threshold and the
//! gamma curve is spending most of its resolution. A fade that sticks or steps
//! down there is the MOSFET, not the table.

#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    gpio::DriveMode,
    ledc::{
        channel::{self, ChannelHW, ChannelIFace},
        timer::{self, TimerIFace},
        LSGlobalClkSource, Ledc, LowSpeed,
    },
    time::Rate,
};
use log::info;
use rs_smarthome_nodes::battery::{self, Battery};
use rs_smarthome_nodes::lamp;

/// One full breath, dark to bright to dark. Under ~10 s reads as agitated.
const PERIOD_MS: u32 = 25_000;

/// How long each table entry is held. Falls out of the period and the table.
const STEP_MS: u32 = PERIOD_MS / lamp::BREATH.len() as u32;

/// **1 kHz, and deliberately not faster.**
///
/// Raising it makes the bottom of the fade worse, which is the opposite of the
/// intuition. The MOSFETs' switching time is fixed at ~0.8 µs, while one LSB
/// is `1 / (f × 4096)` -- 244 ns here. At 4 kHz the LSB shrinks to 61 ns and
/// the same switching time swallows four times as many codes, in exactly the
/// region gamma correction has spent its resolution on. 1 kHz is already
/// flicker-free to the eye and to a phone camera.
const PWM_HZ: u32 = 1_000;

/// Ceiling the bench applies, as a percentage of [`lamp::BREATH`].
///
/// The node does not use this: there the ceiling is
/// [`lamp::MAX_DUTY_PCT`] scaled by the twilight and charge factors, which
/// need a clock and a cell this rig has neither the NTP anchor nor the
/// evening for. This is the same arithmetic with those two factors pinned, so
/// a brightness can be looked at on a bench at any hour.
const CEILING_PCT: u32 = 30;

/// [`CEILING_PCT`] of the breath, rounded, exactly as [`lamp::duty`] rounds.
fn bench_duty(step: usize) -> u32 {
    let shape = lamp::BREATH[step % lamp::BREATH.len()] as u32;
    (shape * CEILING_PCT + 50) / 100
}

/// **Hold the output full on instead of breathing, to measure the string.**
///
/// A multimeter in series with a breathing lamp reads a moving average, and
/// the number that decides whether the string needs a series resistor is the
/// *peak*: during every PWM on-phase the full cell voltage sits across the
/// LEDs however small the duty, so a duty ceiling cannot protect them from it.
/// Holding 100 % makes the meter read that peak directly.
///
/// With no cell fitted, `VIN+` comes from the XIAO's own charger at ~4.2 V,
/// which is the worst case the string ever sees and exactly the case worth
/// measuring. Divide the reading by the number of LEDs: under ~20 mA each is
/// fine, above it the fix is one resistor in series with the whole string.
const MEASURE: bool = false;

/// `esp-backtrace` is configured with `custom-halt` for the firmware's sake --
/// see the note on the dependency in `Cargo.toml` -- so every binary in this
/// crate has to supply the symbol. The firmware's version stores a flag in RTC
/// RAM and resets; a bench rig wants the opposite, to stop dead with the
/// backtrace still on screen.
#[no_mangle]
extern "Rust" fn custom_halt() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_hal::main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::_80MHz));
    esp_println::logger::init_logger_from_env();

    let mut ledc = Ledc::new(peripherals.LEDC);
    ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);

    let mut lstimer0 = ledc.timer::<LowSpeed>(timer::Number::Timer0);
    lstimer0
        .configure(timer::config::Config {
            duty: timer::config::Duty::Duty12Bit,
            clock_source: timer::LSClockSource::APBClk,
            frequency: Rate::from_hz(PWM_HZ),
        })
        .expect("LEDC timer: 12 bit at 1 kHz is well inside what APBClk can divide to");

    // D8 on the XIAO silkscreen. The gate network it feeds lives on the
    // XY-MOS module, so there is nothing between this pin and `TRIG/PWM`.
    let mut channel0 = ledc.channel(channel::Number::Channel0, peripherals.GPIO8);
    channel0
        .configure(channel::config::Config {
            timer: &lstimer0,
            duty_pct: 0,
            drive_mode: DriveMode::PushPull,
        })
        .expect("LEDC channel 0 on GPIO8");

    // Same divider and same pin as the terrasse node, so the firmware's own
    // driver reads it: 100 kΩ / 100 kΩ on `D2`, ratio undone and the ADC
    // calibrated against the chip's eFuse reference inside `battery.rs`.
    // Reading it here is half a wiring check and half the number the duty
    // ceiling will be expressed against.
    let mut cell = Battery::new(peripherals.ADC1, peripherals.GPIO4);
    match cell.read_millivolts() {
        Some(mv) => info!("cell {mv} mV, ~{} %", battery::percent(mv)),
        None => info!("cell: no ADC conversion -- check the divider on D2"),
    }

    let delay = Delay::new();

    if MEASURE {
        channel0.set_duty_hw(4095);
        info!("MEASURE: held at 4095/4096, full on");
        info!("meter in series with the string, DC amps, 10 A jack");
        loop {
            delay.delay_millis(5_000);
            match cell.read_millivolts() {
                // Under load and worth watching: a cell that sags here is
                // telling you the string is pulling more than it can give.
                Some(mv) => info!("full on, cell {mv} mV, ~{} %", battery::percent(mv)),
                None => info!("full on, cell unreadable"),
            }
        }
    }

    info!("breathing on D8: {PERIOD_MS} ms period, {STEP_MS} ms per step, {PWM_HZ} Hz");
    info!(
        "bench ceiling {CEILING_PCT} % -> peak duty {}/4096",
        bench_duty(128)
    );
    info!("with no string attached, watch the XY-MOS indicator LED");

    let mut step: usize = 0;
    loop {
        channel0.set_duty_hw(bench_duty(step));

        // Once per breath, so a long run stays readable.
        if step.is_multiple_of(lamp::BREATH.len()) {
            match cell.read_millivolts() {
                Some(mv) => info!(
                    "breath {}, cell {mv} mV, ~{} %",
                    step / lamp::BREATH.len() + 1,
                    battery::percent(mv)
                ),
                None => info!("breath {}, cell unreadable", step / lamp::BREATH.len() + 1),
            }
        }

        delay.delay_millis(STEP_MS);
        step += 1;
    }
}

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
    ledc::{
        channel::{self, ChannelIFace},
        timer::{self, TimerIFace},
        LSGlobalClkSource, Ledc, LowSpeed,
    },
    prelude::*,
};
use log::info;

/// One full breath, dark to bright to dark. Under ~10 s reads as agitated.
const PERIOD_MS: u32 = 25_000;

/// **Gamma-corrected half-cosine, one full breath, 12-bit duty.**
///
/// Two corrections live in this one table, and both are needed.
///
/// The *cosine* is why the breath has no corners: a triangle ramp visibly
/// kinks at the top and bottom, where this flattens into the turn.
///
/// The *gamma* is why it looks linear at all. The eye responds roughly to the
/// 1/2.2 power of emitted light, so feeding a linear duty ramp to an LED makes
/// it leap away from black and then crawl once it is bright. Raising the
/// intended perceived brightness to 2.2 before it becomes a duty cancels that.
///
/// 256 entries, because the largest step between neighbours is then **1.23 %**
/// of perceived brightness -- under the ~2 % that is noticeable -- so no
/// interpolation is needed between them. Generated with:
///
/// ```python
/// round((((1 - math.cos(2*math.pi*i/256)) / 2) ** 2.2) * 4095)
/// ```
///
/// Verified on hardware 2026-10-04 and unchanged since: the curve is
/// independent of whatever drives the LEDs.
const BREATH: [u16; 256] = [
       0,    0,    0,    0,    0,    0,    0,    0,    0,    0,    0,    1,
       1,    1,    2,    2,    3,    4,    5,    6,    8,   10,   12,   15,
      18,   21,   25,   29,   34,   40,   46,   52,   60,   68,   77,   87,
      97,  109,  122,  135,  150,  165,  182,  200,  219,  240,  261,  284,
     308,  334,  361,  389,  419,  450,  483,  517,  553,  590,  629,  669,
     710,  753,  798,  844,  891,  940,  990, 1042, 1095, 1149, 1204, 1261,
    1319, 1378, 1438, 1499, 1561, 1624, 1688, 1753, 1818, 1884, 1950, 2017,
    2084, 2152, 2220, 2288, 2356, 2424, 2492, 2559, 2626, 2693, 2760, 2825,
    2890, 2955, 3018, 3080, 3142, 3202, 3261, 3318, 3374, 3428, 3481, 3533,
    3582, 3629, 3675, 3718, 3760, 3799, 3836, 3871, 3904, 3934, 3961, 3986,
    4009, 4029, 4046, 4061, 4073, 4083, 4090, 4094, 4095, 4094, 4090, 4083,
    4073, 4061, 4046, 4029, 4009, 3986, 3961, 3934, 3904, 3871, 3836, 3799,
    3760, 3718, 3675, 3629, 3582, 3533, 3481, 3428, 3374, 3318, 3261, 3202,
    3142, 3080, 3018, 2955, 2890, 2825, 2760, 2693, 2626, 2559, 2492, 2424,
    2356, 2288, 2220, 2152, 2084, 2017, 1950, 1884, 1818, 1753, 1688, 1624,
    1561, 1499, 1438, 1378, 1319, 1261, 1204, 1149, 1095, 1042,  990,  940,
     891,  844,  798,  753,  710,  669,  629,  590,  553,  517,  483,  450,
     419,  389,  361,  334,  308,  284,  261,  240,  219,  200,  182,  165,
     150,  135,  122,  109,   97,   87,   77,   68,   60,   52,   46,   40,
      34,   29,   25,   21,   18,   15,   12,   10,    8,    6,    5,    4,
       3,    2,    2,    1,    1,    1,    0,    0,    0,    0,    0,    0,
       0,    0,    0,    0,
];

/// How long each table entry is held. Falls out of the period and the table.
const STEP_MS: u32 = PERIOD_MS / BREATH.len() as u32;

/// **1 kHz, and deliberately not faster.**
///
/// Raising it makes the bottom of the fade worse, which is the opposite of the
/// intuition. The MOSFETs' switching time is fixed at ~0.8 µs, while one LSB
/// is `1 / (f × 4096)` -- 244 ns here. At 4 kHz the LSB shrinks to 61 ns and
/// the same switching time swallows four times as many codes, in exactly the
/// region gamma correction has spent its resolution on. 1 kHz is already
/// flicker-free to the eye and to a phone camera.
const PWM_HZ: u32 = 1_000;

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

#[esp_hal::entry]
fn main() -> ! {
    let peripherals = esp_hal::init({
        let mut c = esp_hal::Config::default();
        c.cpu_clock = CpuClock::Clock80MHz;
        c
    });
    esp_println::logger::init_logger_from_env();

    let mut ledc = Ledc::new(peripherals.LEDC);
    ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);

    let mut lstimer0 = ledc.timer::<LowSpeed>(timer::Number::Timer0);
    lstimer0
        .configure(timer::config::Config {
            duty: timer::config::Duty::Duty12Bit,
            clock_source: timer::LSClockSource::APBClk,
            frequency: PWM_HZ.Hz(),
        })
        .expect("LEDC timer: 12 bit at 1 kHz is well inside what APBClk can divide to");

    // D8 on the XIAO silkscreen. The gate network it feeds lives on the
    // XY-MOS module, so there is nothing between this pin and `TRIG/PWM`.
    let mut channel0 = ledc.channel(channel::Number::Channel0, peripherals.GPIO8);
    channel0
        .configure(channel::config::Config {
            timer: &lstimer0,
            duty_pct: 0,
            pin_config: channel::config::PinConfig::PushPull,
        })
        .expect("LEDC channel 0 on GPIO8");

    info!("breathing on D8: {PERIOD_MS} ms period, {STEP_MS} ms per step, {PWM_HZ} Hz");
    info!("with no string attached, watch the XY-MOS indicator LED");

    let delay = Delay::new();
    let mut step: usize = 0;
    loop {
        channel0.set_duty_hw(BREATH[step % BREATH.len()] as u32);

        // Once per breath, so a long run stays readable.
        if step % BREATH.len() == 0 {
            info!("breath {}", step / BREATH.len() + 1);
        }

        delay.delay_millis(STEP_MS);
        step += 1;
    }
}

//! Bench rig for the `solarleuchte` design: a smooth breathing fade on one
//! PCA9685 channel, plus the deep-sleep check that preceded it.
//!
//! **The deep-sleep question is settled.** It was the one assumption the power
//! budget in [`docs/solarleuchte.md`](../docs/solarleuchte.md) rested on, since
//! light sleep is unavailable here -- `Rtc::sleep_light` resets this chip
//! rather than resuming, see `run_battery` in `src/main.rs`. Verified on
//! hardware 2026-10-04: with the ESP32-C3 in deep sleep for 30 s, the PCA9685
//! held its channel steady from its own registers and oscillator, and the lamp
//! did not flicker.
//!
//! Wiring: see `docs/solarleuchte-wiring.pdf`. Short version, `VCC` on 3.3 V
//! (never 5 V), `OE` to `GND`, `SDA`/`SCL` on `D4`/`D5`, channel 0 to the load.
//!
//! ```bash
//! cargo run --release --example pca9685_bench
//! ```

#![no_std]
#![no_main]

use core::time::Duration;

use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    i2c::master::{Config as I2cConfig, Error as I2cError, I2c},
    rtc_cntl::{sleep::TimerWakeupSource, Rtc},
};
use log::{error, info};
use pwm_pca9685::{Address, Channel, Pca9685};

/// `prescale = round(25 MHz / (4096 x f)) - 1`. Five gives 25e6/(4096x6) =
/// **1017 Hz**: flicker-free to the eye and to a phone camera, and clear of the
/// chip's 1526 Hz ceiling.
const PRESCALE: u8 = 5;

/// One full breath, dark to bright to dark.
///
/// Under ~10 s a breath reads as agitated rather than calm; 25 s is the figure
/// the design settled on.
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

/// **Deep sleep is off, and a smooth fade is why.**
///
/// Not a leftover: the two are incompatible on this board. Waking the C3 costs
/// a cold boot -- ~270 ms of ROM and app init, measured for the terrasse node
/// -- while a 25 s breath wants a new duty every 98 ms. The chip cannot be
/// asleep between steps because it cannot wake fast enough to take them.
///
/// So the choice this rig makes visible is a real one, and it is still open in
/// `docs/solarleuchte.md`:
///
/// - **awake fade**, ~25 mA for the controller on top of the LEDs, which only
///   the 10 W panel pays for;
/// - **slow drift**, one step per deep-sleep wake, which the budget was written
///   around but which is minutes per breath rather than seconds.
///
/// Set this `true` to watch the second half of the hardware check again: the
/// lamp holds its last duty through the sleep, which is the property the whole
/// design rests on.
const DEEP_SLEEP: bool = false;

/// Sleep length when [`DEEP_SLEEP`] is on.
const SLEEP: Duration = Duration::from_secs(30);

/// Steps to run before each sleep, i.e. **~20 s of reachable board**.
///
/// Not cosmetic. Deep sleep powers down the USB Serial/JTAG controller, so the
/// port vanishes for the whole sleep; a firmware that slept straight after boot
/// could only be replaced by strapping `D9` low while re-plugging, which cost
/// an evening on 2026-10-04. A wake window longer than one flash takes keeps
/// the board recoverable with no wire at all.
const STEPS_AWAKE: u32 = 200;

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

    let mut i2c = I2c::new(peripherals.I2C0, I2cConfig::default())
        .with_sda(peripherals.GPIO6)
        .with_scl(peripherals.GPIO7);

    // Scan before constructing the driver: a NACK here names the wiring fault,
    // where the driver would only say the bus did not answer. 0x40 is the
    // PCA9685; 0x70 is its all-call address and not a second device.
    info!("I2C scan:");
    let mut found = false;
    for addr in 0x08u8..0x78 {
        if i2c.read(addr, &mut [0u8; 1]).is_ok() {
            info!("  device at 0x{addr:02x}");
            found = true;
        }
    }
    if !found {
        error!("nothing on the bus -- check SDA/SCL, VCC and a common ground");
    }

    // Deliberately not `expect`: a missing or miswired PCA9685 is the expected
    // failure on a bench, and panicking on it makes the board *less* useful --
    // the panic handler's backtrace printer faults on this chip (observed
    // 2026-10-04, `Load access fault` right after the panic message), which
    // buries the one line that says what went wrong. Log it and carry on, so
    // the board keeps talking while the wiring is fixed.
    let mut pwm = match (|| -> Result<_, pwm_pca9685::Error<I2cError>> {
        let mut pwm = Pca9685::new(i2c, Address::default())?;
        // Prescale first, enable second: the register is writable only while
        // the oscillator is off, which is the state the chip powers up in.
        pwm.set_prescale(PRESCALE)?;
        pwm.enable()?;
        Ok(pwm)
    })() {
        Ok(pwm) => {
            info!("breathing: {PERIOD_MS} ms period, {STEP_MS} ms per step, 1017 Hz");
            Some(pwm)
        }
        Err(e) => {
            error!("PCA9685 not configured: {e:?} -- check SDA/SCL, VCC, GND");
            None
        }
    };

    let delay = Delay::new();
    let mut rtc = Rtc::new(peripherals.LPWR);
    let mut step: u32 = 0;

    loop {
        let duty = BREATH[(step as usize) % BREATH.len()];
        if let Some(pwm) = pwm.as_mut() {
            // A write that fails mid-run is worth one line, not a panic and not
            // a line every 98 ms: the lamp simply holds its last duty.
            if let Err(e) = pwm.set_channel_on_off(Channel::C0, 0, duty) {
                error!("write failed at step {step}: {e:?}");
            }
        }

        // Once per breath, so the log stays readable over a long run.
        if step as usize % BREATH.len() == 0 {
            info!("breath {}, duty {duty}", step as usize / BREATH.len() + 1);
        }

        delay.delay_millis(STEP_MS);
        step += 1;

        if DEEP_SLEEP && step >= STEPS_AWAKE {
            info!("sleeping {SLEEP:?} at duty {duty} -- watch the lamp, not the log");
            let wake = TimerWakeupSource::new(SLEEP);
            rtc.sleep_deep(&[&wake]);
        }
    }
}

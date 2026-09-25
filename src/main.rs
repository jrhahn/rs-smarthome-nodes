//! rs-smarthome-nodes — async firmware for a fleet of ESP32-C3 sensor nodes.
//!
//! One image serves every node: which sensors are populated, what the node is
//! called and how it is powered come from [`node`], picked by `NODE=<name>` at
//! build time or by a provisioned identity in flash. It started life as a
//! battery bird-feeder scale, and that node — now `terrasse`, the default —
//! still drives the flow described below.
//!
//! **Battery profile** — to catch short bird visits without keeping the radio
//! awake, the firmware polls by cold-booting out of deep sleep on a short
//! interval and only spends Wi-Fi energy when weight is actually on the scale:
//!   1. Bring up the HAL + Embassy executor (TIMG0).
//!   2. Read a raw weight sample from the HX711 (with a timeout, so a missing
//!      sensor can't wedge the boot).
//!   3. Compare against the tare baseline persisted in RTC RAM across sleep:
//!        - empty house  -> drift-correct the baseline, skip Wi-Fi, deep-sleep
//!          a short *idle* interval to catch the next visit;
//!        - weight present -> join Wi-Fi (STA + DHCP), publish every populated
//!          sensor over MQTT, deep-sleep a longer *active* interval to keep
//!          tracking. A final reading is published on the falling edge when the
//!          bird leaves.
//!
//! **Mains profile** — indoor air-quality nodes stay associated and sample on a
//! fixed cadence instead, because CO₂ continuity and the SDS011's duty-cycled
//! fan both rule out deep sleep.
//!
//! Home Assistant discovers every node's entities over retained MQTT config
//! messages (see [`discovery`]); calibration and tuning live in flash and are
//! updated live from Home Assistant (see [`config`]).

#![no_std]
#![no_main]

use core::time::Duration as CoreDuration;

use embassy_executor::Spawner;
use embassy_net::{tcp::TcpSocket, Config as NetConfig, Ipv4Address, Stack, StackResources};
use embassy_time::{with_timeout, Duration, Instant, Timer};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    efuse::Efuse,
    gpio::{GpioPin, Input, Level, Output, OutputOpenDrain, Pull},
    peripherals::{LPWR, RADIO_CLK, RNG, TIMG1, WIFI},
    reset::software_reset,
    rng::Rng,
    rtc_cntl::{sleep::TimerWakeupSource, Rtc},
    timer::timg::TimerGroup,
    usb_serial_jtag::UsbSerialJtag,
};
use esp_wifi::{
    config::PowerSaveMode,
    wifi::{
        ClientConfiguration, Configuration, WifiController, WifiDevice, WifiEvent, WifiStaDevice,
        WifiState,
    },
    EspWifiController,
};
use embedded_hal::delay::DelayNs as _;
use log::{info, warn};
use rust_mqtt::{
    client::{client::MqttClient, client_config::ClientConfig},
    packet::v5::publish_packet::QualityOfService,
    utils::rng_generator::CountingRng,
};

use node::{NodeConfig, Provision};
use rs_smarthome_nodes::{
    battery, clock, config, discovery, ds18b20, http, hx711, node, ntp, ota, platform, presence,
    reset_reason, rssi, sensors::scale, state, wifi, FW_VERSION,
};

use battery::Battery;
use config::Config;
use ds18b20::Ds18b20;
use hx711::Hx711;
use platform::{Samples, Sensors};

/// The concrete network-stack type used throughout the firmware.
type WifiStack = Stack<WifiDevice<'static, WifiStaDevice>>;

/// Staging buffer for a firmware download, one flash sector wide.
///
/// Static rather than stacked: 4 KB is a lot to ask of a task stack, and rather
/// than a heap allocation that could fail halfway through an update with the
/// radio up. See [`ota::Writer::new`] for why the size is a whole sector.
fn ota_staging() -> &'static mut [u8; ota::SECTOR as usize] {
    static mut STAGING: [u8; ota::SECTOR as usize] = [0; ota::SECTOR as usize];
    // SAFETY: one consumer, never re-entered. `run_update` is the only caller,
    // it runs on the single executor thread, and it is awaited to completion
    // inside one publish round -- the next round cannot begin until it has
    // returned.
    unsafe { &mut *core::ptr::addr_of_mut!(STAGING) }
}

// --- Compile-time configuration --------------------------------------------
// Override the credentials at build time, e.g.:
//   SSID=MyNet PASSWORD=hunter2 cargo run --release
// The MQTT broker address is edited here directly.
const SSID: &str = match option_env!("SSID") {
    Some(s) => s,
    None => wifi::PLACEHOLDER_SSID,
};
const PASSWORD: &str = match option_env!("PASSWORD") {
    Some(s) => s,
    None => "your-password",
};

/// Optional MQTT broker credentials, baked in at build time (see .env).
/// When unset the client connects anonymously, so an auth-free broker still
/// works out of the box.
const MQTT_USER: Option<&str> = option_env!("MQTT_USER");
const MQTT_PASSWORD: Option<&str> = option_env!("MQTT_PASSWORD");

/// Home Assistant / Mosquitto broker on the LAN. Baked in at compile time from
/// the `MQTT_BROKER` env var (see `.env` / `.env.example`), like the Wi-Fi
/// credentials. It is a private LAN IP, not a secret, but keeping it out of
/// source is cleaner; the default keeps a plain `cargo build` working.
const MQTT_BROKER: Ipv4Address = parse_ipv4(match option_env!("MQTT_BROKER") {
    Some(s) => s,
    None => "192.168.1.67",
});
const MQTT_PORT: u16 = 1883;

/// Where to ask for the time, baked in like the broker.
///
/// Defaults to the broker's own address: the machine running mosquitto is the
/// home server, which is also the one thing on this LAN that is always up and
/// already knows what time it is. Pointing at a public pool instead would make
/// every reading's timestamp depend on the house having a working uplink, and
/// add a DNS lookup to a path that currently needs none.
const NTP_SERVER: Ipv4Address = parse_ipv4(match option_env!("NTP_SERVER") {
    Some(s) => s,
    None => match option_env!("MQTT_BROKER") {
        Some(s) => s,
        None => "192.168.1.67",
    },
});

/// Const-parse a dotted-decimal IPv4 string (e.g. `"192.168.1.67"`) into an
/// [`Ipv4Address`] at compile time, so `MQTT_BROKER` can come from an env var.
const fn parse_ipv4(s: &str) -> Ipv4Address {
    let o = parse_octets(s);
    Ipv4Address::new(o[0], o[1], o[2], o[3])
}

/// The dotted-decimal → four-octet core, split out so it can be checked at
/// compile time. Lenient by design: extra separators are clamped rather than
/// panicking, so a malformed override yields a wrong (but harmless) address,
/// never a build that fails deep inside const-eval.
const fn parse_octets(s: &str) -> [u8; 4] {
    let b = s.as_bytes();
    let mut octets = [0u8; 4];
    let mut idx = 0;
    let mut cur = 0u16;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'.' {
            if idx < 3 {
                octets[idx] = cur as u8;
                idx += 1;
            }
            cur = 0;
        } else {
            cur = cur * 10 + (c - b'0') as u16;
        }
        i += 1;
    }
    if idx < 4 {
        octets[idx] = cur as u8;
    }
    octets
}

// Pin the parser against the default broker address at compile time.
const _: () = {
    let o = parse_octets("192.168.1.67");
    assert!(o[0] == 192 && o[1] == 168 && o[2] == 1 && o[3] == 67);
};

// --- Sampling / detection tuning -------------------------------------------
// Topics, the client id and the sensor set come from [`node::active`]; the
// presence threshold and the idle/active poll intervals live in
// [`config::Config`], persisted in flash and tunable live from Home Assistant.

/// How long to wait for the next retained config message after subscribing.
/// Retained values arrive within tens of ms, so once a receive hits this
/// timeout we assume the broker has sent them all and stop draining.
const CONFIG_RECV_WINDOW: Duration = Duration::from_millis(400);

/// How long a cold boot waits for someone at the serial console before carrying
/// on. Short, because it delays every power-up of every node; long enough to
/// paste three lines into a terminal that is already open.
const CONSOLE_WINDOW: Duration = Duration::from_secs(3);

/// The same wait for a board that has no usable credentials at all. It has
/// nothing else it could be doing, so it is worth waiting properly — and after
/// this it boots on and simply fails to join, which is no worse.
const CONSOLE_WINDOW_STRANDED: Duration = Duration::from_secs(120);

/// Config key carrying a tare request. Called out because, unlike every other
/// key, acting on it means deleting the retained message afterwards.
const TARE_KEY: &str = "tare";
/// Config key that forgets the discovery digest; see `REANNOUNCE_CONTROLS`.
const REANNOUNCE_KEY: &str = "reannounce";
/// Config key that zeroes the visit counter. Like the two above it is a button
/// press rather than a setting, so it is consumed rather than stored -- and
/// unlike them it touches RTC RAM instead of the config blob.
const RESET_VISITS_KEY: &str = "reset_visits";

/// Give up on a single HX711 conversion after this long. A disconnected sensor
/// (with `DT` pulled up) never becomes ready, so this bounds the boot.
const HX711_TIMEOUT: Duration = Duration::from_millis(500);

/// Upper bound on the whole Wi-Fi join + MQTT publish. Without it a failed join
/// would spin in the high-power state and drain the battery. Sensor sampling
/// happens before this window, so a slow sensor never eats into it.
const WIFI_BUDGET: Duration = Duration::from_secs(20);

/// How long to wait for the MQTT DISCONNECT and the TCP FIN to actually leave
/// the board at the end of a round. Short on purpose: the readings are already
/// out by then, so this only buys a tidy shutdown, and it is spent inside
/// [`WIFI_BUDGET`].
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(2);

/// Cadence of the gas sensor's sampling step.
///
/// Sensirion's index algorithm is specified for a 0.5-10 s interval and
/// configured here for exactly one second (`sgp41::SAMPLING_INTERVAL_SECS`), so
/// this is not a tuning knob: changing it without changing that constant makes
/// the index wrong rather than coarse. The step itself takes ~50 ms, so the
/// wait is what sets the pace.
const GAS_STEP_INTERVAL: Duration = Duration::from_secs(1);

/// Hard cap on how long one visit keeps the node awake.
///
/// A bird is expected to leave well inside this. The cap exists for a load that
/// does not — snow, a twig, a squirrel that settles in — so a stuck cell can
/// never hold the CPU awake and flatten the cell.
const VISIT_MAX: Duration = Duration::from_secs(60);

/// Consecutive below-threshold samples that end a visit. More than one so a
/// bird shifting its weight for an instant does not read as a departure.
const VISIT_DEPART_SAMPLES: u8 = 3;

/// Samples taken right after the load first crosses the threshold are dropped:
/// the bird is still landing and the cell is still ringing, so they would drag
/// the median toward a weight nothing ever had.
const VISIT_SETTLE: Duration = Duration::from_millis(400);

/// Convenience: allocate a `T` with `'static` lifetime from a `StaticCell`.
macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        STATIC_CELL.init($val)
    }};
}

/// The load-cell driver as this board wires it: esp-hal pins and delay. The
/// driver itself names none of those types (see [`hx711`]).
type Scale<'d> = Hx711<Input<'d>, Output<'d>, Delay>;

/// Everything this node can measure. Absent hardware is `None`, so both power
/// profiles run the same sampling code.
struct Board<'d> {
    scale: Option<Scale<'d>>,
    probe: Option<Ds18b20<'d>>,
    battery: Option<Battery<'d>>,
    sensors: Sensors,
}

/// The peripherals needed to bring the radio up, bundled so they can be handed
/// down the call chain in one piece.
struct Radio {
    timg1: TIMG1,
    rng: RNG,
    radio_clk: RADIO_CLK,
    wifi: WIFI,
}

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    // --- 1. HAL & async runtime --------------------------------------------
    let hal_config = {
        let mut c = esp_hal::Config::default();
        // 80 MHz rather than the 160 this chip will do. That is exactly the
        // minimum esp-wifi documents for the radio (`MIN_CLOCK` in its `init`,
        // which rejects anything below it), and dynamic power scales with the
        // clock — so this roughly halves what the core burns while idling in
        // the executor.
        //
        // It is a thermal fix, not a power one. A mains node never sleeps, so
        // whatever the core burns ends up as heat in the enclosure and the
        // sensors read their own board rather than the room: measured against
        // a reference thermometer on `schlafzimmer`, 2026-09-03, the air at the
        // electronics sat ~1 °C above the room.
        //
        // Nothing here needs the speed. A round is a handful of I²C
        // transactions and one MQTT publish, and the I²C and UART baud rates
        // derive from APB, not from the CPU clock, so their timing is
        // unchanged. On a battery node the trade is roughly neutral rather than
        // a win — half the clock means twice as long awake for the same work —
        // but the radio-idle stretches, which dominate a wake, still cost less.
        c.cpu_clock = CpuClock::Clock80MHz;
        c
    };
    let peripherals = esp_hal::init(hal_config);

    esp_println::logger::init_logger_from_env();
    esp_alloc::heap_allocator!(72 * 1024);

    // TIMG0 drives the global Embassy executor (per the hardware spec).
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_hal_embassy::init(timg0.timer0);

    // Who am I? A provisioned identity in flash wins over the one this image was
    // built with. Must happen before any peripheral is touched: the sensor set
    // decides which buses come up at all.
    node::init();
    node::set_mac(Efuse::read_base_mac_address());
    let node = node::active();

    info!(
        "node '{}' ({}) booted, {} profile",
        node.id,
        node.name,
        node.power.label()
    );
    // Say where to reach this board if it needs to be told what it is; the MAC
    // is the only name it is sure of before provisioning.
    info!(
        "provision topic: {}",
        node::provision_topic(node::mac())
    );
    info!("firmware: {} on board {}", FW_VERSION, node::mac_string());

    // Which network? Credentials stored over the serial console win over the
    // ones compiled in. Resolved before the radio comes up, and before the
    // console window below, so provisioning can report what it is replacing.
    // Before anything can overwrite it: why this boot happened. A deep-sleep
    // wake is the steady state and is dropped; anything else is latched in RTC
    // RAM so the next publish can carry it. See `reset_reason` for why this is
    // worth the two numbers.
    state::note_reset(reset_reason::code());
    if state::last_reset() != 0 {
        warn!(
            "last non-routine reset: code 0x{:02X}, {} since power-on",
            state::last_reset(),
            state::reset_count()
        );
    }

    let source = wifi::init(built_in_credentials());
    match wifi::active() {
        Some(credentials) => info!(
            "wifi: '{}' ({})",
            credentials.ssid,
            match source {
                wifi::Source::Stored => "from flash",
                wifi::Source::BuiltIn => "built in",
            }
        ),
        None => warn!("wifi: no credentials"),
    }

    // The escape hatch. Only on a cold boot: a deep-sleep wake skips it, so a
    // battery node pays this once per power-up rather than every two seconds.
    if state::is_cold_boot() {
        console_provisioning(peripherals.USB_DEVICE).await;
    }
    // A `persistent` RTC word is not initialised by the startup code, so a
    // counter that has just been added to the firmware starts at whatever was
    // in that slot. This one fired its stuck-load bound on the first boot after
    // the reflash that introduced it -- harmless, since an arrival resets it,
    // but a legitimate first visit would have been misjudged once. Zero it
    // where a cold boot is already being detected.
    if state::is_cold_boot() {
        state::set_present_rounds(0);
        // Belt and braces only: the visit counter guards itself with a checked
        // pair, because this branch cannot cover the case that actually bit --
        // a reflash keeps RTC RAM, so the first boot carrying a *new* counter
        // is not a cold boot. See `scale::VISITS_MAGIC`.
        state::set_visit_count(0);
    }
    state::mark_booted();

    // Runtime config from flash (calibration + tuning), or defaults on a blank
    // sector. Read now, while the radio is still down. It may be updated from
    // Home Assistant during the publish below and persisted before sleep.
    let cfg = config::load();
    info!(
        "config: offset={} scale={} threshold={}g idle={}s active={}s",
        cfg.offset, cfg.scale_factor, cfg.threshold_grams, cfg.idle_secs, cfg.active_secs
    );

    // --- 2. Sensors --------------------------------------------------------
    // Pin numbers are the raw ESP32-C3 GPIOs; on the Seeed XIAO ESP32-C3 the
    // silkscreen pads map D0=GPIO2, D1=GPIO3, D2=GPIO4 (GPIO0/GPIO1 are *not*
    // broken out). So:
    //   HX711 SCK -> D0 (GPIO2)   HX711 DT -> D1 (GPIO3)   DS18B20 -> D2 (GPIO4)
    // The I²C and UART pins live in `platform.rs` alongside their drivers.
    // `DT` is pulled up so a *disconnected* amp reads permanently "not ready"
    // and times out cleanly instead of returning floating garbage.
    let scale = node.scale.enabled.then(|| {
        // Before the pin is configured, not after: `park_scale` latched this
        // pad on the way into deep sleep, and a latched pad ignores whatever
        // the GPIO peripheral tries to drive. Configuring first and releasing
        // afterwards would leave the driver clocking a line that cannot move.
        release_scale_pad();
        let dt = Input::new(peripherals.GPIO3, Pull::Up);
        let sck = Output::new(peripherals.GPIO2, Level::Low);
        Hx711::new(dt, sck, Delay::new())
    });

    // D2 / GPIO4 has two possible jobs and can only do one of them, so exactly
    // one arm below claims the pin:
    //   * the DS18B20's open-drain 1-Wire line (internal pull-up backing the
    //     external 4.7 kΩ), or
    //   * the battery divider's tap, the only ADC1 pad the HX711 leaves free.
    // `node.rs` fails the build if a node asks for both, so the order here can
    // never silently decide it.
    let (probe, battery) = match (node.ds18b20.enabled, node.battery.enabled) {
        (true, _) => (
            Some(Ds18b20::new(OutputOpenDrain::new(
                peripherals.GPIO4,
                Level::High,
                Pull::Up,
            ))),
            None,
        ),
        (_, true) => (
            None,
            Some(Battery::new(peripherals.ADC1, peripherals.GPIO4)),
        ),
        _ => (None, None),
    };

    let mut board = Board {
        scale,
        probe,
        battery,
        sensors: Sensors::new(platform::Peripherals {
            i2c0: peripherals.I2C0,
            sda: peripherals.GPIO6,
            scl: peripherals.GPIO7,
            uart1: peripherals.UART1,
            uart_rx: peripherals.GPIO5,
            uart_tx: peripherals.GPIO10,
        }),
    };

    let radio = Radio {
        timg1: peripherals.TIMG1,
        rng: peripherals.RNG,
        radio_clk: peripherals.RADIO_CLK,
        wifi: peripherals.WIFI,
    };

    // --- 3. Hand over to the power profile ---------------------------------
    // A node that stays awake never deep-sleeps. Sleeping nodes normally do,
    // but Home Assistant can hold one awake (`config/deep_sleep`) for bench
    // testing on USB, where deep sleep just churns the serial monitor. Both
    // branches diverge, so exactly one of them runs per boot.
    if !node.power.deep_sleeps() || !cfg.deep_sleep {
        run_awake(spawner, radio, peripherals.LPWR, &mut board, cfg).await;
    }

    run_battery(spawner, radio, peripherals.LPWR, &mut board, cfg).await;
}

/// Sleeping profile: one measurement per cold boot, then straight back to deep
/// sleep. Never returns.
///
/// Reached by a battery node and by a duty-cycled mains one. The two differ
/// only in how long the sleep is — see [`publish_interval`] — because from
/// here down the question is the same either way: the node is up, it has one
/// round to do, and then it is gone again.
///
/// The cold boot per poll is the expensive part — ~270 ms of ROM boot and app
/// init to clock out one 100 ms conversion, which at `idle_secs: 2` is about a
/// quarter of the node's life. Replacing it with light sleep was tried and
/// reverted: `Rtc::sleep_light` **resets this chip instead of resuming**,
/// producing a boot loop of roughly one cycle per sleep, with no output past
/// the last line before the first sleep (observed on hardware 2026-09-04).
/// Setting `lslp_mem_inf_fpu` on a hand-built `RtcSleepConfig` — the bit that
/// holds SRAM up, and the one esp-hal's own `finish_sleep` gates its restore
/// path on — did not change it. Whatever else the C3 light-sleep path needs in
/// esp-hal 0.22, that is not all of it.
///
/// Anything attempting this again has to be tested with the load cell slot
/// *enabled*: a node with `scale` off takes the early-out below and never
/// reaches the poll loop at all, which is exactly how the boot loop shipped.
async fn run_battery(
    spawner: Spawner,
    radio: Radio,
    lpwr: LPWR,
    board: &mut Board<'_>,
    cfg: Config,
) -> ! {
    let node = node::active();

    // A node with no load cell has no presence logic to run: sample everything
    // it does have, publish, and go back to sleep.
    if !node.scale.enabled {
        let mut samples = collect_samples(None, None, &cfg, board).await;
        let cfg = publish(spawner, radio, &mut samples, cfg, board).await;
        enter_deep_sleep(lpwr, publish_interval(&node, &cfg));
    }

    let raw = match read_scale(board).await {
        Some(v) => v,
        None => {
            warn!("HX711 not responding; skipping cycle");
            // Still count it toward the heartbeat. Abandoning the cycle here
            // meant a node whose amplifier is absent or dead never reached a
            // publish at all — so it fell silent completely, and nothing in
            // Home Assistant said anything was wrong. It reports in without a
            // weight instead, which is a fault someone can see, and it keeps
            // the other sensors on the board publishing.
            let wakes = state::idle_wakes() + 1;
            if wakes >= cfg.heartbeat_wakes() {
                state::set_idle_wakes(0);
                let mut samples = collect_samples(None, None, &cfg, board).await;
                let cfg = publish(spawner, radio, &mut samples, cfg, board).await;
                enter_deep_sleep(lpwr, cfg.idle_interval());
            }
            state::set_idle_wakes(wakes);
            enter_deep_sleep(lpwr, cfg.idle_interval());
        }
    };
    let drift = drift_ticks(board, &cfg).await;
    let raw = raw.saturating_sub(drift);
    info!(
        "HX711 raw reading: {} (thermal correction {} ticks)",
        raw, drift
    );

    // First boot: establish the tare baseline and go back to sleep.
    if !state::is_initialised() {
        state::set_baseline(raw);
        state::mark_initialised();
        info!("tared baseline = {}", raw);
        enter_deep_sleep(lpwr, cfg.idle_interval());
    }

    // Presence decision. The classification itself lives in `presence`, where
    // it is host-tested; this only decides what to do with each verdict.
    let baseline = state::baseline();
    let was_present = state::bird_present();

    let decision = presence::decide(raw, baseline, was_present, cfg.threshold_ticks());
    // Kept here rather than in each of the five arms: the streak is about the
    // *sequence* of verdicts, not about any one of them, and a reset that has
    // to be repeated in four places is a reset that will be forgotten in the
    // fifth. See `presence::UNEXPLAINED_ADOPT_AFTER_SECS`.
    state::set_unexplained_rounds(match decision {
        presence::Decision::Unexplained { .. } => state::unexplained_rounds().saturating_add(1),
        _ => 0,
    });

    match decision {
        presence::Decision::Arrived { delta } => {
            info!(
                "bird arrived: raw={} baseline={} delta={}",
                raw, baseline, delta
            );
            state::set_bird_present(true);

            // Watch the whole visit with the CPU awake instead of deep-sleeping
            // between samples. This is what turns one arbitrary conversion per
            // active interval into a settled median, and one Wi-Fi connect per
            // active interval into one per visit.
            let visit = watch_visit(board, raw, baseline, drift, &cfg).await;
            state::set_bird_present(visit.still_loaded);

            // Counted here rather than on the rising edge, because the edge
            // does not yet say whether anything stayed: a bounce and a meal
            // look identical until the load is watched. See
            // `presence::MIN_COUNTED_VISIT_MILLIS`.
            //
            // Still counted before the rate limiter below, which was the
            // reason the count used to sit on the edge: the limiter drops
            // *publishes* inside its 60 s window, and a second bird in half a
            // minute is still a second bird.
            if presence::counts_as_visit(visit.millis) {
                state::count_visit();
            } else {
                info!(
                    "{} ms on the cell is under the {} ms a visit has to last; not counted",
                    visit.millis,
                    presence::MIN_COUNTED_VISIT_MILLIS
                );
            }

            // A fresh arrival starts a new stretch, whatever the last one did.
            state::set_present_rounds(0);

            if presence_publish_allowed(&cfg) {
                let mut samples =
                    collect_samples(Some(visit.weight), Some(visit.millis), &cfg, board).await;
                let cfg = publish(spawner, radio, &mut samples, cfg, board).await;
                state::set_idle_wakes(0);

                // A load that outlasted the window drops back to the old cheap
                // cadence, so snow on the cell cannot re-arm the awake path
                // forever.
                enter_deep_sleep(
                    lpwr,
                    if visit.still_loaded {
                        cfg.active_interval()
                    } else {
                        cfg.idle_interval()
                    },
                );
            }

            // Rate-limited: withhold the airtime, not the state — and fall
            // through to the heartbeat tail rather than sleeping here. See the
            // invariant noted at that tail.
            warn!(
                "arrival within {} s of the last publish; holding the radio",
                presence::MIN_PUBLISH_GAP_SECS
            );
        }

        presence::Decision::Staying { delta } => {
            // The load outlasted its awake window, so this is no longer a bird
            // being weighed — just a load being tracked cheaply.
            info!("load still on the scale: raw={} delta={}", raw, delta);
            state::set_present_rounds(state::present_rounds().saturating_add(1));

            if presence_is_stuck(&cfg) {
                warn!(
                    "load has been on the scale for over {} s; not a visitor. \
                     Dropping to the idle cadence — re-tare with the beam at rest \
                     (`tare`, or a power cycle) if this is the baseline and not the weather.",
                    presence::STUCK_AFTER_SECS
                );
                // Absorb it, rather than merely stop believing it.
                //
                // Clearing the flag alone was not enough and the hardware said
                // so: the load was still over the threshold, so the next round
                // read `Arrived`, which reset this counter and spent another
                // 60 s in `watch_visit`. The node oscillated at ~30 publishes
                // an hour instead of the six it should manage — better than the
                // 230 it started at, and still five times too many.
                //
                // A load that has sat there for ten minutes *is* the empty
                // state. `presence::drift_band` refuses to absorb a step this
                // large because it cannot tell a step from a visitor, and it is
                // right not to guess — ten minutes is the evidence it was
                // missing. Absorbing it takes the delta to zero, ends the
                // presence, and repairs exactly the fault that caused this: a
                // tare baseline that no longer matches the mechanics.
                state::set_baseline(raw);
                state::set_bird_present(false);
                state::set_present_rounds(0);
                // No sleep here: from this point the round is an empty one, and
                // falling through is what keeps the heartbeat running. Sleeping
                // here instead made the node mute — see the tail.
            } else if presence_publish_allowed(&cfg) {
                let mut samples = collect_samples(Some(raw), None, &cfg, board).await;
                let cfg = publish(spawner, radio, &mut samples, cfg, board).await;
                state::set_idle_wakes(0);
                enter_deep_sleep(lpwr, cfg.active_interval());
            }
        }

        presence::Decision::Departed { delta } => {
            // Reached only when a visit ended while the node was asleep, i.e.
            // after a `Staying` cycle. Publish one last reading so Home
            // Assistant returns to baseline, then resume idle polling.
            info!(
                "load gone; publishing final reading {} (delta={})",
                raw, delta
            );
            state::set_bird_present(false);
            state::set_present_rounds(0);
            if presence_publish_allowed(&cfg) {
                let mut samples = collect_samples(Some(raw), None, &cfg, board).await;
                let cfg = publish(spawner, radio, &mut samples, cfg, board).await;
                state::set_idle_wakes(0);
                enter_deep_sleep(lpwr, cfg.idle_interval());
            }
            // Suppressed here means Home Assistant keeps the last weight until
            // the heartbeat. Worth it: a scale flapping fast enough to hit this
            // limit publishes departures as often as arrivals, and letting one
            // side through unbounded would bound nothing.
            warn!("departure within the rate limit; the heartbeat will carry it");
        }

        // Steady empty: absorb slow creep into the baseline.
        presence::Decision::Quiet { baseline: drifted } => state::set_baseline(drifted),

        // Something is on the cell that is neither creep nor a visit. The
        // baseline is left alone on purpose (see `presence::drift_band`), and
        // saying so is the point: this used to be absorbed in silence.
        //
        // Left alone *for a while*, though. Freezing with no way out is what
        // stranded this node for thirty-two hours on 2026-09-24; past
        // `UNEXPLAINED_ADOPT_AFTER_SECS` the reading is the new empty state and
        // the baseline has to follow it, or nothing will ever be counted again.
        presence::Decision::Unexplained { delta } if baseline_is_stranded(&cfg) => {
            warn!(
                "{} ticks on the scale, unchanged for over {} s: adopting it as the new \
                 baseline. Something shifted the zero — a calibration, a remount, a load that \
                 slid off — and a frozen baseline would have made every visitor invisible.",
                delta,
                presence::UNEXPLAINED_ADOPT_AFTER_SECS
            );
            state::set_baseline(raw);
            state::set_unexplained_rounds(0);
        }
        presence::Decision::Unexplained { delta } => warn!(
            "{} ticks on the scale: too much for creep, too little for a visit. Baseline left \
             alone; lower `threshold` if a bird this light should count.",
            delta
        ),
    }

    // **Every round that did not publish arrives here**, and that is an
    // invariant rather than a convenience: an empty house, a rate-limited
    // presence edge, and a load ruled stuck all reach this tail. A branch that
    // sleeps on its own instead skips the heartbeat, and a battery node that
    // skips its heartbeat is simply mute — which is exactly what the first
    // version of the rate limiter did to the outdoor node on 2026-09-10. It
    // dropped to the idle cadence and never spoke again; the broker log showed
    // no connection at all, while the node itself looked healthy on serial.
    //
    // Periodic heartbeat: once enough polls have elapsed, bring Wi-Fi up and
    // publish anyway, so Home Assistant keeps a fresh reading. The counter
    // lives in RTC RAM so it survives the deep-sleep cold boots between polls.
    let wakes = state::idle_wakes() + 1;
    if wakes >= cfg.heartbeat_wakes() {
        info!("heartbeat: publishing periodic readings");
        state::set_idle_wakes(0);
        let mut samples = collect_samples(Some(raw), None, &cfg, board).await;
        let cfg = publish(spawner, radio, &mut samples, cfg, board).await;
        enter_deep_sleep(lpwr, cfg.idle_interval());
    }
    state::set_idle_wakes(wakes);

    enter_deep_sleep(lpwr, cfg.idle_interval());
}

/// What watching one visit through produced.
struct Visit {
    /// Settled raw reading: the median over the tail of the visit.
    weight: i32,
    /// How long the load stayed, in milliseconds. A **lower bound** — the
    /// arrival is only known to within one idle interval, because that is how
    /// often the sleeping node looks at the cell.
    millis: u64,
    /// The load was still there when the awake window ran out.
    still_loaded: bool,
}

/// Watch a visit through to its end with the CPU awake and the radio still off.
///
/// Called on the rising edge only. Sampling continuously for the few seconds a
/// bird actually stays is both cheaper and far more informative than
/// cold-booting once per `active_interval`: that path published one arbitrary
/// conversion every 10 s, timed the departure to the same 10 s grid, and paid a
/// Wi-Fi connect for every one of those cycles.
///
/// `first` — the reading that tripped the threshold — is deliberately not part
/// of the median; see [`presence::Window`].
///
/// `drift` is this wake's temperature correction in raw ticks, already
/// subtracted from `first` by the caller. It has to be subtracted here too, from
/// every sample taken inside the loop: the comparison below is against
/// `baseline`, which lives in corrected ticks, and mixing the two spaces would
/// move the departure threshold by however far the mount had expanded.
/// Re-measuring the air per sample would be the wrong kind of exact — a visit
/// lasts seconds, and nothing thermal happens in seconds.
async fn watch_visit(
    board: &mut Board<'_>,
    first: i32,
    baseline: i32,
    drift: i32,
    cfg: &Config,
) -> Visit {
    let threshold = cfg.threshold_ticks();
    let started = Instant::now();
    let settled_at = started + VISIT_SETTLE;
    let deadline = started + VISIT_MAX;

    let scale = match board.scale.as_mut() {
        Some(s) => s,
        // Unreachable on this path — the caller only gets a rising edge from a
        // cell that just answered — but a missing cell must not be a panic.
        None => {
            return Visit {
                weight: first,
                millis: 0,
                still_loaded: false,
            }
        }
    };

    let mut window = presence::Window::new();
    let mut below = 0u8;
    let mut last_loaded = started;
    let mut still_loaded = true;

    while Instant::now() < deadline {
        let Some(raw) = scale.read(HX711_TIMEOUT).await else {
            // A cell that stops answering mid-visit is a fault, not a
            // departure. End the visit and go back to idle polling rather than
            // leaving the node flagged "occupied" indefinitely.
            warn!("HX711 went quiet mid-visit; ending the visit here");
            still_loaded = false;
            break;
        };
        let raw = raw.saturating_sub(drift);

        if raw.saturating_sub(baseline) >= threshold {
            below = 0;
            last_loaded = Instant::now();
            if last_loaded >= settled_at {
                window.push(raw);
            }
        } else {
            below += 1;
            if below >= VISIT_DEPART_SAMPLES {
                still_loaded = false;
                break;
            }
        }
    }

    let millis = last_loaded.duration_since(started).as_millis();
    // An empty window means the visit was shorter than `VISIT_SETTLE`, so the
    // only reading that ever described it is the one that tripped the threshold.
    let weight = window.median().unwrap_or(first);
    info!(
        "visit ended after {} ms: weight={} from {} samples, still_loaded={}",
        millis,
        weight,
        window.len(),
        still_loaded
    );

    Visit {
        weight,
        millis,
        still_loaded,
    }
}

/// Stay-awake loop: bring Wi-Fi up once and keep it, then sample + publish +
/// drain config on a fixed cadence. This is the normal mode for a node on
/// `PowerProfile::Mains` (#17) and the bench-testing mode for any sleeping
/// node with `deep_sleep` off, where it streams to the still-connected serial
/// monitor. Never returns — it either loops forever or, if Home Assistant
/// re-enables deep sleep on a node whose profile allows it, drops into it.
async fn run_awake(
    spawner: Spawner,
    radio: Radio,
    lpwr: LPWR,
    board: &mut Board<'_>,
    mut cfg: Config,
) -> ! {
    let node = node::active();

    let stack = match bring_up_wifi(spawner, radio).await {
        Ok(s) => s,
        Err(e) => {
            warn!("Wi-Fi bring-up failed ({}); falling back to deep sleep", e);
            enter_deep_sleep(lpwr, cfg.idle_interval());
        }
    };

    loop {
        // Track presence/drift when this node has a load cell, so a mains-powered
        // scale behaves like the battery one minus the sleeping.
        let raw = read_scale(board).await;
        // Corrected here rather than at the comparison, exactly as the sleeping
        // path does it: `collect_samples` below converts this very number to
        // grams, and a mains scale that reported a different weight from a
        // battery one in the same weather would be the kind of difference
        // nobody finds for months.
        let drift = drift_ticks(board, &cfg).await;
        let raw = raw.map(|raw| raw.saturating_sub(drift));
        if let Some(raw) = raw {
            if !state::is_initialised() {
                state::set_baseline(raw);
                state::mark_initialised();
                info!("tared baseline = {}", raw);
            }
            // The same classification the battery path uses, so a mains-powered
            // scale behaves identically minus the sleeping. No visit loop here:
            // this profile is awake anyway and publishes every round.
            let baseline = state::baseline();
            let was_present = state::bird_present();
            let decision = presence::decide(raw, baseline, was_present, cfg.threshold_ticks());
            state::set_unexplained_rounds(match decision {
                presence::Decision::Unexplained { .. } => {
                    state::unexplained_rounds().saturating_add(1)
                }
                _ => 0,
            });
            info!(
                "HX711 raw={} baseline={} decision={:?}",
                raw, baseline, decision
            );
            match decision {
                presence::Decision::Arrived { .. } | presence::Decision::Staying { .. } => {
                    state::set_bird_present(true)
                }
                presence::Decision::Departed { .. } => state::set_bird_present(false),
                presence::Decision::Quiet { baseline: drifted } => {
                    state::set_bird_present(false);
                    state::set_baseline(drifted);
                }
                presence::Decision::Unexplained { delta } => {
                    state::set_bird_present(false);
                    if baseline_is_stranded(&cfg) {
                        warn!(
                            "{} ticks on the scale, unchanged for over {} s: adopting it as the \
                             new baseline.",
                            delta,
                            presence::UNEXPLAINED_ADOPT_AFTER_SECS
                        );
                        state::set_baseline(raw);
                        state::set_unexplained_rounds(0);
                    } else {
                        warn!(
                            "{} ticks on the scale: too much for creep, too little for a visit. \
                             Baseline left alone; lower `threshold` if a bird this light should \
                             count.",
                            delta
                        );
                    }
                }
            }
        } else if node.scale.enabled {
            warn!("HX711 not responding");
        }

        let mut samples = collect_samples(raw, None, &cfg, board).await;

        // Publish every cycle for a live view; this also drains retained config.
        // The time sync is its own round trip with its own timeout, outside the
        // publish budget: it is best-effort, and a slow time server should cost
        // this round its timestamps, not its readings.
        let now_ms = sync_time(stack).await;
        // Same bookkeeping as the deep-sleep path: count this round against an
        // image that has not proved itself, and confirm it only once the broker
        // has actually been reached.
        let on_trial = ota_begin_attempt();
        let drained =
            match with_timeout(WIFI_BUDGET, publish_samples(stack, &mut samples, cfg, now_ms)).await
            {
                Ok(Ok(d)) => {
                    if on_trial {
                        ota_confirm();
                    }
                    d
                }
                Ok(Err(e)) => {
                    warn!("publish failed: {}", e);
                    Drained {
                cfg,
                tare: false,
                offer: None,
            }
                }
                Err(_) => {
                    warn!("publish exceeded {:?}", WIFI_BUDGET);
                    Drained {
                cfg,
                tare: false,
                offer: None,
            }
                }
            };
        install_if_offered(stack, &drained).await;

        let updated = if drained.tare {
            retare(board, drained.cfg).await
        } else {
            drained.cfg
        };
        cfg = persist_if_changed(cfg, updated);

        // Honour a live switch back to deep sleep immediately. Only for a node
        // whose profile sleeps at all: one that stays awake has nothing to gain
        // and CO₂/PM continuity to lose. A battery node drops to the idle
        // cadence, where the next wake-up is a cheap load-cell poll; a node
        // without a cell has nothing cheap to do, so it goes straight to its
        // publish interval.
        if node.power.deep_sleeps() && cfg.deep_sleep {
            info!("deep sleep re-enabled — sleeping");
            let interval = if node.scale.enabled {
                cfg.idle_interval()
            } else {
                publish_interval(&node, &cfg)
            };
            enter_deep_sleep(lpwr, interval);
        }

        wait_for_next_round(sample_period_secs(&cfg), board).await;
    }
}

/// Sleep until the next round -- but tick the gas sensor while doing it.
///
/// A node without one simply waits. A node with one cannot: the SGP4x reports a
/// hotplate resistance that is only comparable while the heater keeps running,
/// and Sensirion specifies the index algorithm for a 0.5-10 s interval. Ticking
/// it once a second through the gap is what makes the published index an index;
/// sampling it once a round would produce a plausible-looking number that means
/// nothing. Each step costs about 50 ms of bus time, so a second is spent
/// waiting either way.
///
/// Only reachable from the stay-awake loop, which is where a mains node lives.
/// A battery node is asleep between rounds and could not tick anything, which
/// is the other half of why the gas sensor belongs on mains.
async fn wait_for_next_round(secs: u64, board: &mut Board<'_>) {
    if !board.sensors.has_gas_sensor() {
        Timer::after(Duration::from_secs(secs)).await;
        return;
    }
    for _ in 0..secs {
        board.sensors.step_gas().await;
        Timer::after(GAS_STEP_INTERVAL).await;
    }
}

/// Seconds between rounds in the stay-awake loop: a mains node follows its
/// per-node cadence, a battery node kept awake for bench testing follows the
/// live-tunable idle interval so it still feels like the sleeping one.
/// Whether a presence-driven publish may spend airtime this round.
///
/// The load cell is the only sensor here that can ask for the radio on its own
/// schedule, and on 2026-09-09 it asked for all of it: an uncalibrated cell
/// sitting 21152 ticks over a 4200-tick threshold reported `Staying` every
/// active round for ten hours, at ~230 broker sessions an hour against a
/// design rate of six. It cost nothing that night only because the node was on
/// USB; on the cell it would have been about 28 mAh an hour.
///
/// [`state::idle_wakes`] already counts rounds since the last publish and is
/// reset by every publish, so the limit needs no clock of its own.
fn presence_publish_allowed(cfg: &Config) -> bool {
    let gap = presence::rounds_for(presence::MIN_PUBLISH_GAP_SECS, cfg.idle_secs);
    presence::may_publish(state::idle_wakes(), gap)
}

/// Whether a load has sat there too long to still be a visitor.
///
/// Bounds the other half of the same failure: the rate limit stops a *flapping*
/// scale, this stops a *stuck* one. A bird does not sit on a feeder for ten
/// minutes, so past that it is snow, a twig, or a baseline taken while the beam
/// was being handled — and none of those should hold the node on the expensive
/// cadence.
fn presence_is_stuck(cfg: &Config) -> bool {
    let limit = presence::rounds_for(presence::STUCK_AFTER_SECS, cfg.active_secs);
    state::present_rounds() >= limit
}

/// Whether the baseline has been frozen long enough to be the problem rather
/// than the caution.
///
/// Counted against the *idle* cadence, not the active one: an unexplained
/// reading is by definition not a presence, so the node is polling cheaply
/// while this accumulates. Using `active_secs` here would make the bound depend
/// on a setting the node is not currently using -- and on the terrace, where
/// idle is 5 s and active 10 s, would have doubled a ten-minute wait into
/// twenty for no reason anyone could have found later.
fn baseline_is_stranded(cfg: &Config) -> bool {
    let limit = presence::rounds_for(presence::UNEXPLAINED_ADOPT_AFTER_SECS, cfg.idle_secs);
    state::unexplained_rounds() >= limit
}

/// How long a sleeping node without a load cell stays down between publishes.
///
/// Every wake-up it takes is already a full publish with a Wi-Fi connect in it,
/// so this is the node's whole cadence, and the two profiles want different
/// things from it.
///
/// A battery node uses the *heartbeat* interval, not the idle one.
/// `idle_interval` is the rate the load cell gets polled at — two seconds by
/// default, which is cheap precisely because those wake-ups never touch the
/// radio. Without a cell there is nothing to poll, and sleeping two seconds
/// between full publishes would spend the whole cell on the radio and nothing
/// else.
///
/// A duty-cycled mains node uses `sample_secs` instead: it is not saving a
/// battery, it is staying cool, and it has no reason to report any less often
/// than it did while it was awake. Keeping the number the node already
/// published at means the change leaves no step in the Home Assistant history
/// and needs no new knob to explain.
fn publish_interval(node: &NodeConfig, cfg: &Config) -> CoreDuration {
    if node.power.is_battery() {
        cfg.heartbeat_interval()
    } else {
        CoreDuration::from_secs(node.sample_secs.max(1))
    }
}

fn sample_period_secs(cfg: &Config) -> u64 {
    let node = node::active();
    if node.power.is_battery() {
        cfg.idle_secs.max(1) as u64
    } else {
        node.sample_secs.max(1)
    }
}

/// One settled HX711 reading, or `None` if this node has no load cell or the
/// amp stayed silent.
///
/// The chip is switched off between wakes now (see [`park_scale`]), so this
/// starts from a cold converter rather than a paused one and has to discard
/// [`hx711::SETTLING_READS`] conversions instead of the single one that
/// sufficed while it ran continuously. That is the price of the power-down, and
/// it is worth paying: the amplifier and its bridge draw ~4.5 mA around the
/// clock, about half this node's entire budget, against roughly 0.3 s of extra
/// awake time per wake.
///
/// The discarded conversions are not merely noisy. The converter's digital
/// filter comes up with no history, so an early sample is wrong by an amount
/// nothing downstream could recognise as a settling artefact -- it would arrive
/// in `presence::decide` as a step, and become a phantom visitor or a stranded
/// baseline.
async fn read_scale(board: &mut Board<'_>) -> Option<i32> {
    let scale = board.scale.as_mut()?;
    scale.power_up();
    let mut last = None;
    for _ in 0..hx711::SETTLING_READS {
        last = scale.read(HX711_TIMEOUT).await;
        last?;
    }
    last
}

/// This wake's thermal correction, in raw HX711 ticks.
///
/// Reads the air itself rather than taking a figure from the caller, because the
/// rounds that need it most are the cheap ones where no sensor is sampled at all
/// — the idle wakes that carry the presence logic between publishes. The SHT31
/// costs one I²C transaction against a boot that costs two seconds, so reading
/// it every wake is cheaper than the RTC-RAM cache that would avoid it, and it
/// cannot go stale.
///
/// Skipped entirely — no bus traffic, no measurement — when no coefficient is
/// set, which is every node in the fleet until someone sets one. The correction
/// is opt-in per mount (see [`Config::temp_coeff_centi`]), so it must cost
/// nothing at all on a node that has not opted in.
async fn drift_ticks(board: &mut Board<'_>, cfg: &Config) -> i32 {
    if cfg.temp_coeff_centi == 0 {
        return 0;
    }
    let air = board.sensors.air_temperature_tenths().await;
    if air.is_none() {
        warn!("no air temperature this round; leaving the weight uncorrected");
    }
    cfg.drift_ticks(air)
}

/// Measure everything this node has and format the readings for MQTT.
///
/// Called on publish cycles only, so the DS18B20's 750 ms conversion and the
/// SDS011's 10–30 s fan warm-up never run on the cheap idle polls (the battery
/// ADC is microseconds either way, but it belongs with the rest). `raw` is the
/// load-cell reading already taken by the caller (it drives the presence logic),
/// converted to grams here with the stored calibration.
async fn collect_samples(
    raw: Option<i32>,
    visit_millis: Option<u64>,
    cfg: &Config,
    board: &mut Board<'_>,
) -> Samples {
    let node = node::active();
    let mut samples = Samples::new();

    if let Some(raw) = raw {
        let mut grams = heapless::String::new();
        cfg.write_grams(&mut grams, raw);
        info!("weight = {} g", grams);
        platform::push_sample(&mut samples, node.scale, "weight", grams);
    }

    if let Some(millis) = visit_millis {
        let mut secs = heapless::String::new();
        presence::write_secs(&mut secs, millis);
        info!("visit = {} s", secs);
        platform::push_sample(&mut samples, node.scale, "visit", secs);
    }

    // Published on every round, not only on a visit: the total is a running
    // value and Home Assistant would let it expire between birds otherwise.
    if node.scale.enabled {
        let mut count = heapless::String::new();
        scale::write_visits(&mut count, state::visit_count());
        platform::push_sample(&mut samples, node.scale, "visits", count);
    }

    if let Some(probe) = board.probe.as_mut() {
        match probe.read().await {
            Some(raw_temp) => {
                let mut value = heapless::String::new();
                ds18b20::write_temp_c(&mut value, raw_temp);
                info!("DS18B20 = {} °C", value);
                platform::push_sample(&mut samples, node.ds18b20, "temperature", value);
            }
            None => warn!("DS18B20 not responding; skipping temperature"),
        }
    }

    // Diagnostics, on every node: what the last non-routine reset was, and how
    // many there have been since power-on. Both come straight out of RTC RAM,
    // so this costs no hardware access and works on the cycles where the radio
    // never comes up. Zero means nothing but deep sleep has happened, which is
    // the healthy reading.
    {
        let mut value = heapless::String::new();
        reset_reason::write_code(&mut value, state::last_reset());
        platform::push_sample(&mut samples, node::Slot::on(), "reset_reason", value);

        let mut value = heapless::String::new();
        reset_reason::write_code(&mut value, state::reset_count());
        platform::push_sample(&mut samples, node::Slot::on(), "reset_count", value);
    }

    if let Some(sense) = board.battery.as_mut() {
        match sense.read_millivolts() {
            // Below the plausible floor this is not a discharged cell but an
            // absent one, or a divider that is not there — publishing it would
            // put a convincing "flat battery" in Home Assistant and trip
            // whatever watches for one. Say what it actually means instead.
            Some(mv) if mv < battery::MIN_PLAUSIBLE_CELL_MV => warn!(
                "battery reads {} mV, which is no cell at all — check the divider is fitted \
                 between B+ and GND with its tap on D2, and that a cell is connected",
                mv
            ),
            Some(mv) => {
                let mut value = heapless::String::new();
                battery::write_volts(&mut value, mv);
                info!("battery = {} V", value);
                if mv < battery::LOW_CELL_MV {
                    warn!(
                        "battery below {} mV: the protection board does not cut off until far \
                         lower, so the cell loses capacity from here on",
                        battery::LOW_CELL_MV
                    );
                }
                platform::push_sample(&mut samples, node.battery, "voltage", value);

                // The estimate, beside the measurement. Same branch on
                // purpose: below `MIN_PLAUSIBLE_CELL_MV` neither is published,
                // because a percentage derived from a wiring fault would look
                // far more convincing than the voltage it came from.
                let mut level = heapless::String::new();
                battery::write_percent(&mut level, battery::percent(mv));
                info!("battery = {} %", level);
                platform::push_sample(&mut samples, node.battery, "percent", level);
            }
            None => warn!("battery ADC never finished a conversion; skipping voltage"),
        }
    }

    // Push the live calibration down before sampling, so a slider moved in Home
    // Assistant takes effect on this round rather than the next one. Doing it
    // here rather than at construction means it also survives a config change
    // arriving mid-run: the driver compares against what it last wrote and only
    // touches the bus on a real change.
    board.sensors.set_scd41_offset(cfg.scd41_offset_centi);
    board.sensors.set_sds011_kappa(cfg.sds011_kappa_centi);

    board.sensors.measure_all(&mut samples).await;
    samples
}

/// Persist `new` to flash if it differs from `old`, and return `new`. Flash
/// writes are slow / finite-wear, so we only touch it on an actual change.
fn persist_if_changed(old: Config, new: Config) -> Config {
    if new != old {
        match config::store(&new) {
            Ok(()) => info!("config updated and saved to flash"),
            Err(e) => warn!("config save failed: {}", e),
        }
        // A changed heartbeat used to force a re-announce from here, because
        // `expire_after` is baked into the discovery payload. The announcement
        // digest covers the payloads, so the next connect notices by itself —
        // see `discovery::announcement_tag`.
    }
    new
}

/// What one round with the broker came back with.
///
/// The tare flag rides along rather than being acted on where it is found: the
/// press is noticed deep inside the MQTT drain, which holds a socket and a
/// client and no load cell at all. Re-zeroing means *measuring*, so the flag
/// travels back out to the loop that owns the board. See [`retare`].
struct Drained {
    cfg: Config,
    tare: bool,
    /// An update offer picked up while draining, carried out of the MQTT code
    /// for the same reason `tare` is: acting on it needs the network stack and
    /// a socket of its own, and neither belongs inside the drain loop.
    offer: Option<heapless::String<256>>,
}

/// Bring up Wi-Fi + the network stack, publish `samples`, and pull any retained
/// config from Home Assistant — all bounded by [`WIFI_BUDGET`]. Returns the
/// (possibly HA-updated) config and persists it to flash when it changed. All
/// failures are logged and swallowed: the caller deep-sleeps straight after,
/// which tears down the half-built stack regardless, and an unchanged config is
/// simply returned untouched.
async fn publish(
    spawner: Spawner,
    radio: Radio,
    samples: &mut Samples,
    cfg: Config,
    board: &mut Board<'_>,
) -> Config {
    let drained = match connect_and_publish(spawner, radio, samples, cfg).await {
        Ok(d) => d,
        Err(e) => {
            warn!("publish failed: {}", e);
            Drained {
                cfg,
                tare: false,
                offer: None,
            }
        }
    };
    let updated = if drained.tare {
        retare(board, drained.cfg).await
    } else {
        drained.cfg
    };
    persist_if_changed(cfg, updated)
}

/// Re-zero the scale from a fresh burst of readings.
///
/// Called once, when someone has pressed the button and can see the feeder is
/// empty. It does three things a single reading could not:
///
/// * it **measures**. The old tare adopted `state::baseline()`, the drift-tracked
///   empty-house value — which only moves on a `Quiet` round, i.e. when the
///   reading is already close to it. After the cell is remounted it never is:
///   every round lands in `Unexplained`, the baseline stays where it was, and
///   taring copied that stale number into `offset`, making the weight worse
///   rather than better. Remounting was exactly when someone would press it.
/// * it takes the **median** of [`presence::TARE_SAMPLES`], so a bird landing
///   partway through cannot become the new zero.
/// * it re-anchors the **presence baseline** to the same number. The gram zero
///   and the presence reference describe one physical state, and letting them
///   disagree is what stranded the node in `Unexplained` in the first place.
/// * it records the **air temperature** the zero was taken at, which is what the
///   thermal correction measures drift against. Same argument as the baseline
///   one line up: the zero, the presence reference and the temperature describe
///   one physical state, and the correction means nothing if they were captured
///   at different ones. Note that the samples below are read *uncorrected* on
///   purpose — at the anchor temperature the correction is zero by
///   construction, so applying it here would be both a no-op and a chance to
///   get the sign wrong.
async fn retare(board: &mut Board<'_>, cfg: Config) -> Config {
    let Some(scale) = board.scale.as_mut() else {
        warn!("tare requested on a node with no load cell");
        return cfg;
    };

    // The first conversion after a pause carries the filter's settling, the
    // same reason `read_scale` throws one away.
    let _ = scale.read(HX711_TIMEOUT).await;

    let mut window = presence::Window::new();
    let mut seen: heapless::Vec<i32, { presence::TARE_SAMPLES }> = heapless::Vec::new();
    for _ in 0..presence::TARE_SAMPLES {
        match scale.read(HX711_TIMEOUT).await {
            Some(raw) => {
                window.push(raw);
                let _ = seen.push(raw);
            }
            None => break,
        }
    }

    if seen.len() < presence::TARE_SAMPLES {
        warn!(
            "tare: only {} of {} readings arrived; leaving the zero alone",
            seen.len(),
            presence::TARE_SAMPLES
        );
        return cfg;
    }
    if !presence::tare_spread_ok(&seen) {
        warn!(
            "tare: readings never settled (spread over {} ticks); leaving the zero alone.              Something was on the scale or moving it — try again once it is still.",
            presence::TARE_MAX_SPREAD
        );
        return cfg;
    }
    let Some(zero) = window.median() else {
        return cfg;
    };

    // After the readings, not before: the anchor has to describe the air the
    // zero was actually measured in, and taring takes a few seconds. A sensor
    // that says nothing leaves the previous anchor alone rather than clearing
    // it — a node that has been compensating correctly for weeks should not lose
    // that because the SHT31 missed one transaction, and the new zero is still
    // the better zero.
    let tare_temp_tenths = match board.sensors.air_temperature_tenths().await {
        Some(tenths) => {
            info!("tare: anchored at {} tenths of a degree", tenths);
            tenths
        }
        None => {
            if cfg.temp_coeff_centi != 0 {
                warn!(
                    "tare: no air temperature, so the thermal anchor stays where it was. \
                     The correction now measures drift from the old zero's weather — \
                     tare again once the SHT31 answers."
                );
            }
            cfg.tare_temp_tenths
        }
    };

    info!(
        "tare: zero {} -> {} (median of {}), presence baseline re-anchored",
        cfg.offset,
        zero,
        presence::TARE_SAMPLES
    );
    state::set_baseline(zero);
    state::set_bird_present(false);
    state::set_present_rounds(0);
    Config {
        offset: zero,
        tare_temp_tenths,
        ..cfg
    }
}

/// Initialise esp-wifi (STA + DHCP), spawn the background tasks, and wait for a
/// link + lease. Returns the `'static` network stack. Both the one-shot
/// deep-sleep publish and the stay-awake loop bring Wi-Fi up through here; only
/// one runs per boot, so the `mk_static!` cells are initialised exactly once.
async fn bring_up_wifi(spawner: Spawner, radio: Radio) -> Result<&'static WifiStack, &'static str> {
    // esp-wifi needs its own timer; TIMG0 is already owned by the executor, so
    // hand it TIMG1.
    let mut rng = Rng::new(radio.rng);
    let timg1 = TimerGroup::new(radio.timg1);
    let esp_wifi_ctrl = &*mk_static!(
        EspWifiController<'static>,
        esp_wifi::init(timg1.timer0, rng, radio.radio_clk).map_err(|_| "wifi init")?
    );

    let (wifi_interface, controller) =
        esp_wifi::wifi::new_with_mode(esp_wifi_ctrl, radio.wifi, WifiStaDevice)
            .map_err(|_| "wifi mode")?;

    let net_config = NetConfig::dhcpv4(Default::default());
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    // Four sockets, not three: the MQTT connection, DHCP, DNS -- and the UDP
    // socket `ntp::query` opens for one round trip. smoltcp refuses to add a
    // socket beyond this, so a stack sized for three would have made every
    // time sync fail at `bind` rather than on the wire.
    let stack = &*mk_static!(
        WifiStack,
        Stack::new(
            wifi_interface,
            net_config,
            mk_static!(StackResources<4>, StackResources::<4>::new()),
            seed,
        )
    );

    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(stack)).ok();

    // Wait for the link and a DHCP lease.
    wait_for_network(stack).await;
    Ok(stack)
}

/// Join Wi-Fi (STA + DHCP) and push one round of readings to the broker.
async fn connect_and_publish(
    spawner: Spawner,
    radio: Radio,
    samples: &mut Samples,
    cfg: Config,
) -> Result<Drained, &'static str> {
    // The budget covers the join and the publish, and deliberately stops there.
    // It used to wrap this whole function from the caller, which quietly made
    // an over-the-air update impossible: a download is allowed minutes and a
    // publish twenty seconds, so the outer timeout won every time and every
    // update would have been abandoned mid-write, identically, for ever.
    let (stack, updated, on_trial) = with_timeout(WIFI_BUDGET, async {
        let stack = bring_up_wifi(spawner, radio).await?;

        // Count this round against an image that has not proved itself yet,
        // before the round rather than after it: an image that panics halfway
        // through a publish is exactly what the rollback exists for, and it
        // would never get as far as recording its own attempt.
        let on_trial = ota_begin_attempt();

        let updated = publish_samples(stack, samples, cfg, sync_time(stack).await).await?;
        Ok::<_, &'static str>((stack, updated, on_trial))
    })
    .await
    .map_err(|_| "wifi/publish exceeded its budget")??;

    // Reaching the broker is the proof, and nothing weaker will do. An image
    // with a wrong SSID, a moved broker or a panic after association boots
    // perfectly well and is never heard from again.
    if on_trial {
        ota_confirm();
    }

    // An offer picked up in the round just now, installed where the stack is up
    // and the MQTT socket has been handed back.
    install_if_offered(stack, &updated).await;

    // Give the TCP stack a moment to flush the FIN before we cut power.
    Timer::after(Duration::from_millis(200)).await;
    Ok(updated)
}

/// Install an offer if the round brought one back, and restart into it.
///
/// Shared by both publish paths, and that sharing is the point: a node that
/// stays associated never goes through `connect_and_publish`, so an update that
/// lived only there would have worked on exactly the nodes that are hardest to
/// reach and on none of the ones worth trying it on first.
async fn install_if_offered(stack: &'static WifiStack, drained: &Drained) {
    let Some(offer) = drained.offer.as_deref() else {
        return;
    };
    match run_update(stack, offer).await {
        Ok(seq) => {
            info!("image written and selected (seq {}); restarting into it", seq);
            Timer::after(Duration::from_millis(200)).await;
            software_reset();
        }
        Err(e) => warn!("update not installed: {}", e),
    }
}

/// What this board is, as opposed to what it measures.
///
/// JSON rather than a handful of topics: it is read by a person with
/// `mosquitto_sub`, not by the archiver, and one retained object per node makes
/// `smarthome/+/meta/board` a listing of the whole fleet — MAC, image, address
/// and which slot it is running from, for sleeping nodes too.
fn board_meta(stack: &'static WifiStack) -> Option<heapless::String<224>> {
    use core::fmt::Write as _;

    let node = node::active();
    let active = ota::active(ota::read_otadata());
    let mut p = heapless::String::new();
    write!(
        p,
        r#"{{"mac":"{}","version":"{}","node":"{}","identity":"{}","slot":{},"seq":{}"#,
        node::mac_string(),
        FW_VERSION,
        node.id,
        // Whether this board is running the identity it was flashed with, which
        // is the other half of "why is this node called that".
        if config::load_node_name().is_some() {
            "provisioned"
        } else {
            "built-in"
        },
        active.slot,
        active.seq,
    )
    .ok()?;
    if let Some(config) = stack.config_v4() {
        write!(p, r#","ip":"{}""#, config.address.address()).ok()?;
    }
    p.push('}').ok()?;
    Some(p)
}

/// How long a download gets before it is abandoned. Generous: 740 KB over a
/// link a battery node is paying for, with a flash erase every sector. A round
/// that overruns it costs the update, not the node -- the slot being written is
/// the one that is *not* running.
const OTA_BUDGET: Duration = Duration::from_secs(240);

/// Count a publish attempt against an unconfirmed image, rolling back if it has
/// had its three. Returns whether an image is on trial at all.
fn ota_begin_attempt() -> bool {
    let active = ota::active(ota::read_otadata());
    match ota::on_attempt(ota::load_pending(), active.seq, ota::MAX_ATTEMPTS) {
        ota::Verdict::Nothing => false,
        ota::Verdict::Trying { attempts } => {
            warn!(
                "running an unconfirmed image (attempt {} of {}); it is confirmed by reaching the broker",
                attempts,
                ota::MAX_ATTEMPTS
            );
            if let Err(e) = ota::store_pending(Some(ota::Pending {
                seq: active.seq,
                attempts,
            })) {
                warn!("could not record the attempt: {}", e);
            }
            true
        }
        ota::Verdict::RollBack => {
            warn!(
                "unconfirmed image failed {} attempts; going back to slot {}",
                ota::MAX_ATTEMPTS,
                active.target_slot()
            );
            match ota::roll_back(active) {
                Ok(seq) => info!("selector points back at slot {} (seq {})", active.target_slot(), seq),
                Err(e) => warn!("rollback failed, and this node is now on its own: {}", e),
            }
            software_reset();
            false
        }
    }
}

/// Mark the running image as proved.
fn ota_confirm() {
    match ota::clear_pending() {
        Ok(()) => info!("update confirmed: this image has published a round"),
        Err(e) => warn!("could not clear the trial record: {}", e),
    }
}

/// Fetch an offered image into the slot that is not running, check it, and point
/// the selector at it. Returns the new sequence number; the caller reboots.
///
/// Every failure here leaves the node exactly as it was: the slot being written
/// is the inactive one, and the selector is not touched until the digest has
/// matched.
async fn run_update(stack: &'static WifiStack, offer_json: &str) -> Result<u32, &'static str> {
    let node = node::active();
    let offer =
        ota::parse_offer(offer_json, FW_VERSION, node.id).map_err(ota::OfferError::as_str)?;
    let url = http::parse_url(offer.url).map_err(http::UrlError::as_str)?;
    let active = ota::active(ota::read_otadata());
    let slot = active.target_slot();
    info!(
        "update offered: {} ({} bytes) -> slot {}",
        offer.version, offer.size, slot
    );

    let mut writer = ota::Writer::new(slot, offer.size, ota_staging())?;

    let mut rx_buffer = [0u8; 1536];
    let mut tx_buffer = [0u8; 512];
    let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
    socket.set_timeout(Some(Duration::from_secs(15)));
    socket
        .connect((
            Ipv4Address::new(url.ip[0], url.ip[1], url.ip[2], url.ip[3]),
            url.port,
        ))
        .await
        .map_err(|_| "could not reach the image server")?;

    let mut scratch = [0u8; 512];
    let fetched = with_timeout(
        OTA_BUDGET,
        http::fetch(&mut socket, &url, 0, &mut scratch, |chunk| {
            writer.write(chunk)
        }),
    )
    .await
    .map_err(|_| "download exceeded its budget")??;
    socket.close();

    let slot = writer.finish(&offer.sha256)?;
    info!("{} bytes written to slot {}, digest matches", fetched, slot);
    ota::activate(active, slot)
}

/// What time it is, or `None` if this round will go out unstamped.
///
/// Asked once per publish round rather than once per boot. On a sleeping node
/// those are the same thing; on a mains node they are not, and asking every
/// round is what keeps the timestamps honest — nothing on this board holds a
/// clock between rounds, deliberately (see [`clock`]).
///
/// Never fatal. A node that cannot reach its time server still has readings
/// worth publishing, and an unstamped one is dated on arrival by the archiver,
/// exactly as every reading was before any of this existed.
async fn sync_time(stack: &'static WifiStack) -> Option<u64> {
    match ntp::query(stack, NTP_SERVER).await {
        Ok(millis) if clock::is_plausible(millis) => {
            info!("time synced: {} ms since the epoch", millis);
            Some(millis)
        }
        // A server can answer correctly and still be answering about the wrong
        // century — an era mistake, or a clock nobody ever set. Refused here so
        // it costs a timestamp rather than poisoning the history with one.
        Ok(millis) => {
            warn!("time server gave an implausible {} ms; publishing unstamped", millis);
            None
        }
        Err(why) => {
            warn!("no time sync ({}); publishing unstamped", why);
            None
        }
    }
}

/// Enter RTC-timer deep sleep for `interval`. Never returns — the chip resets
/// on wake and re-runs `main`.
fn enter_deep_sleep(lpwr: LPWR, interval: CoreDuration) -> ! {
    park_scale();
    info!("Entering deep sleep for {:?}", interval);
    let mut rtc = Rtc::new(lpwr);
    let wake = TimerWakeupSource::new(interval);
    rtc.sleep_deep(&[&wake]);
}

/// Put the HX711 to sleep and make the pad keep it there.
///
/// Two halves, and neither works alone. Holding `PD_SCK` high for more than
/// 60 µs latches the chip into its ~0.3 µA power-down -- but on the ESP32-C3 a
/// digital pad loses its level in deep sleep, so the chip would wake up with
/// the MCU and draw its 1.5 mA plus the bridge's 3 mA through every one of the
/// five seconds this node spends asleep. Latching the pad in
/// `RTC_CNTL.PAD_HOLD` is what carries the level across, because that register
/// and the pad it drives live in the RTC domain, which stays powered.
///
/// `SCK` is `GPIO2`, which on this chip is `RTC_GPIO2` -- one of the six pads
/// that can be held at all. That is not a coincidence to rely on quietly: the
/// pin assignment in `main` was made for the HX711's bit-banging, and this
/// optimisation only exists because it happened to land on a holdable pad.
///
/// Stealing the pin is sound *here specifically* because the caller never
/// returns: this runs on the last line before `sleep_deep`, so nothing can
/// observe the duplicate handle. The `Board`'s own `Output` is not reachable
/// from `enter_deep_sleep`, and threading it through twelve call sites to
/// borrow it properly would put the whole mechanism at the mercy of whoever
/// adds the thirteenth.
///
/// The matching release is in `main`, before the pins are configured. Without
/// it the pad stays latched and the driver clocks a line that cannot move --
/// which reads exactly like a dead amplifier.
fn park_scale() {
    if !node::active().scale.enabled {
        return;
    }
    // `Level::High` is the power-down itself: the chip latches once `PD_SCK`
    // has been high for 60 µs. Dropping the handle afterwards changes nothing —
    // `Output` has no `Drop` — and the latch below outlives it regardless.
    let _sck = Output::new(unsafe { GpioPin::<2>::steal() }, Level::High);
    Delay::new().delay_us(hx711::POWER_DOWN_US);
    unsafe { &*LPWR::PTR }
        .pad_hold()
        .modify(|_, w| w.gpio_pin2_hold().set_bit());
}

/// Let go of the pad `park_scale` latched, so the pins can be configured.
///
/// Unconditional and first: a board that has just been flashed with this
/// firmware may still be holding a pad from a previous boot, and a board that
/// never held one loses nothing by clearing a bit that is already clear.
fn release_scale_pad() {
    unsafe { &*LPWR::PTR }
        .pad_hold()
        .modify(|_, w| w.gpio_pin2_hold().clear_bit());
}

/// Block (async) until the interface reports link-up and DHCP has yielded an
/// IPv4 address.
async fn wait_for_network(stack: &'static WifiStack) {
    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }
    info!("Wi-Fi link up, waiting for DHCP...");
    loop {
        if let Some(config) = stack.config_v4() {
            info!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }
}

/// Open a TCP connection to the broker, announce this node to Home Assistant
/// (once per power cycle), publish every reading of this round, then drain any
/// retained `<namespace>/<node>/config/*` values. Returns the config with those
/// updates applied (unchanged if none were waiting).
async fn publish_samples(
    stack: &'static WifiStack,
    samples: &mut Samples,
    cfg: Config,
    now_ms: Option<u64>,
) -> Result<Drained, &'static str> {
    let node = node::active();
    let mut rx_buffer = [0u8; 1536];
    let mut tx_buffer = [0u8; 1536];
    let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
    socket.set_timeout(Some(Duration::from_secs(10)));

    socket
        .connect((MQTT_BROKER, MQTT_PORT))
        .await
        .map_err(|_| "tcp connect")?;

    // Declared before the client config that borrows them, so they outlive it.
    let client_id = node.client_id();
    let availability_topic = node.availability_topic();
    let mut mqtt_config: ClientConfig<'_, 5, _> = ClientConfig::new(
        rust_mqtt::client::client_config::MqttVersion::MQTTv5,
        CountingRng(20_000),
    );
    mqtt_config.add_client_id(&client_id);
    // Last will: if this node drops off without saying goodbye, the broker tells
    // Home Assistant. Only mains nodes register one — a battery node spends most
    // of its life legitimately disconnected (see `NodeConfig::uses_lwt`).
    if node.uses_lwt() {
        mqtt_config.add_will(&availability_topic, discovery::PAYLOAD_OFFLINE, true);
    }
    if let Some(user) = MQTT_USER {
        mqtt_config.add_username(user);
    }
    if let Some(password) = MQTT_PASSWORD {
        mqtt_config.add_password(password);
    }
    // Large enough for a discovery config message (the biggest thing we send).
    mqtt_config.max_packet_size = MQTT_BUFFER as u32;

    let mut recv_buffer = [0u8; MQTT_BUFFER];
    let mut write_buffer = [0u8; MQTT_BUFFER];
    // By reference, so the socket outlives the client: the graceful shutdown at
    // the end of this function needs it back (see there).
    let mut client = MqttClient::new(
        &mut socket,
        &mut write_buffer,
        MQTT_BUFFER,
        &mut recv_buffer,
        MQTT_BUFFER,
        mqtt_config,
    );

    client
        .connect_to_broker()
        .await
        .map_err(|_| "mqtt connect")?;

    // Retract the will's `offline` now that we are back. Retained, so Home
    // Assistant sees the node as available even if it restarts meanwhile.
    if node.uses_lwt() {
        client
            .send_message(
                &availability_topic,
                discovery::PAYLOAD_ONLINE,
                QualityOfService::QoS0,
                true,
            )
            .await
            .map_err(|_| "mqtt availability")?;
    }

    // Which image this is. Retained, four levels deep so the archiver's
    // `<namespace>/+/+` subscription never sees a payload that is not a number.
    let version_topic = node.ota_version_topic();
    if client
        .send_message(
            &version_topic,
            FW_VERSION.as_bytes(),
            QualityOfService::QoS0,
            true,
        )
        .await
        .is_err()
    {
        warn!("could not publish the firmware version");
    }

    // And which *board* this is, which is a different question: the node name
    // can be provisioned from one board to another, the MAC cannot. Two boards
    // publishing under one name showed up as flapping values on 2026-09-17 and
    // took two hours to pin down; here it would have been one `mosquitto_sub`.
    let meta_topic = node.meta_topic();
    if let Some(payload) = board_meta(stack) {
        if client
            .send_message(&meta_topic, payload.as_bytes(), QualityOfService::QoS0, true)
            .await
            .is_err()
        {
            warn!("could not publish the board description");
        }
    }

    // --- Home Assistant discovery (#16) ------------------------------------
    // Retained, so the broker replays it to Home Assistant on its next restart;
    // hence once per power cycle is enough (the flag lives in RTC RAM).
    // Discovery goes out at QoS1, unlike the state topics below.
    //
    // QoS0 is fire-and-forget: `send_message` hands the packet to the socket
    // and returns Ok, and whether the broker ever saw it is not knowable. That
    // is fine for a reading which the next round replaces, and wrong for a
    // retained config which nothing will send again -- because the digest is
    // then stored on the strength of a write, not of an arrival, and the retry
    // that exists for exactly this case never fires.
    //
    // It cost an afternoon on the outdoor node: ten of its fourteen
    // announcements reached the broker, `ok` stayed true, the digest was
    // stored, and `battery_percent`, `scale_factor`, `offset` and `tare` were
    // simply absent with nothing anywhere reporting a failure. QoS1 makes the
    // broker acknowledge each one, so the digest records what the broker has
    // rather than what the socket accepted.
    let availability = discovery::availability(&node, &cfg);
    let announcement = discovery::announcement_tag(&node, &availability);
    if state::discovery_tag() != announcement {
        let mut ok = true;
        for entity in discovery::entities(&node) {
            let topic = discovery::config_topic(&node, &entity);
            let Some(payload) = discovery::config_payload(&node, &entity, &availability) else {
                warn!("discovery payload too long for {}; skipping", topic);
                continue;
            };
            if client
                .send_message(&topic, payload.as_bytes(), QualityOfService::QoS1, true)
                .await
                .is_err()
            {
                ok = false;
                break;
            }
        }
        // The tuning knobs are discovered the same way, as `number` / `switch` /
        // `button` entities pointing back at the config topics we subscribe to
        // below.
        for control in discovery::controls(&node) {
            if !ok {
                break;
            }
            let topic = discovery::control_topic(&node, control);
            let Some(payload) = discovery::control_payload(&node, control, &availability) else {
                warn!("discovery payload too long for {}; skipping", topic);
                continue;
            };
            if client
                .send_message(&topic, payload.as_bytes(), QualityOfService::QoS1, true)
                .await
                .is_err()
            {
                ok = false;
            }
        }
        if ok {
            state::set_discovery_tag(announcement);
            info!("published Home Assistant discovery for node '{}'", node.id);
        } else {
            warn!("discovery publish failed; will retry on the next connect");
        }
    }

    // --- State ---------------------------------------------------------------
    // Taken here rather than in `collect_samples`, which runs before the radio
    // comes up on a battery node: there is no association to measure yet at
    // that point. By now the socket is connected, so the driver's stored value
    // for the AP is the link this round actually published over.
    if let Some(dbm) = rssi::read() {
        let mut value = heapless::String::new();
        rssi::write_dbm(&mut value, dbm);
        info!("rssi = {} dBm", dbm);
        platform::push_sample(samples, node::Slot::on(), "rssi", value);
    }

    // The wall clock reached the node after the samples were taken, so each one
    // is dated by walking back its own age rather than by "now" -- see
    // `clock::stamp`. With no clock this round, every payload simply goes out
    // without a time.
    let published_at = Instant::now();
    for sample in samples.iter() {
        let taken_at = now_ms.map(|now| {
            clock::stamp(
                now,
                published_at.saturating_duration_since(sample.at).as_millis(),
            )
        });
        let topic = node.state_topic(sample.prefix, sample.reading.key);
        let Some(payload) = discovery::state_payload(&sample.reading.value, taken_at) else {
            // Unreachable with a 16-byte value and a 13-digit timestamp, but a
            // truncated payload would be malformed JSON and take the entity
            // down, so it is dropped rather than sent.
            warn!("state payload too long for {}; skipping", topic);
            continue;
        };
        client
            // Retained, which a reading was deliberately not until it could
            // carry its own timestamp. Now that it can, the broker holding the
            // last value per topic is what lets the archiver recover the head
            // of every series after a restart instead of discarding it as
            // undateable. Home Assistant is unaffected: `exp_aft` invalidates a
            // retained value it is too old to believe.
            .send_message(&topic, payload.as_bytes(), QualityOfService::QoS0, true)
            .await
            .map_err(|_| "mqtt publish")?;
        info!("Published {} to {}", payload, topic);

        // Mirror the weight to the pre-discovery topic while the hand-declared
        // Home Assistant entity is still around. Bare and unretained, as it has
        // always been: that entity is declared in the home-server nix config
        // with no template, so JSON would read as `unknown` there.
        if let (Some(legacy), "weight") = (node.legacy_weight_topic, sample.reading.key) {
            client
                .send_message(
                    legacy,
                    sample.reading.value.as_bytes(),
                    QualityOfService::QoS0,
                    false,
                )
                .await
                .map_err(|_| "mqtt publish legacy")?;
        }
    }

    // Pull retained config from Home Assistant, and the retained provisioning
    // message if this board has been told what it is. We're already online, so
    // this is the cheap moment for both. Retained messages arrive right after
    // the SUBACK, so we read with a short per-message window and stop on the
    // first timeout (nothing more waiting), capped by a hard message count as a
    // backstop. Best-effort: a failed sync never fails the publish.
    let mut updated = cfg;
    let mut reprovision = None;
    let mut offer: Option<heapless::String<256>> = None;
    let mut tare_pressed = false;
    let mut reannounce_pressed = false;
    let mut reset_visits_pressed = false;
    let config_prefix = node.config_prefix();
    let provision_topic = node::provision_topic(Efuse::read_base_mac_address());

    // One SUBSCRIBE carrying both filters, emphatically not two in a row.
    // `rust-mqtt`'s `subscribe_to_topic` polls for its own SUBACK and discards
    // whatever else turns up -- "If an application message comes at this
    // moment, it is lost", says the library, and then it returns an error. So
    // subscribing twice fed the *first* subscription's retained config straight
    // into the second call's poll, which dropped it on the floor. Every runtime
    // knob was unreachable that way: tare, scale_factor, deep_sleep and the
    // intervals all sat retained on the broker being thrown away once a round.
    let config_wildcard = node.config_wildcard();
    let ota_offer_topic = node.ota_offer_topic();
    let mut filters = heapless::Vec::<&str, 3>::new();
    let _ = filters.push(config_wildcard.as_str());
    let _ = filters.push(provision_topic.as_str());
    let _ = filters.push(ota_offer_topic.as_str());

    if client.subscribe_to_topics(&filters).await.is_ok() {
        for _ in 0..12 {
            match with_timeout(CONFIG_RECV_WINDOW, client.receive_message()).await {
                Ok(Ok((topic, payload))) => {
                    let Ok(value) = core::str::from_utf8(payload) else {
                        continue;
                    };
                    let value = value.trim();
                    if topic == ota_offer_topic {
                        // Copied out rather than acted on here: installing it
                        // needs a socket of its own, and this one is busy being
                        // an MQTT client. An offer too long to hold is dropped
                        // with a word rather than silently truncated into
                        // something that would fail to parse later.
                        match heapless::String::try_from(value) {
                            Ok(text) => offer = Some(text),
                            Err(()) => warn!("update offer is too long to hold; ignoring it"),
                        }
                    } else if topic == provision_topic {
                        reprovision = node::provision_request(
                            value,
                            &node,
                            config::load_node_name().is_some(),
                        );
                    } else if let Some(key) = topic.strip_prefix(config_prefix.as_str()) {
                        // Tracked separately from `apply`'s "did anything
                        // change" answer: taring an already-zeroed scale changes
                        // nothing, but the press still has to be consumed.
                        // Noticed here rather than inside `apply`, for the same
                        // reason `reset_visits` is: acting on it is not a field
                        // assignment. `apply` owns settings; a re-zero is a
                        // measurement, and it happens once this round is over
                        // and the board is reachable again (see `retare`).
                        if key == TARE_KEY && updated.tare_press_is_new(value) {
                            tare_pressed = true;
                        }

                        // Forget what the broker is believed to hold, so the
                        // next connect announces everything again. The only
                        // way out of a digest that disagrees with the broker;
                        // see `discovery::REANNOUNCE_CONTROLS`.
                        if key == REANNOUNCE_KEY && !value.is_empty() {
                            info!("re-announce requested; clearing the discovery digest");
                            state::set_discovery_tag(0);
                            reannounce_pressed = true;
                        }

                        // Acted on here rather than through `apply`: the count
                        // is not part of the config blob, so there is nothing
                        // for `apply` to change and nothing to persist to
                        // flash. Writing zero through `set_visit_count` also
                        // rewrites the check word, so the next read trusts it.
                        if key == RESET_VISITS_KEY && !value.is_empty() {
                            info!(
                                "visit counter reset requested; {} -> 0",
                                state::visit_count()
                            );
                            state::set_visit_count(0);
                            reset_visits_pressed = true;
                        }
                        if updated.apply(key, value) {
                            info!("config: {} = {}", key, value);
                        }
                    }
                }
                // Broker error/disconnect, or no more retained messages in the
                // window: either way, done draining.
                Ok(Err(_)) | Err(_) => break,
            }
        }
    }

    // Consume the tare press. The button's payload is a constant and the message
    // has to be retained (the node is asleep when it is pressed), so the only
    // thing distinguishing a press from its own echo is whether it is still on
    // the broker: an empty retained payload deletes it. If this fails we simply
    // tare again next time, which on an empty scale lands on the same zero.
    for (pressed, key) in [
        (reannounce_pressed, REANNOUNCE_KEY),
        (reset_visits_pressed, RESET_VISITS_KEY),
    ] {
        if !pressed {
            continue;
        }
        let mut topic = config_prefix.clone();
        if topic.push_str(key).is_ok()
            && client
                .send_message(&topic, &[], QualityOfService::QoS0, true)
                .await
                .is_err()
        {
            warn!("could not clear retained {}; it may be applied again", key);
        }
    }

    if tare_pressed {
        let mut tare_topic = config_prefix.clone();
        if tare_topic.push_str(TARE_KEY).is_ok()
            && client
                .send_message(&tare_topic, &[], QualityOfService::QoS0, true)
                .await
                .is_err()
        {
            warn!("could not clear retained tare; it may be applied again");
        }
    }

    // Say goodbye properly. A DISCONNECT tells the broker to *discard* the will,
    // so a node that simply finished its round is not announced as dead — the
    // will then only fires when the link really breaks.
    let _ = client.disconnect().await;

    // ...but writing it is not sending it. `rust-mqtt` hands the packet to the
    // socket and returns; embassy-net leaves it sitting in the TX buffer until
    // the stack next polls, and `Drop for TcpSocket` just removes the socket
    // from the set, taking anything still queued with it. The broker therefore
    // saw every round end as an abrupt drop, and published the retained will on
    // the next connect — which is the `offline` that flickered immediately
    // before every `online` (observed on `bad` and `schlafzimmer`, 2026-08-26)
    // and made the availability history useless.
    //
    // So: drop the client to get the socket back, flush until the send queue is
    // empty, then close and let the FIN drain. Both waits are bounded — a dead
    // link must not hold the round open, and by this point the readings are
    // already published, so giving up here costs only the tidy shutdown.
    drop(client);
    let _ = with_timeout(SHUTDOWN_BUDGET, socket.flush()).await;
    socket.close();
    let _ = with_timeout(SHUTDOWN_BUDGET, socket.flush()).await;

    // Becoming a different node means rebooting, so this is the last thing we
    // do with the connection. Any tuning picked up in the loop above is dropped
    // by the restart — it is retained on the broker and comes back on the next
    // connect, whereas the identity is what the whole boot depends on.
    if let Some(request) = reprovision {
        apply_provisioning(request);
    }

    Ok(Drained {
        cfg: updated,
        tare: tare_pressed,
        offer,
    })
}

/// Store the new identity and restart into it.
///
/// A reboot rather than an in-place switch: the sensor set decides which buses
/// are initialised, which happens once during boot. Re-doing that at runtime
/// would be a lot of machinery for something that happens once in a board's
/// life. A failed flash write is logged and ignored — the board keeps running as
/// whatever it currently is, and the retained message is still there to be
/// applied on the next connect.
fn apply_provisioning(request: Provision) {
    let stored = match &request {
        Provision::Become(name) => config::store_node_name(name),
        Provision::Reset => config::clear_node_name(),
    };

    match (stored, &request) {
        (Ok(()), Provision::Become(name)) => {
            info!("provisioned as node '{}'; restarting", name);
            software_reset();
        }
        (Ok(()), Provision::Reset) => {
            info!(
                "provisioning cleared; restarting as built-in node '{}'",
                node::BUILT_AS.id
            );
            software_reset();
        }
        (Err(e), _) => warn!("provisioning write failed: {}; staying as is", e),
    }
}

/// MQTT read/write buffer size. Sized for the largest packet the node sends —
/// a Home Assistant discovery config (topic ≤96 B + payload ≤`PAYLOAD_MAX` +
/// MQTT v5 headers) — with room to spare.
const MQTT_BUFFER: usize = 768;

const _: () = assert!(MQTT_BUFFER >= discovery::PAYLOAD_MAX + 96 + 64);

/// The credentials this image was compiled with. The fallback when flash holds
/// none, and the net the connection task reverts to when the stored ones keep
/// being refused.
fn built_in_credentials() -> wifi::Credentials {
    wifi::Credentials::new(SSID, PASSWORD).unwrap_or_else(|| {
        // Only reachable from an image built with an empty or over-long `SSID=`.
        // Naming nothing is better than naming half a network: the console
        // window below is then the way in.
        warn!("built-in credentials do not fit; console provisioning only");
        wifi::Credentials::new(wifi::PLACEHOLDER_SSID, "").expect("placeholder fits")
    })
}

/// Offer the serial console a chance to change the Wi-Fi credentials, and act
/// on it.
///
/// This is the one path that works when the network does not, which is exactly
/// when it is needed — so it deliberately runs before the radio is initialised
/// and costs nothing but the window. A board with no usable credentials waits
/// far longer, since it has nothing else to be doing.
async fn console_provisioning(usb: esp_hal::peripherals::USB_DEVICE) {
    let stranded = wifi::active().map_or(true, |c| c.is_placeholder());
    let window = if stranded {
        warn!("wifi: no usable credentials; waiting for the console");
        CONSOLE_WINDOW_STRANDED
    } else {
        CONSOLE_WINDOW
    };

    let mut console = UsbSerialJtag::new(usb).into_async();
    match wifi::provision(&mut console, window).await {
        wifi::Outcome::Save(credentials) => {
            match config::store_credentials(&credentials.ssid, &credentials.psk) {
                // A restart rather than an in-place switch, for the same reason
                // re-provisioning the node identity reboots: the radio is
                // configured once, on the way up.
                Ok(()) => {
                    info!("wifi: stored '{}'; restarting", credentials.ssid);
                    software_reset();
                }
                Err(e) => warn!("wifi: could not store credentials: {}", e),
            }
        }
        wifi::Outcome::Clear => match config::clear_credentials() {
            Ok(()) => {
                info!("wifi: cleared stored credentials; restarting");
                software_reset();
            }
            Err(e) => warn!("wifi: could not clear credentials: {}", e),
        },
        wifi::Outcome::Continue => {}
    }
}

/// Background task: keeps the Wi-Fi controller connected, reconnecting on drop.
#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    info!("Wi-Fi connection task started");
    // Consecutive refusals of the *stored* credentials, read from RTC RAM rather
    // than started at zero here. A task local looked right and was not: this
    // node cold-boots every few seconds, so the count reset before
    // `FALLBACK_AFTER` could ever be reached and the protection against a
    // mistyped passphrase never fired on the only node that cannot be reflashed
    // casually. `reset_reason::latch` still clears it on a power-on, which is
    // the "give them another try" the original comment meant.
    let mut refusals = state::join_refusals();
    let mut configured: Option<heapless::String<{ config::SSID_MAX }>> = None;

    loop {
        if esp_wifi::wifi::wifi_state() == WifiState::StaConnected {
            // Stay parked until we lose the connection.
            controller.wait_for_event(WifiEvent::StaDisconnected).await;
            Timer::after(Duration::from_millis(5000)).await;
        }

        // The net: stored credentials that keep being refused are set aside for
        // the rest of this run in favour of the ones compiled in. Unlike a wrong
        // node name, a wrong passphrase cannot be corrected over the air — so
        // without this a single typo at the console would take a board off the
        // network until someone walked over with a cable.
        let credentials = match (refusals >= wifi::FALLBACK_AFTER)
            .then(wifi::built_in)
            .flatten()
        {
            Some(fallback) => fallback,
            None => match wifi::active() {
                Some(credentials) => credentials,
                None => {
                    warn!("Wi-Fi: no credentials to try");
                    Timer::after(Duration::from_millis(5000)).await;
                    continue;
                }
            },
        };

        // Re-configure only when the pair actually changed; `set_configuration`
        // on an already-running controller is not free.
        if configured.as_deref() != Some(credentials.ssid.as_str())
            || !matches!(controller.is_started(), Ok(true))
        {
            let client_config = Configuration::Client(ClientConfiguration {
                ssid: credentials.ssid.as_str().try_into().unwrap_or_default(),
                password: credentials.psk.as_str().try_into().unwrap_or_default(),
                ..Default::default()
            });
            controller.set_configuration(&client_config).unwrap();
            configured = Some(credentials.ssid.clone());
            if !matches!(controller.is_started(), Ok(true)) {
                info!("Starting Wi-Fi controller");
                controller.start_async().await.unwrap();
                // Modem sleep, set on every start because it is the radio's
                // own state and a restart is exactly when it would be lost.
                //
                // `Maximum` rather than `Minimum` deliberately. esp-wifi never
                // calls `esp_wifi_set_ps` on its own — the setting only exists
                // if we make it — and the stack underneath already comes up in
                // MIN_MODEM, so asking for `Minimum` would be a no-op dressed
                // up as a change. MAX_MODEM sleeps through `listen_interval`
                // beacons (3 by default, so ~300 ms at a 100 ms beacon) instead
                // of waking for every DTIM.
                //
                // What that costs: up to ~300 ms before an inbound packet is
                // collected from the AP's buffer. Everything reaching these
                // nodes is a Home Assistant knob — a slider, a button — where
                // nobody can tell. Everything time-critical is outbound, and
                // transmitting never waits for the sleep schedule.
                match controller.set_power_saving(PowerSaveMode::Maximum) {
                    Ok(()) => info!("Wi-Fi modem sleep: max"),
                    // Not fatal: it only means the radio idles hotter than it
                    // could. Saying so beats a node that is silently drawing
                    // more than the comment above claims.
                    Err(e) => warn!("Wi-Fi modem sleep refused: {:?}; running without it", e),
                }
            }
        }

        match controller.connect_async().await {
            Ok(_) => {
                info!("Connected to Wi-Fi '{}'", credentials.ssid);
                refusals = 0;
                state::set_join_refusals(0);
            }
            Err(e) => {
                refusals = refusals.saturating_add(1);
                state::set_join_refusals(refusals);
                warn!(
                    "Wi-Fi connect to '{}' failed: {:?} (attempt {}), retrying",
                    credentials.ssid, e, refusals
                );
                if refusals == wifi::FALLBACK_AFTER {
                    warn!("Wi-Fi: falling back to the built-in credentials");
                }
                Timer::after(Duration::from_millis(5000)).await;
            }
        }
    }
}

/// Background task: drives the `embassy-net` stack.
#[embassy_executor::task]
async fn net_task(stack: &'static WifiStack) {
    stack.run().await
}

//! rs-smarthome-nodes — async firmware for a fleet of ESP32-C3 sensor nodes.
//!
//! One image serves every node: which sensors are populated, what the node is
//! called and how it is powered come from [`node`], picked by `NODE=<name>` at
//! build time or by a provisioned identity in flash. It started life as a
//! battery bird-feeder scale, and that node (`draussen`, the default) still
//! drives the flow described below.
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
use embassy_time::{with_timeout, Duration, Timer};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    efuse::Efuse,
    gpio::{Input, Level, Output, OutputOpenDrain, Pull},
    peripherals::{LPWR, RADIO_CLK, RNG, TIMG1, WIFI},
    reset::software_reset,
    rng::Rng,
    rtc_cntl::{sleep::TimerWakeupSource, Rtc},
    timer::timg::TimerGroup,
    usb_serial_jtag::UsbSerialJtag,
};
use esp_wifi::{
    wifi::{
        ClientConfiguration, Configuration, WifiController, WifiDevice, WifiEvent, WifiStaDevice,
        WifiState,
    },
    EspWifiController,
};
use log::{info, warn};
use rust_mqtt::{
    client::{client::MqttClient, client_config::ClientConfig},
    packet::v5::publish_packet::QualityOfService,
    utils::rng_generator::CountingRng,
};

use node::Provision;
use rs_smarthome_nodes::{battery, config, discovery, ds18b20, hx711, node, platform, state, wifi};

use battery::Battery;
use config::Config;
use ds18b20::Ds18b20;
use hx711::Hx711;
use platform::{Samples, Sensors};

/// The concrete network-stack type used throughout the firmware.
type WifiStack = Stack<WifiDevice<'static, WifiStaDevice>>;

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

/// Exponential-decay shift for empty-house baseline drift tracking. Each idle
/// cycle nudges the baseline by `delta >> BASELINE_DRIFT_SHIFT` to absorb slow
/// thermal / mechanical creep without chasing a real load.
const BASELINE_DRIFT_SHIFT: u32 = 4;

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
        c.cpu_clock = CpuClock::max();
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
    let node = node::active();

    info!(
        "node '{}' ({}) booted, {} profile",
        node.id,
        node.name,
        if node.power.is_battery() {
            "battery"
        } else {
            "mains"
        }
    );
    // Say where to reach this board if it needs to be told what it is; the MAC
    // is the only name it is sure of before provisioning.
    info!(
        "provision topic: {}",
        node::provision_topic(Efuse::read_base_mac_address())
    );

    // Which network? Credentials stored over the serial console win over the
    // ones compiled in. Resolved before the radio comes up, and before the
    // console window below, so provisioning can report what it is replacing.
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
    // Mains nodes never deep-sleep. Battery nodes normally do, but Home
    // Assistant can hold one awake (`config/deep_sleep`) for bench testing on
    // USB, where deep sleep just churns the serial monitor. Both branches
    // diverge, so exactly one of them runs per boot.
    if !node.power.is_battery() || !cfg.deep_sleep {
        run_awake(spawner, radio, peripherals.LPWR, &mut board, cfg).await;
    }

    run_battery(spawner, radio, peripherals.LPWR, &mut board, cfg).await;
}

/// Battery profile: one measurement per cold boot, then straight back to deep
/// sleep. Never returns.
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
        let samples = collect_samples(None, &cfg, board).await;
        let cfg = publish(spawner, radio, &samples, state::baseline(), cfg).await;
        enter_deep_sleep(lpwr, cfg.idle_interval());
    }

    let raw = match read_scale(board).await {
        Some(v) => v,
        None => {
            warn!("HX711 not responding; skipping cycle");
            enter_deep_sleep(lpwr, cfg.idle_interval());
        }
    };
    info!("HX711 raw reading: {}", raw);

    // First boot: establish the tare baseline and go back to sleep.
    if !state::is_initialised() {
        state::set_baseline(raw);
        state::mark_initialised();
        info!("tared baseline = {}", raw);
        enter_deep_sleep(lpwr, cfg.idle_interval());
    }

    // Presence decision.
    let baseline = state::baseline();
    let delta = raw - baseline;
    let was_present = state::bird_present();

    if delta >= cfg.threshold_ticks() {
        // A bird is on the scale: publish and keep sampling at the active rate.
        info!(
            "presence: raw={} baseline={} delta={}",
            raw, baseline, delta
        );
        state::set_bird_present(true);
        let samples = collect_samples(Some(raw), &cfg, board).await;
        let cfg = publish(spawner, radio, &samples, baseline, cfg).await;
        state::set_idle_wakes(0);
        enter_deep_sleep(lpwr, cfg.active_interval());
    }

    // Empty house from here on.
    if was_present {
        // Falling edge: the bird just left. Publish one last reading so Home
        // Assistant returns to baseline, then resume idle polling.
        info!("bird left; publishing final reading {}", raw);
        state::set_bird_present(false);
        let samples = collect_samples(Some(raw), &cfg, board).await;
        let cfg = publish(spawner, radio, &samples, baseline, cfg).await;
        state::set_idle_wakes(0);
        enter_deep_sleep(lpwr, cfg.idle_interval());
    } else {
        // Steady empty: absorb slow drift into the baseline.
        state::set_baseline(baseline + (delta >> BASELINE_DRIFT_SHIFT));

        // Periodic heartbeat: once enough empty polls have elapsed, bring Wi-Fi
        // up and publish anyway, so Home Assistant keeps a fresh reading even
        // with no visitor. The counter lives in RTC RAM so it survives the
        // deep-sleep cold boots between polls.
        let wakes = state::idle_wakes() + 1;
        if wakes >= cfg.heartbeat_wakes() {
            info!("heartbeat: publishing periodic readings");
            state::set_idle_wakes(0);
            let samples = collect_samples(Some(raw), &cfg, board).await;
            let cfg = publish(spawner, radio, &samples, baseline, cfg).await;
            enter_deep_sleep(lpwr, cfg.idle_interval());
        }
        state::set_idle_wakes(wakes);
    }

    enter_deep_sleep(lpwr, cfg.idle_interval());
}

/// Stay-awake loop: bring Wi-Fi up once and keep it, then sample + publish +
/// drain config on a fixed cadence. This is the normal mode for mains nodes
/// (#17) and the bench-testing mode for a battery node with `deep_sleep` off,
/// where it streams to the still-connected serial monitor. Never returns — it
/// either loops forever or, if Home Assistant re-enables deep sleep on a battery
/// node, drops into it.
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
        if let Some(raw) = raw {
            if !state::is_initialised() {
                state::set_baseline(raw);
                state::mark_initialised();
                info!("tared baseline = {}", raw);
            }
            let baseline = state::baseline();
            let delta = raw - baseline;
            let present = delta >= cfg.threshold_ticks();
            info!(
                "HX711 raw={} baseline={} delta={} present={}",
                raw, baseline, delta, present
            );
            state::set_bird_present(present);
            if !present {
                state::set_baseline(baseline + (delta >> BASELINE_DRIFT_SHIFT));
            }
        } else if node.scale.enabled {
            warn!("HX711 not responding");
        }

        let baseline = state::baseline();
        let samples = collect_samples(raw, &cfg, board).await;

        // Publish every cycle for a live view; this also drains retained config.
        let updated = match with_timeout(
            WIFI_BUDGET,
            publish_samples(stack, &samples, baseline, cfg),
        )
        .await
        {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                warn!("publish failed: {}", e);
                cfg
            }
            Err(_) => {
                warn!("publish exceeded {:?}", WIFI_BUDGET);
                cfg
            }
        };
        cfg = persist_if_changed(cfg, updated);

        // Honour a live switch back to deep sleep immediately (battery only —
        // a mains node has nothing to gain and CO₂/PM continuity to lose).
        if node.power.is_battery() && cfg.deep_sleep {
            info!("deep sleep re-enabled — sleeping");
            enter_deep_sleep(lpwr, cfg.idle_interval());
        }

        Timer::after(Duration::from_secs(sample_period_secs(&cfg))).await;
    }
}

/// Seconds between rounds in the stay-awake loop: a mains node follows its
/// per-node cadence, a battery node kept awake for bench testing follows the
/// live-tunable idle interval so it still feels like the sleeping one.
fn sample_period_secs(cfg: &Config) -> u64 {
    let node = node::active();
    if node.power.is_battery() {
        cfg.idle_secs.max(1) as u64
    } else {
        node.sample_secs.max(1)
    }
}

/// One clean HX711 reading, or `None` if this node has no load cell or the amp
/// stayed silent. The first sample after power-up settles the internal filter,
/// so it is discarded.
async fn read_scale(board: &mut Board<'_>) -> Option<i32> {
    let scale = board.scale.as_mut()?;
    let _ = scale.read(HX711_TIMEOUT).await;
    scale.read(HX711_TIMEOUT).await
}

/// Measure everything this node has and format the readings for MQTT.
///
/// Called on publish cycles only, so the DS18B20's 750 ms conversion and the
/// SDS011's 10–30 s fan warm-up never run on the cheap idle polls (the battery
/// ADC is microseconds either way, but it belongs with the rest). `raw` is the
/// load-cell reading already taken by the caller (it drives the presence logic),
/// converted to grams here with the stored calibration.
async fn collect_samples(raw: Option<i32>, cfg: &Config, board: &mut Board<'_>) -> Samples {
    let node = node::active();
    let mut samples = Samples::new();

    if let Some(raw) = raw {
        let mut grams = heapless::String::new();
        cfg.write_grams(&mut grams, raw);
        info!("weight = {} g", grams);
        platform::push_sample(&mut samples, node.scale, "weight", grams);
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
        // The discovery payload embeds `expire_after`, derived from the publish
        // cadence — so a changed heartbeat has to be re-announced, or Home
        // Assistant keeps expiring the entities on the old schedule.
        if new.heartbeat_secs != old.heartbeat_secs {
            info!("heartbeat changed; re-announcing discovery on the next connect");
            state::clear_discovery_published();
        }
    }
    new
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
    samples: &Samples,
    baseline: i32,
    cfg: Config,
) -> Config {
    let updated = match with_timeout(
        WIFI_BUDGET,
        connect_and_publish(spawner, radio, samples, baseline, cfg),
    )
    .await
    {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            warn!("publish failed: {}", e);
            cfg
        }
        Err(_) => {
            warn!("Wi-Fi/publish exceeded {:?}, giving up", WIFI_BUDGET);
            cfg
        }
    };
    persist_if_changed(cfg, updated)
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

    let stack = &*mk_static!(
        WifiStack,
        Stack::new(
            wifi_interface,
            net_config,
            mk_static!(StackResources<3>, StackResources::<3>::new()),
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
    samples: &Samples,
    baseline: i32,
    cfg: Config,
) -> Result<Config, &'static str> {
    let stack = bring_up_wifi(spawner, radio).await?;

    let updated = publish_samples(stack, samples, baseline, cfg).await?;

    // Give the TCP stack a moment to flush the FIN before we cut power.
    Timer::after(Duration::from_millis(200)).await;
    Ok(updated)
}

/// Enter RTC-timer deep sleep for `interval`. Never returns — the chip resets
/// on wake and re-runs `main`.
fn enter_deep_sleep(lpwr: LPWR, interval: CoreDuration) -> ! {
    info!("Entering deep sleep for {:?}", interval);
    let mut rtc = Rtc::new(lpwr);
    let wake = TimerWakeupSource::new(interval);
    rtc.sleep_deep(&[&wake]);
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
    samples: &Samples,
    baseline: i32,
    cfg: Config,
) -> Result<Config, &'static str> {
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

    // --- Home Assistant discovery (#16) ------------------------------------
    // Retained, so the broker replays it to Home Assistant on its next restart;
    // hence once per power cycle is enough (the flag lives in RTC RAM).
    if !state::discovery_published() {
        let availability = discovery::availability(&node, &cfg);
        let mut ok = true;
        for entity in discovery::entities(&node) {
            let topic = discovery::config_topic(&node, &entity);
            let Some(payload) = discovery::config_payload(&node, &entity, &availability) else {
                warn!("discovery payload too long for {}; skipping", topic);
                continue;
            };
            if client
                .send_message(&topic, payload.as_bytes(), QualityOfService::QoS0, true)
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
                .send_message(&topic, payload.as_bytes(), QualityOfService::QoS0, true)
                .await
                .is_err()
            {
                ok = false;
            }
        }
        if ok {
            state::mark_discovery_published();
            info!("published Home Assistant discovery for node '{}'", node.id);
        } else {
            warn!("discovery publish failed; will retry on the next connect");
        }
    }

    // --- State ---------------------------------------------------------------
    for sample in samples {
        let topic = node.state_topic(sample.prefix, sample.reading.key);
        client
            .send_message(
                &topic,
                sample.reading.value.as_bytes(),
                QualityOfService::QoS0,
                false,
            )
            .await
            .map_err(|_| "mqtt publish")?;
        info!("Published {} to {}", sample.reading.value, topic);

        // Mirror the weight to the pre-discovery topic while the hand-declared
        // Home Assistant entity is still around.
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
    let mut tare_pressed = false;
    let config_prefix = node.config_prefix();
    let provision_topic = node::provision_topic(Efuse::read_base_mac_address());

    // Two separate `let`s so neither subscription can be short-circuited away;
    // either one succeeding is reason enough to drain.
    let config_sub = client
        .subscribe_to_topic(&node.config_wildcard())
        .await
        .is_ok();
    let provision_sub = client.subscribe_to_topic(&provision_topic).await.is_ok();

    if config_sub || provision_sub {
        for _ in 0..12 {
            match with_timeout(CONFIG_RECV_WINDOW, client.receive_message()).await {
                Ok(Ok((topic, payload))) => {
                    let Ok(value) = core::str::from_utf8(payload) else {
                        continue;
                    };
                    let value = value.trim();
                    if topic == provision_topic {
                        reprovision = node::provision_request(
                            value,
                            &node,
                            config::load_node_name().is_some(),
                        );
                    } else if let Some(key) = topic.strip_prefix(config_prefix.as_str()) {
                        // Tracked separately from `apply`'s "did anything
                        // change" answer: taring an already-zeroed scale changes
                        // nothing, but the press still has to be consumed.
                        tare_pressed |= key == TARE_KEY && !value.is_empty();
                        if updated.apply(key, value, baseline) {
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

    Ok(updated)
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
const MQTT_BUFFER: usize = 640;

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
    // Consecutive refusals of the *stored* credentials. Kept here rather than in
    // RTC RAM on purpose: a power cycle should give them another try, since the
    // likeliest reason for a run of failures is an access point that was down,
    // not a passphrase that changed under us.
    let mut refusals = 0u32;
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
            }
        }

        match controller.connect_async().await {
            Ok(_) => {
                info!("Connected to Wi-Fi '{}'", credentials.ssid);
                refusals = 0;
            }
            Err(e) => {
                refusals = refusals.saturating_add(1);
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

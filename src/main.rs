//! rs-bird-scale — async battery bird-feeder scale firmware.
//!
//! To catch short bird visits without keeping the radio awake, the firmware
//! polls by cold-booting out of deep sleep on a short interval and only spends
//! Wi-Fi energy when weight is actually on the scale.
//!
//! Flow on each wake-up:
//!   1. Bring up the HAL + Embassy executor (TIMG0).
//!   2. Read a raw weight sample from the HX711 (with a timeout, so a missing
//!      sensor can't wedge the boot).
//!   3. Compare against the tare baseline persisted in RTC RAM across sleep:
//!        - empty house  -> drift-correct the baseline, skip Wi-Fi, deep-sleep
//!          a short *idle* interval to catch the next visit;
//!        - weight present -> join Wi-Fi (STA + DHCP), publish the raw `i32`
//!          over MQTT to `birds/scale/state`, deep-sleep a longer *active*
//!          interval to keep tracking. A final reading is published on the
//!          falling edge when the bird leaves.
//!
//! Calibration (tare offset + ticks-per-gram) is intentionally *not* done on
//! the MCU; it lives in the Home Assistant sensor template so it can be tuned
//! without reflashing. See `README.md`.

#![no_std]
#![no_main]

mod config;
mod ds18b20;
mod hx711;
// Scaffolding for the configurable multi-sensor base platform (epic #11). Not
// wired into the bird-scale flow yet; see `docs/base-platform.md`.
mod sensors;
mod state;

use core::time::Duration as CoreDuration;

use embassy_executor::Spawner;
use embassy_net::{tcp::TcpSocket, Config as NetConfig, Ipv4Address, Stack, StackResources};
use embassy_time::{with_timeout, Duration, Timer};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, Level, Output, OutputOpenDrain, Pull},
    peripherals::{LPWR, RADIO_CLK, RNG, TIMG1, WIFI},
    rng::Rng,
    rtc_cntl::{sleep::TimerWakeupSource, Rtc},
    timer::timg::TimerGroup,
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

use config::Config;
use ds18b20::Ds18b20;
use hx711::Hx711;

/// The concrete network-stack type used throughout the firmware.
type WifiStack = Stack<WifiDevice<'static, WifiStaDevice>>;

// --- Compile-time configuration --------------------------------------------
// Override the credentials at build time, e.g.:
//   SSID=MyNet PASSWORD=hunter2 cargo run --release
// The MQTT broker address is edited here directly.
const SSID: &str = match option_env!("SSID") {
    Some(s) => s,
    None => "your-ssid",
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
const MQTT_TOPIC: &str = "birds/scale/state";
/// Ambient temperature from the DS18B20, published alongside a weight reading.
const MQTT_TOPIC_TEMP: &str = "birds/scale/temperature";
/// Prefix under which Home Assistant publishes retained tuning/calibration
/// values (one key per topic); the firmware subscribes to the wildcard and
/// reads them while it is already online for a publish. See [`config`].
const MQTT_CONFIG_PREFIX: &str = "birds/scale/config/";
const MQTT_CONFIG_WILDCARD: &str = "birds/scale/config/#";
const MQTT_CLIENT_ID: &str = "rs-bird-scale";

// --- Sampling / detection tuning -------------------------------------------
// The presence threshold and the idle/active poll intervals are no longer
// compile-time constants: they live in [`config::Config`], persisted in flash
// and tunable live from Home Assistant. See `src/config.rs`.

/// How long to wait for the next retained config message after subscribing.
/// Retained values arrive within tens of ms, so once a receive hits this
/// timeout we assume the broker has sent them all and stop draining.
const CONFIG_RECV_WINDOW: Duration = Duration::from_millis(400);

/// Give up on a single HX711 conversion after this long. A disconnected sensor
/// (with `DT` pulled up) never becomes ready, so this bounds the boot.
const HX711_TIMEOUT: Duration = Duration::from_millis(500);

/// Upper bound on the whole Wi-Fi join + MQTT publish. Without it a failed join
/// would spin in the high-power state and drain the battery.
const WIFI_BUDGET: Duration = Duration::from_secs(20);

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

    info!("rs-bird-scale booted, taking a measurement");

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
    // `DT` is pulled up so a *disconnected* amp reads permanently "not ready"
    // and times out cleanly instead of returning floating garbage.
    let dt = Input::new(peripherals.GPIO3, Pull::Up);
    let sck = Output::new(peripherals.GPIO2, Level::Low);
    let mut scale = Hx711::new(dt, sck);

    // DS18B20 open-drain 1-Wire line on D2 / GPIO4 (internal pull-up backs the
    // external 4.7 kΩ). Built once so both modes can reuse it.
    let ds_io = OutputOpenDrain::new(peripherals.GPIO4, Level::High, Pull::Up);
    let mut temp_sensor = Ds18b20::new(ds_io);

    // --- 3. Stay-awake mode (deep sleep disabled) --------------------------
    // Bench testing on USB: keep Wi-Fi up and loop in place so the serial
    // monitor stays connected and readings stream continuously. Toggled live
    // from Home Assistant via `birds/scale/config/deep_sleep`. `run_awake`
    // diverges, so on the normal (deep-sleep) path the peripherals below are
    // untouched and reused.
    if !cfg.deep_sleep {
        info!("deep sleep disabled — staying awake");
        run_awake(
            spawner,
            peripherals.TIMG1,
            peripherals.RNG,
            peripherals.RADIO_CLK,
            peripherals.WIFI,
            peripherals.LPWR,
            &mut scale,
            &mut temp_sensor,
            cfg,
        )
        .await;
    }

    // --- 4. Deep-sleep cycle: one measurement, then back to sleep ----------
    // The first sample after power-up settles the internal filter; throw it
    // away and take a clean reading.
    let _ = scale.read(HX711_TIMEOUT).await;
    let raw = match scale.read(HX711_TIMEOUT).await {
        Some(v) => v,
        None => {
            warn!("HX711 not responding; skipping cycle");
            enter_deep_sleep(peripherals.LPWR, cfg.idle_interval());
        }
    };
    info!("HX711 raw reading: {}", raw);

    // First boot: establish the tare baseline and go back to sleep.
    if !state::is_initialised() {
        state::set_baseline(raw);
        state::mark_initialised();
        info!("tared baseline = {}", raw);
        enter_deep_sleep(peripherals.LPWR, cfg.idle_interval());
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
        let temp = read_temperature(&mut temp_sensor).await;
        let cfg = publish(
            spawner,
            peripherals.TIMG1,
            peripherals.RNG,
            peripherals.RADIO_CLK,
            peripherals.WIFI,
            raw,
            temp,
            baseline,
            cfg,
        )
        .await;
        state::set_idle_wakes(0);
        enter_deep_sleep(peripherals.LPWR, cfg.active_interval());
    }

    // Empty house from here on.
    if was_present {
        // Falling edge: the bird just left. Publish one last reading so Home
        // Assistant returns to baseline, then resume idle polling.
        info!("bird left; publishing final reading {}", raw);
        state::set_bird_present(false);
        let temp = read_temperature(&mut temp_sensor).await;
        let cfg = publish(
            spawner,
            peripherals.TIMG1,
            peripherals.RNG,
            peripherals.RADIO_CLK,
            peripherals.WIFI,
            raw,
            temp,
            baseline,
            cfg,
        )
        .await;
        state::set_idle_wakes(0);
        enter_deep_sleep(peripherals.LPWR, cfg.idle_interval());
    } else {
        // Steady empty: absorb slow drift into the baseline.
        state::set_baseline(baseline + (delta >> BASELINE_DRIFT_SHIFT));

        // Periodic heartbeat: once enough empty polls have elapsed, bring Wi-Fi
        // up and publish temperature + weight anyway, so Home Assistant keeps a
        // fresh reading even with no visitor. The counter lives in RTC RAM so it
        // survives the deep-sleep cold boots between polls.
        let wakes = state::idle_wakes() + 1;
        if wakes >= cfg.heartbeat_wakes() {
            info!("heartbeat: publishing periodic temperature + weight");
            state::set_idle_wakes(0);
            let temp = read_temperature(&mut temp_sensor).await;
            let cfg = publish(
                spawner,
                peripherals.TIMG1,
                peripherals.RNG,
                peripherals.RADIO_CLK,
                peripherals.WIFI,
                raw,
                temp,
                baseline,
                cfg,
            )
            .await;
            enter_deep_sleep(peripherals.LPWR, cfg.idle_interval());
        }
        state::set_idle_wakes(wakes);
    }

    enter_deep_sleep(peripherals.LPWR, cfg.idle_interval());
}

/// Stay-awake loop for bench testing (deep sleep disabled): bring Wi-Fi up once
/// and keep it, then measure + publish + drain config on a fixed cadence,
/// streaming to the still-connected serial monitor. Never returns — it either
/// loops forever or, if Home Assistant re-enables deep sleep, drops into it.
#[allow(clippy::too_many_arguments)]
async fn run_awake(
    spawner: Spawner,
    timg1: TIMG1,
    rng: RNG,
    radio_clk: RADIO_CLK,
    wifi: WIFI,
    lpwr: LPWR,
    scale: &mut Hx711<'_>,
    temp_sensor: &mut Ds18b20<'_>,
    mut cfg: Config,
) -> ! {
    let stack = match bring_up_wifi(spawner, timg1, rng, radio_clk, wifi).await {
        Ok(s) => s,
        Err(e) => {
            warn!("Wi-Fi bring-up failed ({}); falling back to deep sleep", e);
            enter_deep_sleep(lpwr, cfg.idle_interval());
        }
    };

    loop {
        // Discard the settling sample, then take a clean reading.
        let _ = scale.read(HX711_TIMEOUT).await;
        let raw = match scale.read(HX711_TIMEOUT).await {
            Some(v) => v,
            None => {
                warn!("HX711 not responding");
                Timer::after(Duration::from_secs(cfg.idle_secs.max(1) as u64)).await;
                continue;
            }
        };

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

        let temp = read_temperature(temp_sensor).await;

        // Publish every cycle for a live view; this also drains retained config.
        let updated = match with_timeout(
            WIFI_BUDGET,
            publish_reading(stack, raw, temp, baseline, cfg),
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

        // Honour a live switch back to deep sleep immediately.
        if cfg.deep_sleep {
            info!("deep sleep re-enabled — sleeping");
            enter_deep_sleep(lpwr, cfg.idle_interval());
        }

        // Track presence edge + absorb drift, mirroring the deep-sleep path.
        state::set_bird_present(present);
        if !present {
            state::set_baseline(baseline + (delta >> BASELINE_DRIFT_SHIFT));
        }

        Timer::after(Duration::from_secs(cfg.idle_secs.max(1) as u64)).await;
    }
}

/// Read the DS18B20 once. Only called on the publish paths (bird present /
/// just-left, or each awake-loop iteration), so the ~750 ms conversion never
/// runs on the cheap deep-sleep idle-poll cycles. A missing/faulty probe yields
/// `None` and simply omits the temperature from the publish.
async fn read_temperature(sensor: &mut Ds18b20<'_>) -> Option<i16> {
    let temp = sensor.read().await;
    match temp {
        Some(raw) => info!("DS18B20 raw reading: {}", raw),
        None => warn!("DS18B20 not responding; skipping temperature"),
    }
    temp
}

/// Persist `new` to flash if it differs from `old`, and return `new`. Flash
/// writes are slow / finite-wear, so we only touch it on an actual change.
fn persist_if_changed(old: Config, new: Config) -> Config {
    if new != old {
        match config::store(&new) {
            Ok(()) => info!("config updated and saved to flash"),
            Err(e) => warn!("config save failed: {}", e),
        }
    }
    new
}

/// Bring up Wi-Fi + the network stack, publish `raw` (as grams) and the
/// temperature, and pull any retained config from Home Assistant — all bounded
/// by [`WIFI_BUDGET`]. Returns the (possibly HA-updated) config and persists it
/// to flash when it changed. All failures are logged and swallowed: the caller
/// deep-sleeps straight after, which tears down the half-built stack regardless,
/// and an unchanged config is simply returned untouched.
#[allow(clippy::too_many_arguments)]
async fn publish(
    spawner: Spawner,
    timg1: TIMG1,
    rng: RNG,
    radio_clk: RADIO_CLK,
    wifi: WIFI,
    raw: i32,
    temp: Option<i16>,
    baseline: i32,
    cfg: Config,
) -> Config {
    let updated = match with_timeout(
        WIFI_BUDGET,
        connect_and_publish(
            spawner, timg1, rng, radio_clk, wifi, raw, temp, baseline, cfg,
        ),
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
async fn bring_up_wifi(
    spawner: Spawner,
    timg1: TIMG1,
    rng: RNG,
    radio_clk: RADIO_CLK,
    wifi: WIFI,
) -> Result<&'static WifiStack, &'static str> {
    // esp-wifi needs its own timer; TIMG0 is already owned by the executor, so
    // hand it TIMG1.
    let mut rng = Rng::new(rng);
    let timg1 = TimerGroup::new(timg1);
    let esp_wifi_ctrl = &*mk_static!(
        EspWifiController<'static>,
        esp_wifi::init(timg1.timer0, rng, radio_clk).map_err(|_| "wifi init")?
    );

    let (wifi_interface, controller) =
        esp_wifi::wifi::new_with_mode(esp_wifi_ctrl, wifi, WifiStaDevice)
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

/// Join Wi-Fi (STA + DHCP) and push a single reading to the broker.
#[allow(clippy::too_many_arguments)]
async fn connect_and_publish(
    spawner: Spawner,
    timg1: TIMG1,
    rng: RNG,
    radio_clk: RADIO_CLK,
    wifi: WIFI,
    raw: i32,
    temp: Option<i16>,
    baseline: i32,
    cfg: Config,
) -> Result<Config, &'static str> {
    let stack = bring_up_wifi(spawner, timg1, rng, radio_clk, wifi).await?;

    let updated = publish_reading(stack, raw, temp, baseline, cfg).await?;

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

/// Open a TCP connection to the broker, publish the weight in grams (converted
/// on-device from `cfg`) plus the DS18B20 temperature, then drain any retained
/// `birds/scale/config/*` values from Home Assistant. Returns the config with
/// those updates applied (unchanged if none were waiting).
async fn publish_reading(
    stack: &'static WifiStack,
    raw: i32,
    temp: Option<i16>,
    baseline: i32,
    cfg: Config,
) -> Result<Config, &'static str> {
    let mut rx_buffer = [0u8; 1536];
    let mut tx_buffer = [0u8; 1536];
    let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
    socket.set_timeout(Some(Duration::from_secs(10)));

    socket
        .connect((MQTT_BROKER, MQTT_PORT))
        .await
        .map_err(|_| "tcp connect")?;

    let mut mqtt_config: ClientConfig<'_, 5, _> = ClientConfig::new(
        rust_mqtt::client::client_config::MqttVersion::MQTTv5,
        CountingRng(20_000),
    );
    mqtt_config.add_client_id(MQTT_CLIENT_ID);
    if let Some(user) = MQTT_USER {
        mqtt_config.add_username(user);
    }
    if let Some(password) = MQTT_PASSWORD {
        mqtt_config.add_password(password);
    }
    mqtt_config.max_packet_size = 100;

    let mut recv_buffer = [0u8; 256];
    let mut write_buffer = [0u8; 256];
    let mut client = MqttClient::new(
        socket,
        &mut write_buffer,
        256,
        &mut recv_buffer,
        256,
        mqtt_config,
    );

    client
        .connect_to_broker()
        .await
        .map_err(|_| "mqtt connect")?;

    // Convert the raw reading to grams on-device and publish as ASCII.
    let mut payload = heapless::String::<16>::new();
    cfg.write_grams(&mut payload, raw);
    client
        .send_message(
            MQTT_TOPIC,
            payload.as_bytes(),
            QualityOfService::QoS0,
            false,
        )
        .await
        .map_err(|_| "mqtt publish")?;
    info!("Published {} g to {}", payload, MQTT_TOPIC);

    // Publish the temperature (°C, one decimal) when the probe answered.
    if let Some(raw_temp) = temp {
        let mut temp_payload = heapless::String::<16>::new();
        ds18b20::write_temp_c(&mut temp_payload, raw_temp);
        client
            .send_message(
                MQTT_TOPIC_TEMP,
                temp_payload.as_bytes(),
                QualityOfService::QoS0,
                false,
            )
            .await
            .map_err(|_| "mqtt publish temp")?;
        info!("Published {} to {}", temp_payload, MQTT_TOPIC_TEMP);
    }

    // Pull retained config from Home Assistant. We're already online, so this is
    // the cheap moment to pick up any tuning/calibration the user changed.
    // Retained messages arrive right after the SUBACK, so we read with a short
    // per-message window and stop on the first timeout (nothing more waiting),
    // capped by a hard message count as a backstop. Best-effort: config sync
    // failures never fail the publish.
    let mut updated = cfg;
    if client
        .subscribe_to_topic(MQTT_CONFIG_WILDCARD)
        .await
        .is_ok()
    {
        for _ in 0..12 {
            match with_timeout(CONFIG_RECV_WINDOW, client.receive_message()).await {
                Ok(Ok((topic, payload))) => {
                    if let (Some(key), Ok(value)) = (
                        topic.strip_prefix(MQTT_CONFIG_PREFIX),
                        core::str::from_utf8(payload),
                    ) {
                        if updated.apply(key, value.trim(), baseline) {
                            info!("config: {} = {}", key, value.trim());
                        }
                    }
                }
                // Broker error/disconnect, or no more retained messages in the
                // window: either way, done draining.
                Ok(Err(_)) | Err(_) => break,
            }
        }
    }

    Ok(updated)
}

/// Background task: keeps the Wi-Fi controller connected, reconnecting on drop.
#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    info!("Wi-Fi connection task started");
    loop {
        if esp_wifi::wifi::wifi_state() == WifiState::StaConnected {
            // Stay parked until we lose the connection.
            controller.wait_for_event(WifiEvent::StaDisconnected).await;
            Timer::after(Duration::from_millis(5000)).await;
        }

        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = Configuration::Client(ClientConfiguration {
                ssid: SSID.try_into().unwrap(),
                password: PASSWORD.try_into().unwrap(),
                ..Default::default()
            });
            controller.set_configuration(&client_config).unwrap();
            info!("Starting Wi-Fi controller");
            controller.start_async().await.unwrap();
        }

        match controller.connect_async().await {
            Ok(_) => info!("Connected to Wi-Fi '{}'", SSID),
            Err(e) => {
                warn!("Wi-Fi connect failed: {:?}, retrying", e);
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

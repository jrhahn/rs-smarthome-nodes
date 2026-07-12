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

mod hx711;
mod state;

use core::time::Duration as CoreDuration;

use embassy_executor::Spawner;
use embassy_net::{tcp::TcpSocket, Config as NetConfig, Ipv4Address, Stack, StackResources};
use embassy_time::{with_timeout, Duration, Timer};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, Level, Output, Pull},
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

/// Home Assistant / Mosquitto broker on the LAN.
const MQTT_BROKER: Ipv4Address = Ipv4Address::new(192, 168, 1, 10);
const MQTT_PORT: u16 = 1883;
const MQTT_TOPIC: &str = "birds/scale/state";
const MQTT_CLIENT_ID: &str = "rs-bird-scale";

// --- Sampling / detection tuning -------------------------------------------
/// Weight change from the baseline, in raw HX711 ticks, that counts as "a bird
/// landed". Positive = the load cell reads *up* under load; flip the comparison
/// if yours is wired the other way. Calibrate against a known mass.
const PRESENCE_THRESHOLD: i32 = 50_000;

/// How long to deep-sleep between polls while the house is empty. Short enough
/// to catch a brief visit; this is the dominant term in idle battery life.
const IDLE_POLL_INTERVAL: CoreDuration = CoreDuration::from_secs(2);

/// How long to deep-sleep between publishes while a bird is present, i.e. the
/// sample cadence Home Assistant sees during a visit.
const ACTIVE_POLL_INTERVAL: CoreDuration = CoreDuration::from_secs(10);

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
    let config = {
        let mut c = esp_hal::Config::default();
        c.cpu_clock = CpuClock::max();
        c
    };
    let peripherals = esp_hal::init(config);

    esp_println::logger::init_logger_from_env();
    esp_alloc::heap_allocator!(72 * 1024);

    // TIMG0 drives the global Embassy executor (per the hardware spec).
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_hal_embassy::init(timg0.timer0);

    info!("rs-bird-scale booted, taking a measurement");

    // --- 2. Read the load cell ---------------------------------------------
    // `DT` is pulled up so a *disconnected* HX711 reads permanently "not ready"
    // and times out cleanly, rather than returning floating garbage.
    let dt = Input::new(peripherals.GPIO1, Pull::Up);
    let sck = Output::new(peripherals.GPIO0, Level::Low);
    let mut scale = Hx711::new(dt, sck);

    // The first sample after power-up settles the internal filter; throw it
    // away and take a clean reading.
    let _ = scale.read(HX711_TIMEOUT).await;
    let raw = match scale.read(HX711_TIMEOUT).await {
        Some(v) => v,
        None => {
            warn!("HX711 not responding; skipping cycle");
            enter_deep_sleep(peripherals.LPWR, IDLE_POLL_INTERVAL);
        }
    };
    info!("HX711 raw reading: {}", raw);

    // --- 3. First boot: establish the tare baseline and go back to sleep ----
    if !state::is_initialised() {
        state::set_baseline(raw);
        state::mark_initialised();
        info!("tared baseline = {}", raw);
        enter_deep_sleep(peripherals.LPWR, IDLE_POLL_INTERVAL);
    }

    // --- 4. Presence decision ----------------------------------------------
    let baseline = state::baseline();
    let delta = raw - baseline;
    let was_present = state::bird_present();

    if delta >= PRESENCE_THRESHOLD {
        // A bird is on the scale: publish and keep sampling at the active rate.
        info!("presence: raw={} baseline={} delta={}", raw, baseline, delta);
        state::set_bird_present(true);
        publish(
            spawner,
            peripherals.TIMG1,
            peripherals.RNG,
            peripherals.RADIO_CLK,
            peripherals.WIFI,
            raw,
        )
        .await;
        enter_deep_sleep(peripherals.LPWR, ACTIVE_POLL_INTERVAL);
    }

    // Empty house from here on.
    if was_present {
        // Falling edge: the bird just left. Publish one last reading so Home
        // Assistant returns to baseline, then resume idle polling.
        info!("bird left; publishing final reading {}", raw);
        state::set_bird_present(false);
        publish(
            spawner,
            peripherals.TIMG1,
            peripherals.RNG,
            peripherals.RADIO_CLK,
            peripherals.WIFI,
            raw,
        )
        .await;
    } else {
        // Steady empty: absorb slow drift into the baseline.
        state::set_baseline(baseline + (delta >> BASELINE_DRIFT_SHIFT));
    }

    enter_deep_sleep(peripherals.LPWR, IDLE_POLL_INTERVAL);
}

/// Bring up Wi-Fi + the network stack and publish `raw`, bounded by
/// [`WIFI_BUDGET`]. All failures are logged and swallowed: the caller deep-sleeps
/// straight after, which tears down the half-built stack regardless.
async fn publish(
    spawner: Spawner,
    timg1: TIMG1,
    rng: RNG,
    radio_clk: RADIO_CLK,
    wifi: WIFI,
    raw: i32,
) {
    match with_timeout(
        WIFI_BUDGET,
        connect_and_publish(spawner, timg1, rng, radio_clk, wifi, raw),
    )
    .await
    {
        Ok(Ok(())) => info!("Published {} to {}", raw, MQTT_TOPIC),
        Ok(Err(e)) => warn!("publish failed: {}", e),
        Err(_) => warn!("Wi-Fi/publish exceeded {:?}, giving up", WIFI_BUDGET),
    }
}

/// Join Wi-Fi (STA + DHCP) and push a single reading to the broker.
async fn connect_and_publish(
    spawner: Spawner,
    timg1: TIMG1,
    rng: RNG,
    radio_clk: RADIO_CLK,
    wifi: WIFI,
    raw: i32,
) -> Result<(), &'static str> {
    // esp-wifi needs its own timer; TIMG0 is already owned by the executor, so
    // hand it TIMG1.
    let mut rng = Rng::new(rng);
    let timg1 = TimerGroup::new(timg1);
    let esp_wifi_ctrl = &*mk_static!(
        EspWifiController<'static>,
        esp_wifi::init(timg1.timer0, rng, radio_clk).map_err(|_| "wifi init")?
    );

    let (wifi_interface, controller) = esp_wifi::wifi::new_with_mode(esp_wifi_ctrl, wifi, WifiStaDevice)
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

    publish_reading(stack, raw).await?;

    // Give the TCP stack a moment to flush the FIN before we cut power.
    Timer::after(Duration::from_millis(200)).await;
    Ok(())
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

/// Open a TCP connection to the broker and publish the raw reading (as ASCII).
async fn publish_reading(stack: &'static WifiStack, raw: i32) -> Result<(), &'static str> {
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

    // Encode the raw i32 as a decimal ASCII string for the HA template.
    let mut payload = heapless::String::<16>::new();
    write_i32(&mut payload, raw);

    client
        .send_message(
            MQTT_TOPIC,
            payload.as_bytes(),
            QualityOfService::QoS0,
            false,
        )
        .await
        .map_err(|_| "mqtt publish")?;

    Ok(())
}

/// Minimal `i32 -> String` formatting (avoids pulling in `core::fmt` write on
/// the hot path; `heapless` supports `write!` but this keeps intent explicit).
fn write_i32(buf: &mut heapless::String<16>, value: i32) {
    use core::fmt::Write;
    // Infallible for i32 into a 16-byte buffer (max "-2147483648" = 11 chars).
    let _ = write!(buf, "{}", value);
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

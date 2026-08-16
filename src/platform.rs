//! Concrete ESP32-C3 wiring for the sensor abstraction (#12).
//!
//! `sensors/` is HAL-agnostic; this module is where it meets the board. It
//! brings up the buses this node actually needs (per [`node::active`]),
//! constructs the enabled drivers, and offers one `measure_all()` that the
//! publish path can call without knowing what is populated.
//!
//! ## Pin map (Seeed XIAO ESP32-C3 silkscreen -> GPIO)
//!
//! | Pad | GPIO | Use |
//! | --- | --- | --- |
//! | D0 | 2  | HX711 SCK |
//! | D1 | 3  | HX711 DT |
//! | D2 | 4  | DS18B20 1-Wire |
//! | D3 | 5  | SDS011 UART RX (sensor TX) |
//! | D4 | 6  | I²C SDA (SHT31-D, SCD41) |
//! | D5 | 7  | I²C SCL |
//! | D10| 10 | SDS011 UART TX (sensor RX) |
//!
//! The SDS011 deliberately avoids D6/D7 (GPIO21/20): those are the console UART
//! pads, and the log output shares them.
//!
//! The two I²C sensors sit on one bus at different addresses (0x44 / 0x62), so
//! the bus is wrapped in a [`SharedI2c`] handle that each driver can own.

use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex};
use embedded_hal_async::i2c::{ErrorType, I2c as I2cTrait, Operation};
use esp_hal::{
    gpio::GpioPin,
    i2c::master::{Config as I2cConfig, Error as I2cError, I2c},
    peripherals::{I2C0, UART1},
    uart::{Config as UartConfig, Uart},
    Async,
};
use heapless::Vec;
use log::{info, warn};
use static_cell::StaticCell;

use crate::node::{self, Slot};
use crate::sensors::{scd41, scd41::Scd41, sds011::Sds011, sht31, sht31::Sht31, Reading, Sensor};

/// SDS011 baud rate (fixed in hardware).
const SDS011_BAUD: u32 = 9600;

/// Upper bound on the readings one round can produce: weight + probe
/// temperature + SHT31 (2) + SCD41 (3) + SDS011 (2), with headroom.
pub const MAX_SAMPLES: usize = 12;

/// One reading plus the node-level key prefix that disambiguates it (see
/// [`Slot`]). The publish path turns this into `<ns>/<node>/<prefix><key>`.
pub struct Sample {
    pub prefix: &'static str,
    pub reading: Reading,
}

/// The set of readings gathered in one round.
pub type Samples = Vec<Sample, MAX_SAMPLES>;

// --- Shared I²C bus ----------------------------------------------------------

type I2cBus = I2c<'static, Async>;
static I2C_BUS: StaticCell<Mutex<NoopRawMutex, I2cBus>> = StaticCell::new();

/// A cloneable handle to the one I²C peripheral, so both I²C drivers can own
/// their bus (as the [`Sensor`] contract wants) while sharing the hardware.
/// `NoopRawMutex` is sufficient: everything runs on the single-core executor and
/// measurements are sequential, the lock only enforces that at compile time.
#[derive(Clone, Copy)]
pub struct SharedI2c(&'static Mutex<NoopRawMutex, I2cBus>);

impl ErrorType for SharedI2c {
    type Error = I2cError;
}

impl I2cTrait for SharedI2c {
    async fn transaction(
        &mut self,
        address: u8,
        operations: &mut [Operation<'_>],
    ) -> Result<(), Self::Error> {
        // Spell the trait out: esp-hal's `I2c` also has an inherent
        // `transaction` taking its own `Operation` type, which would shadow this.
        let mut bus = self.0.lock().await;
        I2cTrait::transaction(&mut *bus, address, operations).await
    }
}

// --- Peripherals this module may claim ---------------------------------------

/// The peripherals the sensor platform can take over. `main` hands these across
/// wholesale; which of them are actually touched depends on the identity.
pub struct Peripherals {
    pub i2c0: I2C0,
    pub sda: GpioPin<6>,
    pub scl: GpioPin<7>,
    pub uart1: UART1,
    pub uart_rx: GpioPin<5>,
    pub uart_tx: GpioPin<10>,
}

/// The drivers populated on this node. Absent sensors are simply `None`, so the
/// publish path is the same code for every node in the fleet.
pub struct Sensors {
    bus: Option<SharedI2c>,
    sht31: Option<Sht31<SharedI2c>>,
    scd41: Option<Scd41<SharedI2c>>,
    sds011: Option<Sds011<Uart<'static, Async>>>,
    /// Whether the I²C bring-up probe has already run this boot.
    probed: bool,
}

impl Sensors {
    /// Bring up only the buses this node needs and construct its drivers.
    /// [`node::init`] must have run first — the identity decides what exists.
    pub fn new(p: Peripherals) -> Self {
        let node = node::active();

        let bus = if node.uses_i2c() {
            let i2c = I2c::new(p.i2c0, I2cConfig::default())
                .with_sda(p.sda)
                .with_scl(p.scl)
                .into_async();
            Some(SharedI2c(I2C_BUS.init(Mutex::new(i2c))))
        } else {
            None
        };

        let sht31 = match (bus, node.sht31.enabled) {
            (Some(bus), true) => Some(Sht31::new(bus)),
            _ => None,
        };
        let scd41 = match (bus, node.scd41.enabled) {
            (Some(bus), true) => Some(Scd41::new(bus, node.scd41_mode())),
            _ => None,
        };

        let sds011 = if node.uses_uart() {
            // A UART that fails to configure is a wiring/build mistake, not a
            // runtime condition, so log it and carry on without the sensor.
            match Uart::new_with_config(
                p.uart1,
                UartConfig::default().baudrate(SDS011_BAUD),
                p.uart_rx,
                p.uart_tx,
            ) {
                Ok(uart) => Some(Sds011::new(uart.into_async())),
                Err(e) => {
                    warn!("SDS011 UART init failed: {:?}; sensor disabled", e);
                    None
                }
            }
        } else {
            None
        };

        Self {
            bus,
            sht31,
            scd41,
            sds011,
            probed: false,
        }
    }

    /// Ask each expected I²C address whether anything is there, and say so in
    /// the log. Without this a wiring fault, a missing pull-up and a strapped
    /// address all look identical from the outside: "not responding".
    ///
    /// The probe commands are side-effect-free reads, and an SHT31-D found only
    /// at the alternate address is adopted rather than reported as missing —
    /// breakouts differ on how they strap ADDR.
    async fn probe_i2c(&mut self) {
        let Some(bus) = self.bus else { return };

        if self.sht31.is_some() {
            let primary = acks(bus, sht31::ADDR, sht31::CMD_READ_STATUS).await;
            let alt = acks(bus, sht31::ADDR_ALT, sht31::CMD_READ_STATUS).await;
            match (primary, alt) {
                (true, _) => info!("SHT31-D found at 0x{:02X}", sht31::ADDR),
                (false, true) => {
                    info!(
                        "SHT31-D found at 0x{:02X} (ADDR strapped high); using it",
                        sht31::ADDR_ALT
                    );
                    self.sht31 = Some(Sht31::with_address(bus, sht31::ADDR_ALT));
                }
                (false, false) => warn!(
                    "no SHT31-D at 0x{:02X} or 0x{:02X} — check SDA/SCL and the pull-ups",
                    sht31::ADDR,
                    sht31::ADDR_ALT
                ),
            }
        }

        if self.scd41.is_some() {
            if acks(bus, scd41::ADDR, scd41::CMD_GET_DATA_READY).await {
                info!("SCD41 found at 0x{:02X}", scd41::ADDR);
            } else {
                warn!(
                    "no SCD41 at 0x{:02X} — check SDA/SCL and the pull-ups",
                    scd41::ADDR
                );
            }
        }
    }

    /// Measure every populated sensor and append the readings to `out`.
    ///
    /// A sensor that does not answer contributes nothing and is logged — never
    /// fatal, matching the DS18B20 contract. This is only called on publish
    /// cycles: the SDS011 in particular spends 10–30 s spinning its fan here.
    pub async fn measure_all(&mut self, out: &mut Samples) {
        let node = node::active();

        // Once per boot, ahead of the first reading, so the log says *why* a
        // sensor is quiet before it is quiet. A few I²C transactions; the
        // battery node's cheap idle wakes never get here at all.
        if !self.probed {
            self.probed = true;
            self.probe_i2c().await;
        }

        if let Some(s) = self.sht31.as_mut() {
            collect(s, node.sht31, out).await;
        }
        if let Some(s) = self.scd41.as_mut() {
            collect(s, node.scd41, out).await;
        }
        if let Some(s) = self.sds011.as_mut() {
            collect(s, node.sds011, out).await;
        }
    }
}

/// Does a device acknowledge `addr`? `probe_cmd` must be a command with no side
/// effects — we only care whether the address is ACKed, not what comes back.
async fn acks(mut bus: SharedI2c, addr: u8, probe_cmd: u16) -> bool {
    bus.write(addr, &probe_cmd.to_be_bytes()).await.is_ok()
}

/// Run one sensor and fold its readings into the round's samples.
async fn collect<S: Sensor>(sensor: &mut S, slot: Slot, out: &mut Samples) {
    let readings = sensor.measure().await;
    if readings.is_empty() {
        warn!("{} not responding; skipping its readings", sensor.kind());
        return;
    }
    for reading in readings {
        info!(
            "{}: {}{} = {}",
            sensor.kind(),
            slot.prefix,
            reading.key,
            reading.value
        );
        if out
            .push(Sample {
                prefix: slot.prefix,
                reading,
            })
            .is_err()
        {
            warn!("sample buffer full; dropping remaining readings");
            return;
        }
    }
}

/// Append a reading produced outside the [`Sensor`] trait (the HX711 weight and
/// the DS18B20 temperature, which both need `main`'s state).
pub fn push_sample(out: &mut Samples, slot: Slot, key: &'static str, value: heapless::String<16>) {
    let _ = out.push(Sample {
        prefix: slot.prefix,
        reading: Reading { key, value },
    });
}

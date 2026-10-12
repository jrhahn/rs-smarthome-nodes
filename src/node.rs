//! Flavors, identity and power profile (#12, #17).
//!
//! A **flavor** is a kind of board: which sensors are populated and whether it
//! sleeps between readings or stays awake. The table below lists them, and it
//! is all this repository knows about hardware. Which board is which -- its
//! topic id and the name Home Assistant shows -- is not in here at all: that
//! is the deployment's business and arrives at build time.
//!
//! An image is put together from three build-time values:
//!
//! - `FLAVOR=climate` picks the table entry, defaulting to `climate`. An
//!   unknown flavor fails the build (const-eval panic) rather than silently
//!   flashing the wrong hardware profile onto a board;
//! - `NODE_ID=<slug>` is what the board publishes under, defaulting to the
//!   flavor's own name, so a plain build still produces a working node;
//! - `NODE_NAME=<text>` is the device name in Home Assistant, defaulting to
//!   the id.
//!
//! A **provisioned** flavor stored in flash overrides the built one, without
//! touching the id -- see [`provision_topic`]. That is for a board whose
//! hardware changed, not for renaming one.
//!
//! Because the sensor set decides which peripherals get initialised, the
//! flavor is read once at boot ([`init`]) and then fixed for the run; a node
//! that is re-provisioned while running reboots into its new self.

use core::fmt::Write as _;
use core::ptr::addr_of;
#[cfg(any(feature = "hal", test))]
use core::ptr::addr_of_mut;

use heapless::String;
#[cfg(feature = "hal")]
use log::info;
use log::warn;

#[cfg(feature = "hal")]
use crate::config;
use crate::sensors::scd41::Mode as Scd41Mode;

/// How a node is powered *and* whether it duty-cycles, which together decide
/// whether it deep-sleeps between samples or stays associated and loops (#17).
///
/// Power source and sleep policy are not the same question, and this enum used
/// to pretend they were. A cell forces sleeping; a cable only *permits* staying
/// awake, and permitting is not requiring -- see [`PowerProfile::MainsDutyCycled`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PowerProfile {
    /// Deep-sleep between samples; wakes cold and re-runs `main` (bird scale).
    /// Cadence comes from the runtime intervals in [`crate::config::Config`],
    /// because on a cell it is worth retuning without a reflash.
    Battery,
    /// Stays awake, keeps Wi-Fi up, samples on a fixed cadence. Required for the
    /// SDS011 fan and for CO₂ continuity.
    Mains,
    /// On a cable, but deep-sleeps between rounds anyway, at the node's own
    /// [`NodeConfig::sample_secs`].
    ///
    /// The reason is heat, not power. A node that keeps Wi-Fi associated draws
    /// on the order of 80-110 mA without pause, and in a closed box that is a
    /// third of a watt with nowhere to go. `docs/commissioning.md` measured
    /// 0.9 °C of it on a `climate-co2` board in 2026-09, and the `climate` boxes are
    /// smaller. A node whose entire job is to report the room's temperature
    /// cannot afford to warm the air it is reporting on, and the cheapest way
    /// not to is to stop running between readings.
    ///
    /// Only a node with nothing to keep continuous may take this: no SDS011 fan
    /// to duty-cycle, no SCD41 whose self-calibration assumes it keeps running,
    /// no SGP41 hotplate, and no load cell (whose presence logic wants the
    /// battery profile's two-speed cadence, not a fixed one). A const assert
    /// below enforces all four.
    ///
    /// Unlike [`PowerProfile::Battery`] this sleeps for `sample_secs` rather
    /// than the runtime intervals, so the published cadence is exactly what it
    /// was while the node stayed awake and no history gets a step in it.
    MainsDutyCycled,
}

impl PowerProfile {
    /// Runs off a cell, so its cadence comes from [`crate::config::Config`] and
    /// it gets the battery tuning knobs in Home Assistant.
    pub const fn is_battery(self) -> bool {
        matches!(self, PowerProfile::Battery)
    }

    /// Sleeps between rounds, whatever it is powered from.
    ///
    /// This, not [`PowerProfile::is_battery`], is the question to ask about
    /// last-wills, SCD41 mode, and which of the two loops in `main` runs: all
    /// three turn on whether the node is reachable between readings, and a
    /// duty-cycled mains node is exactly as absent as a battery one.
    pub const fn deep_sleeps(self) -> bool {
        !matches!(self, PowerProfile::Mains)
    }

    /// The word this profile goes by in the boot log.
    pub const fn label(self) -> &'static str {
        match self {
            PowerProfile::Battery => "battery",
            PowerProfile::Mains => "mains",
            PowerProfile::MainsDutyCycled => "mains, duty-cycled",
        }
    }

    /// How far a publish round may be stretched while the network is gone, as a
    /// multiple of the normal interval.
    ///
    /// The ceiling is a property of what a wake-up *costs*, which is why it
    /// hangs off the profile rather than being one number.
    ///
    /// On mains the wake-up is free and the only thing backing off buys is not
    /// knocking on a dead access point every two minutes, so one doubling is
    /// the whole benefit; stretching further would trade readings for nothing.
    /// On a cell the wake-up is the expensive part of the day — bringing the
    /// radio up to be refused is close to the worst way to spend it — so this
    /// goes considerably further.
    ///
    /// **Both ceilings are bounded by over-the-air reach, not by power.** A
    /// node that has backed off is a node that cannot be updated for that long,
    /// and the fix for whatever is wrong usually arrives that way. Sixteen
    /// rounds of a ten-minute heartbeat is already most of a day's patience.
    pub const fn sleep_backoff_ceiling(self) -> u32 {
        match self {
            PowerProfile::Mains | PowerProfile::MainsDutyCycled => 2,
            PowerProfile::Battery => 16,
        }
    }
}

/// How much to stretch the next publish round, after `failed_rounds`
/// consecutive rounds that never reached the broker.
///
/// Doubling, capped at [`PowerProfile::sleep_backoff_ceiling`]. Zero failed
/// rounds is a factor of one, so a healthy node's cadence is untouched — this
/// has to be invisible on every node that is working, which is nearly always
/// all of them.
///
/// Doubling by rounds rather than by refusals: a single failed round now spends
/// four or five join attempts (see [`crate::wifi::join_backoff_ms`]), so pacing
/// off the refusal count would reach the ceiling inside one bad round and
/// stretch a cadence that nothing is actually wrong with.
pub fn sleep_backoff_factor(failed_rounds: u32, profile: PowerProfile) -> u32 {
    let ceiling = profile.sleep_backoff_ceiling();
    let mut factor: u32 = 1;
    for _ in 0..failed_rounds.min(32) {
        if factor >= ceiling {
            break;
        }
        factor = factor.saturating_mul(2);
    }
    factor.min(ceiling)
}

/// One sensor slot on a node: whether it is populated, plus how its readings are
/// named. `prefix` disambiguates keys when two sensors emit the same quantity
/// (the bedroom's SCD41 and SHT31-D both report temperature and humidity), and
/// `label` does the same for the human-readable Home Assistant entity name. It
/// also just *names* things where there is nothing to collide with — the outdoor
/// node's `battery_voltage` is a plain voltage until the prefix says whose.
#[derive(Clone, Copy)]
pub struct Slot {
    pub enabled: bool,
    pub prefix: &'static str,
    pub label: &'static str,
    /// Keys that keep their plain form even though the slot carries a prefix.
    ///
    /// The prefix exists to stop two sensors on one node claiming the same
    /// entity. A quantity only this sensor measures has nothing to collide
    /// with, and renaming it would throw away its Home Assistant history for
    /// nothing — `co2` on a node whose SCD41 shares the bus with an SHT31 is
    /// exactly that case: the SHT31 takes over `temperature` and `humidity`,
    /// while the CO₂ curve carries on under the id it has always had.
    pub unprefixed: &'static [&'static str],
    /// Seconds between this sensor's own measurements, or `0` to run it on
    /// every round like everything else.
    ///
    /// One node can carry sensors whose natural cadences are an order of
    /// magnitude apart: an SCD41 wants a reading a minute, while the SDS011's
    /// fan is a ~8000 h consumable that must not spin more than about four
    /// times an hour. Before this existed the node had a single `sample_secs`,
    /// so the two had to be traded off against each other — the fan dragged
    /// the whole node down to its own cadence. Now [`NodeConfig::sample_secs`]
    /// is the *base* round, and a slot may sit out rounds it does not need.
    ///
    /// Rounded up to a whole number of base rounds (see
    /// [`Slot::rounds_between`]): the loop only wakes on the base cadence, so
    /// a period between two multiples would otherwise drift.
    pub period_secs: u32,
    /// Whether this sensor's readings are corrected for ambient humidity, and
    /// its uncorrected values published alongside them (see
    /// [`crate::sensors::sds011::compensate`]). Only the SDS011 honours it, and
    /// only a node that also carries an SHT31 may set it — a const assert below
    /// enforces that, since the correction has nothing to work from otherwise.
    pub compensated: bool,
    /// Whether the fitted gas sensor has a NOx channel, i.e. is an SGP41 and
    /// not an SGP40. Only the SGP41 slot honours it.
    ///
    /// Static because discovery is built from the node config and has no bus to
    /// ask, while the driver settles the same question at runtime to avoid
    /// *publishing* a channel that is not there. If this disagrees with the
    /// board, correcting it re-announces by itself: the announcement digest
    /// covers the entity set (see [`crate::discovery::announcement_tag`]).
    pub nox: bool,
}

impl Slot {
    /// Populated, using each descriptor's plain key and name.
    pub const fn on() -> Slot {
        Slot {
            enabled: true,
            prefix: "",
            label: "",
            unprefixed: &[],
            period_secs: 0,
            compensated: false,
            nox: false,
        }
    }

    /// Populated, with its keys/names disambiguated, e.g. `("air_", "Luft")`
    /// turns `temperature` / "Temperatur" into `air_temperature` / "Luft
    /// Temperatur".
    pub const fn on_as(prefix: &'static str, label: &'static str) -> Slot {
        Slot {
            enabled: true,
            prefix,
            label,
            unprefixed: &[],
            period_secs: 0,
            compensated: false,
            nox: false,
        }
    }

    /// Exempt these keys from the slot's prefix (see [`Slot::unprefixed`]).
    pub const fn keeping(self, unprefixed: &'static [&'static str]) -> Slot {
        Slot { unprefixed, ..self }
    }

    /// Run this sensor on its own, slower cadence (see [`Slot::period_secs`]).
    pub const fn every(self, period_secs: u32) -> Slot {
        Slot {
            period_secs,
            ..self
        }
    }

    /// Correct this sensor against the node's SHT31 and publish the raw values
    /// too (see [`Slot::compensated`]).
    /// Declare that the gas sensor in this slot has a NOx channel.
    pub const fn with_nox(self) -> Slot {
        Slot { nox: true, ..self }
    }

    pub const fn compensated(self) -> Slot {
        Slot {
            compensated: true,
            nox: false,
            ..self
        }
    }

    /// Not populated on this node.
    pub const fn off() -> Slot {
        Slot {
            enabled: false,
            prefix: "",
            label: "",
            unprefixed: &[],
            period_secs: 0,
            compensated: false,
            nox: false,
        }
    }

    /// The key prefix that applies to `key` — empty for an exempted one.
    pub fn prefix_for(&self, key: &str) -> &'static str {
        if self.is_exempt(key) {
            ""
        } else {
            self.prefix
        }
    }

    /// The entity-name label that applies to `key`, same rule as
    /// [`Slot::prefix_for`] so the id and the display name never disagree.
    pub fn label_for(&self, key: &str) -> &'static str {
        if self.is_exempt(key) {
            ""
        } else {
            self.label
        }
    }

    fn is_exempt(&self, key: &str) -> bool {
        self.unprefixed.contains(&key)
    }

    /// How many base rounds this slot sits out between measurements, given the
    /// node's base cadence. Always at least 1, so a slot with no period of its
    /// own — or one shorter than a single round — simply runs every time.
    ///
    /// Rounds **up**: with a 60 s base and a 900 s period that is every 15th
    /// round; with a 100 s period it is every second round (200 s), because
    /// running early would spin the fan more often than asked, and the fan is
    /// the whole reason the knob exists.
    pub const fn rounds_between(&self, base_secs: u64) -> u32 {
        if self.period_secs == 0 {
            return 1;
        }
        let base = if base_secs == 0 { 1 } else { base_secs };
        let rounds = (self.period_secs as u64).div_ceil(base);
        if rounds == 0 {
            1
        } else {
            rounds as u32
        }
    }

    /// The seconds this slot actually publishes on, i.e. its period rounded to
    /// whole base rounds. What Home Assistant must expire it against.
    pub const fn effective_secs(&self, base_secs: u64) -> u64 {
        let base = if base_secs == 0 { 1 } else { base_secs };
        base * self.rounds_between(base_secs) as u64
    }
}

/// A flavor, and once [`BUILT_AS`] has stamped an identity on it, one board.
#[derive(Clone, Copy)]
pub struct NodeConfig {
    /// Which table entry this is, e.g. `"climate"`. What provisioning compares.
    pub flavor: &'static str,
    /// Slug used in topics, unique ids and the client id. The flavor's name in
    /// the table; the deployment's `NODE_ID` once built.
    pub id: &'static str,
    /// Device name shown in Home Assistant. The flavor's description in the
    /// table; the deployment's `NODE_NAME` once built.
    pub name: &'static str,
    /// Topic namespace: state topics are `<namespace>/<id>/<key>`.
    pub namespace: &'static str,
    pub power: PowerProfile,
    /// Seconds between sample+publish rounds, for both mains profiles: awake it
    /// is how long the loop waits, duty-cycled it is how long the node sleeps.
    /// Battery nodes use the runtime idle/active intervals from
    /// [`crate::config::Config`] instead.
    pub sample_secs: u64,
    pub scale: Slot,
    pub ds18b20: Slot,
    pub sht31: Slot,
    pub scd41: Slot,
    pub sds011: Slot,
    /// SGP41 (or SGP40) gas sensor. Mains only: its index needs a 1 Hz sample
    /// and a hotplate that never cools, which a sleeping node cannot give it.
    pub sgp41: Slot,
    /// Cell-voltage sense through the external divider (see [`crate::battery`]).
    /// Not a sensor on a bus but a property of how the board is powered, so it
    /// only ever belongs on a node with [`PowerProfile::Battery`] — and never
    /// alongside `ds18b20`, which wants the same pin.
    pub battery: Slot,
    /// The LED string of the `solar-lamp` flavor: a `light` entity in Home Assistant
    /// and an LEDC channel on the board. Unlike every slot above it names no
    /// bus and no sensor — it is an output, and the only one in the table.
    pub lamp: Slot,
}

impl NodeConfig {
    /// Does this node need the I²C bus brought up?
    pub const fn uses_i2c(&self) -> bool {
        self.sht31.enabled || self.scd41.enabled || self.sgp41.enabled
    }

    /// Does this node need the UART brought up?
    pub const fn uses_uart(&self) -> bool {
        self.sds011.enabled
    }

    /// A sleeping node takes one single-shot CO₂ sample per wake; a node that
    /// stays awake lets the SCD41 run continuously, which is what its
    /// self-calibration expects. Keyed on sleeping rather than on the power
    /// source: a periodic measurement that is cut off by a deep sleep is not
    /// periodic, whatever the node is plugged into.
    pub const fn scd41_mode(&self) -> Scd41Mode {
        if self.power.deep_sleeps() {
            Scd41Mode::SingleShot
        } else {
            Scd41Mode::Periodic
        }
    }

    /// State topic for one reading: `<namespace>/<id>/<prefix><key>`.
    pub fn state_topic(&self, prefix: &str, key: &str) -> String<80> {
        let mut t = String::new();
        let _ = write!(t, "{}/{}/{}{}", self.namespace, self.id, prefix, key);
        t
    }

    /// Whether this node backs its Home Assistant availability with an MQTT
    /// last-will.
    ///
    /// Only a node that stays awake does. A sleeping node is *meant* to be
    /// disconnected almost all the time — it wakes, publishes, and drops the
    /// link again — so a will would mark it offline seconds after every
    /// reading. Those nodes rely on the discovery `expire_after` instead (see
    /// [`crate::discovery`]). That is about reachability, not about the power
    /// source, so a duty-cycled mains node drops its will along with the
    /// battery one.
    pub const fn uses_lwt(&self) -> bool {
        !self.power.deep_sleeps()
    }

    /// Retained topic carrying `online` / `offline` for a node with a last-will.
    pub fn availability_topic(&self) -> String<80> {
        self.state_topic("", "status")
    }

    /// Prefix under which Home Assistant publishes retained tuning values.
    pub fn config_prefix(&self) -> String<64> {
        let mut t = String::new();
        let _ = write!(t, "{}/{}/config/", self.namespace, self.id);
        t
    }

    /// Retained topic an update offer arrives on.
    ///
    /// Four levels deep on purpose. The archiver subscribes to
    /// `<namespace>/+/+`, i.e. exactly three, so an offer published one level
    /// higher would be handed to the ingest path and counted as an unparseable
    /// reading once per delivery. Everything under `ota/` is invisible to it.
    pub fn ota_offer_topic(&self) -> String<64> {
        let mut t = String::new();
        let _ = write!(t, "{}/{}/ota/offer", self.namespace, self.id);
        t
    }

    /// Where the node says which image it is actually running — the answer to
    /// "is this one updated?", which cost an hour of guessing on 2026-09-17.
    pub fn ota_version_topic(&self) -> String<64> {
        let mut t = String::new();
        let _ = write!(t, "{}/{}/ota/version", self.namespace, self.id);
        t
    }

    /// Retained topic carrying what this board *is*, as opposed to what it
    /// measures: MAC, running image, address, active slot.
    ///
    /// Four levels deep for the same reason the OTA topics are — the archiver's
    /// `<namespace>/+/+` subscription must never be handed a payload that is
    /// not a number.
    pub fn meta_topic(&self) -> String<64> {
        let mut t = String::new();
        let _ = write!(t, "{}/{}/meta/board", self.namespace, self.id);
        t
    }

    /// Wildcard the firmware subscribes to while it is online.
    pub fn config_wildcard(&self) -> String<64> {
        let mut t = self.config_prefix();
        let _ = t.push('#');
        t
    }

    /// MQTT client id. Unique per node so two boards never fight over a session.
    pub fn client_id(&self) -> String<32> {
        let mut t = String::new();
        let _ = write!(t, "rs-{}", self.id);
        t
    }
}

// --- The flavors -------------------------------------------------------------

/// Temperature and humidity, nothing else: a XIAO and an SHT31-D on wires.
///
/// Duty-cycled rather than awake. On a USB charger staying associated costs
/// nothing in energy, but it costs about 0.3 W of heat in a 65 x 39 x 30 box,
/// and a node that reports only temperature and humidity has no other output
/// to weigh that against. A box that reads 2.5 K warmer than the room reports a
/// relative humidity about seven points low -- in a room that gets wet on
/// purpose, the one number people look at. Nothing here needs continuity: no
/// fan, no CO₂ self-calibration, no hotplate. So it sleeps, and `sample_secs`
/// below is the sleep, at the 120 s cadence these nodes have always published.
const CLIMATE: NodeConfig = NodeConfig {
    flavor: "climate",
    id: "climate",
    name: "Climate",
    namespace: "smarthome",
    power: PowerProfile::MainsDutyCycled,
    sample_secs: 120,
    scale: Slot::off(),
    ds18b20: Slot::off(),
    sht31: Slot::on(),
    scd41: Slot::off(),
    sds011: Slot::off(),
    sgp41: Slot::off(),
    battery: Slot::off(),
    lamp: Slot::off(),
};

/// Climate plus CO₂. The SHT31-D is not redundant with the SCD41's built-in
/// RH/T: the SCD4x datasheet specifies its humidity at ±6 %RH (±9 outside
/// 15–35 °C / 20–65 %RH) against the SHT31-D's ±2 %, because it sits on a die
/// that heats itself for the CO₂ measurement. Measured side by side on
/// 2026-08-26 the two disagreed by 15 points. So the SHT31-D owns
/// `temperature` and `humidity`, and the SCD41's own pair is published under
/// `scd41_` — still worth having, because calibrating its temperature offset
/// against a reference is exactly Sensirion's field procedure and needs both
/// numbers visible. `co2` keeps its plain key: nothing collides with it.
const CLIMATE_CO2: NodeConfig = NodeConfig {
    flavor: "climate-co2",
    id: "climate-co2",
    name: "Climate CO2",
    namespace: "smarthome",
    power: PowerProfile::Mains,
    sample_secs: 60,
    scale: Slot::off(),
    ds18b20: Slot::off(),
    sht31: Slot::on(),
    scd41: Slot::on_as("scd41_", "SCD41").keeping(&["co2"]),
    sds011: Slot::off(),
    sgp41: Slot::off(),
    battery: Slot::off(),
    lamp: Slot::off(),
};

/// The busiest flavor: [`CLIMATE_CO2`] plus particulates and a gas index, on
/// both buses at once.
///
/// Meant for the room people sit in, not for a kitchen. The SDS011's optics and
/// fan are open to the air they measure, and kitchen air carries fat aerosol
/// from frying — that films the optical path and drifts the calibration without
/// ever failing outright, the same silent-but-plausible failure the SCD41's
/// membrane already cost one of. And the question worth a long time series is
/// what reaches the room people are in, not that cooking makes smoke.
///
/// The SHT31 on the same board is what makes the humidity correction possible
/// at all (see [`Slot::compensated`]).
///
/// The three sensors' natural cadences are two orders of magnitude apart, hence
/// the per-slot period: the base round stays at the SCD41's minute, and the
/// SDS011 sits out fourteen of every fifteen of them so its ~8000 h fan is not
/// spent keeping up with the CO₂.
const AIR_QUALITY: NodeConfig = NodeConfig {
    flavor: "air-quality",
    id: "air-quality",
    name: "Air quality",
    namespace: "smarthome",
    power: PowerProfile::Mains,
    sample_secs: 60,
    scale: Slot::off(),
    ds18b20: Slot::off(),
    sht31: Slot::on(),
    scd41: Slot::on_as("scd41_", "SCD41").keeping(&["co2"]),
    sds011: Slot::on().compensated().every(900),
    // The gas sensor needs a 1 Hz sample, which only a mains node can give
    // it, and a VOC index is a statement about the air people are in.
    // `with_nox` announces the second channel; an SGP40 in the socket instead
    // of an SGP41 is detected at boot and simply never feeds it.
    sgp41: Slot::on().with_nox(),
    battery: Slot::off(),
    lamp: Slot::off(),
};

/// The outdoor bird-feeder scale: load cell, SHT31-D and cell-voltage sense,
/// on a battery so it needs no cable run outside.
///
/// D2 carries either the divider or a DS18B20, never both — the divider has the
/// pin here.
///
/// The divider keeps a `battery_` prefix and the SHT31-D has none, and the
/// difference is deliberate: a bare `voltage` entity in Home Assistant does not
/// say whose, while `temperature` and `humidity` have nothing to collide with on
/// a board without a probe. (Its predecessor put the SHT31 behind `air_` to
/// leave those keys to a DS18B20 that was not fitted for a long time.)
const BIRD_SCALE: NodeConfig = NodeConfig {
    flavor: "bird-scale",
    id: "bird-scale",
    name: "Bird scale",
    namespace: "smarthome",
    power: PowerProfile::Battery,
    // Only consulted by mains nodes; a battery cadence comes from `Config`.
    sample_secs: 60,
    scale: Slot::on(),
    ds18b20: Slot::off(),
    sht31: Slot::on(),
    scd41: Slot::off(),
    sds011: Slot::off(),
    sgp41: Slot::off(),
    battery: Slot::on_as("battery_", "Batterie"),
    lamp: Slot::off(),
};

/// A rebuilt solar garden light, the only flavor whose job is to *emit* rather
/// than to measure.
///
/// Battery and deep-sleeping, but on a cadence nothing else here has: it wakes
/// once at dusk, stays up for hours breathing, and sleeps through the day. See
/// [`docs/solarleuchte.md`](../docs/solarleuchte.md).
///
/// One sensor, the SHT31-D, and only to find out whether the cell is charged
/// below freezing. The cell voltage is not telemetry on this board but part of
/// the control loop — [`crate::lamp::charge_permille`] derives the evening's
/// brightness ceiling from it.
const SOLAR_LAMP: NodeConfig = NodeConfig {
    flavor: "solar-lamp",
    id: "solar-lamp",
    name: "Solar lamp",
    namespace: "smarthome",
    power: PowerProfile::Battery,
    // Only consulted by mains nodes; a battery cadence comes from `Config`.
    sample_secs: 60,
    scale: Slot::off(),
    ds18b20: Slot::off(),
    // Not for the lamp's sake — nothing here reads the weather. It is the only
    // way to find out whether this cell is ever charged below freezing, which
    // is the one risk the charger's sense resistor is sized against and which
    // nobody has measured. `D4`/`D5` were free, so it costs a part and no
    // firmware.
    sht31: Slot::on(),
    scd41: Slot::off(),
    sds011: Slot::off(),
    sgp41: Slot::off(),
    battery: Slot::on_as("battery_", "Batterie"),
    lamp: Slot::on(),
};

/// Every flavor, keyed by the name `FLAVOR=` and provisioning accept. The
/// single source of truth: [`flavor`] walks it, so an entry added here is
/// immediately selectable both ways.
pub const FLAVORS: &[(&str, NodeConfig)] = &[
    ("climate", CLIMATE),
    ("climate-co2", CLIMATE_CO2),
    ("air-quality", AIR_QUALITY),
    ("bird-scale", BIRD_SCALE),
    ("solar-lamp", SOLAR_LAMP),
];

// D2 / GPIO4 carries either the DS18B20's 1-Wire line or the battery divider's
// tap on ADC1 — the pin cannot be both, and `main` hands it to whichever slot is
// enabled. A node asking for both would compile, then quietly give the pin to
// the probe and leave someone wondering why the voltage never arrives; fail the
// build here instead, where the table that caused it is on screen.
const _: () = {
    let mut i = 0;
    while i < FLAVORS.len() {
        assert!(
            !(FLAVORS[i].1.ds18b20.enabled && FLAVORS[i].1.battery.enabled),
            "a node cannot carry both a DS18B20 and a battery divider: they share D2"
        );
        i += 1;
    }
};

// Humidity compensation reads the node's own SHT31 (see `Slot::compensated`).
// A node asking for it without one would build, then publish a "corrected"
// value that is simply the raw one — the most misleading outcome available, and
// exactly the class of silent-but-wrong the fleet has been bitten by before.
// The flag is also meaningless on any slot but the SDS011, so say that here
// rather than let it sit in a table looking effective.
const _: () = {
    let mut i = 0;
    while i < FLAVORS.len() {
        let n = FLAVORS[i].1;
        assert!(
            !(n.sds011.compensated && !n.sht31.enabled),
            "humidity compensation needs an SHT31 on the same node to read from"
        );
        assert!(
            !(n.scale.compensated
                || n.ds18b20.compensated
                || n.sht31.compensated
                || n.scd41.compensated
                || n.battery.compensated),
            "only the SDS011 slot honours humidity compensation"
        );
        i += 1;
    }
};

// A duty-cycled mains node is asleep between rounds, which rules out every
// sensor that has to be attended to continuously — the SDS011's fan duty cycle,
// the SCD41's self-calibration, the SGP41's hotplate — and the load cell, whose
// presence logic is built around the battery profile's two-speed cadence rather
// than one fixed round. Each of those would build and then misbehave quietly:
// an index computed from a cold hotplate is a plausible-looking number that
// means nothing. Fail the build here, next to the table that asked for it.
const _: () = {
    let mut i = 0;
    while i < FLAVORS.len() {
        let n = FLAVORS[i].1;
        let duty_cycled = matches!(n.power, PowerProfile::MainsDutyCycled);
        assert!(
            !(duty_cycled && (n.sds011.enabled || n.scd41.enabled || n.sgp41.enabled)),
            "a duty-cycled node cannot carry a sensor that must run continuously"
        );
        assert!(
            !(duty_cycled && n.scale.enabled),
            "the load cell's presence logic needs the battery profile's cadence"
        );
        assert!(
            !(duty_cycled && n.battery.enabled),
            "a duty-cycled node is on a cable; it has no cell to measure"
        );
        i += 1;
    }
};

// The same names as one string, for error messages. Spelled out rather than
// built from [`FLAVORS`] because the const-eval `panic!` in `select` needs a
// literal, and a macro so that panic and `KNOWN_FLAVORS` share one; a test
// keeps it in step with `FLAVORS`.
macro_rules! known_flavors {
    () => {
        "climate, climate-co2, air-quality, bird-scale, solar-lamp"
    };
}

/// The flavors' names as one string, for error messages.
pub const KNOWN_FLAVORS: &str = known_flavors!();

/// The longest id a board may publish under. The flash slot, the client id and
/// every topic buffer are sized for it; a longer one would be cut short
/// somewhere silently, so the build refuses it instead.
pub const NODE_ID_MAX: usize = crate::config::NODE_NAME_MAX;

/// The longest device name. It goes verbatim into every discovery payload,
/// whose buffer is sized for it (see `discovery`).
pub const NODE_NAME_MAX: usize = 24;

/// The board this image was **built** as: the flavor from `FLAVOR`, the id and
/// name from `NODE_ID` / `NODE_NAME`. The fallback when flash carries no
/// provisioned flavor; use [`active`] for what is actually in force.
pub const BUILT_AS: NodeConfig = identify(
    select(match option_env!("FLAVOR") {
        Some(s) => s,
        None => "climate",
    }),
    option_env!("NODE_ID"),
    option_env!("NODE_NAME"),
);

/// Look a flavor up by name. Shared by the build-time selection and by runtime
/// provisioning, so both accept exactly the same set of names.
pub const fn flavor(name: &str) -> Option<NodeConfig> {
    let mut i = 0;
    while i < FLAVORS.len() {
        if str_eq(name, FLAVORS[i].0) {
            return Some(FLAVORS[i].1);
        }
        i += 1;
    }
    None
}

/// Map the `FLAVOR` build-time name onto a config. Unknown names panic during
/// const-eval, i.e. the build fails with the typo instead of producing a
/// plausible-looking image for the wrong hardware.
const fn select(name: &str) -> NodeConfig {
    match flavor(name) {
        Some(cfg) => cfg,
        None => {
            panic!(concat!(
                "unknown FLAVOR; expected one of: ",
                known_flavors!()
            ))
        }
    }
}

/// Stamp a deployment's identity on a flavor. No id keeps the flavor's own; no
/// name falls back to the id, which is at least the string the topics say.
pub const fn identify(
    flavor: NodeConfig,
    id: Option<&'static str>,
    name: Option<&'static str>,
) -> NodeConfig {
    let (id, name) = match (id, name) {
        (Some(id), Some(name)) => (id, name),
        (Some(id), None) => (id, id),
        (None, Some(name)) => (flavor.id, name),
        (None, None) => (flavor.id, flavor.name),
    };
    assert!(valid_id(id), "NODE_ID must be 1-16 of a-z, 0-9, '-', '_'");
    assert!(
        valid_name(name),
        "NODE_NAME must be 1-24 bytes without quotes or backslashes"
    );
    NodeConfig { id, name, ..flavor }
}

/// An id is a topic level, a client id and part of every unique id: lowercase
/// ASCII, digits, `-` and `_`, and short enough for the buffers.
const fn valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    if b.is_empty() || b.len() > NODE_ID_MAX {
        return false;
    }
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_') {
            return false;
        }
        i += 1;
    }
    true
}

/// A name is written into JSON unescaped, so it may carry anything but the two
/// characters that would end or break the string.
const fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    if b.is_empty() || b.len() > NODE_NAME_MAX {
        return false;
    }
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'"' || b[i] == b'\\' || b[i] < 0x20 {
            return false;
        }
        i += 1;
    }
    true
}

// --- The identity in force ---------------------------------------------------

/// The node this board is running as. Written once by [`init`] before anything
/// reads it, then only read — the same single-context, single-word discipline
/// [`crate::state`] uses for its RTC-RAM cells.
static mut ACTIVE: NodeConfig = BUILT_AS;

/// The identity in force. Cheap: [`NodeConfig`] is `Copy` and lives in RAM.
pub fn active() -> NodeConfig {
    unsafe { addr_of!(ACTIVE).read() }
}

/// This board's MAC, or all-zero before [`set_mac`] has been called.
///
/// The MAC is the only name that does not move: a board can be provisioned to
/// another flavor and reflashed with another image, and this stays what it was.
/// That is exactly the distinction that took two hours to make on 2026-09-17,
/// when two boards published under one node name — so it is published, not just
/// printed to a console nobody is reading.
pub fn mac() -> [u8; 6] {
    unsafe { addr_of!(BOARD_MAC).read() }
}

/// `ac:27:6e:7f:a6:b4`, lowercase and colon-separated — the form `udevadm`
/// prints and Home Assistant expects in a device `connections` entry.
pub fn mac_string() -> String<17> {
    let m = mac();
    let mut s = String::new();
    let _ = write!(
        s,
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[0], m[1], m[2], m[3], m[4], m[5]
    );
    s
}

/// Record the board's MAC. Call once in `main`, before anything announces
/// itself; everything after that reads it through [`mac`].
///
/// Available to the host tests as well, because the discovery payload is bigger
/// with a MAC in it than without — and a test that measured the smaller one
/// would be guarding the wrong number.
#[cfg(any(feature = "hal", test))]
pub fn set_mac(mac: [u8; 6]) {
    unsafe { addr_of_mut!(BOARD_MAC).write(mac) };
}

static mut BOARD_MAC: [u8; 6] = [0; 6];

/// Resolve the flavor for this boot: a provisioned one in flash wins over the
/// one the image was built with, and the id and name stay the built ones either
/// way. Call once, early in `main`, before any peripheral is set up — the
/// sensor set decides which buses come up at all.
///
/// A stored name that is not in the table is reported and ignored rather than
/// obeyed, so a bad provisioning message degrades to the build-time flavor
/// instead of a board that does nothing. That includes every room name a board
/// provisioned before flavors existed may still carry.
#[cfg(feature = "hal")]
pub fn init() {
    let Some(stored) = config::load_node_name() else {
        return;
    };
    match flavor(&stored) {
        Some(cfg) => {
            let cfg = NodeConfig {
                id: BUILT_AS.id,
                name: BUILT_AS.name,
                ..cfg
            };
            unsafe { addr_of_mut!(ACTIVE).write(cfg) };
            info!("provisioned as flavor '{}' (from flash)", stored);
        }
        None => warn!(
            "flash names unknown flavor '{}'; falling back to build-time '{}'. Expected one of: {}",
            stored, BUILT_AS.flavor, KNOWN_FLAVORS
        ),
    }
}

/// Topic a board listens on to be told which flavor it is:
/// `smarthome/provision/<mac>`, with the MAC as lowercase hex, no separators.
///
/// Keyed by MAC rather than by node id for the obvious chicken-and-egg
/// reason — a board that is not running the flavor it should still knows its
/// own MAC. Publish **retained** so a node picks it up whenever it next comes
/// online:
///
/// ```text
/// mosquitto_pub -h <broker> -r -t smarthome/provision/a1b2c3d4e5f6 -m climate-co2
/// ```
///
/// The payload `default` clears the override, returning the board to the
/// flavor it was flashed with.
pub fn provision_topic(mac: [u8; 6]) -> String<64> {
    let mut t = String::new();
    let _ = write!(t, "{}", PROVISION_PREFIX);
    for byte in mac {
        let _ = write!(t, "{:02x}", byte);
    }
    t
}

/// Fleet-wide provisioning namespace — deliberately not per-node, since the
/// point is to reach a board whose identity is not settled yet.
pub const PROVISION_PREFIX: &str = "smarthome/provision/";

/// Payload that clears a provisioned identity.
pub const PROVISION_RESET: &str = "default";

// --- Provisioning requests ---------------------------------------------------

/// What a retained provisioning message asks this board to become.
#[derive(Debug, PartialEq, Eq)]
pub enum Provision {
    /// Run as this flavor from the next boot (already checked against the table).
    Become(String<{ crate::config::NODE_NAME_MAX }>),
    /// Drop the stored override and go back to the built-in flavor.
    Reset,
}

/// Interpret a provisioning payload, or `None` if there is nothing to do.
///
/// "Nothing to do" is the common case and matters: the message is retained, so
/// it is re-delivered on *every* connect. Only an actual change may reach flash,
/// or a board would rewrite its identity sector for the rest of its life.
///
/// `current` is the node in force and `has_override` says whether flash
/// carries one — both passed in rather than read here, so the decision itself
/// stays pure and testable.
pub fn provision_request(
    value: &str,
    current: &NodeConfig,
    has_override: bool,
) -> Option<Provision> {
    // A zero-length payload is how a retained message is cleared, not a request.
    if value.is_empty() {
        return None;
    }

    if value == PROVISION_RESET {
        return has_override.then_some(Provision::Reset);
    }

    match flavor(value) {
        // Compared by flavor, never by id: the id is the deployment's and a
        // board running as `kitchen` is still a `climate` board.
        Some(cfg) if str_eq(cfg.flavor, current.flavor) => None,
        Some(_) => Some(Provision::Become(String::try_from(value).ok()?)),
        None => {
            warn!(
                "provisioning asked for unknown flavor '{}'; expected one of: {}",
                value, KNOWN_FLAVORS
            );
            None
        }
    }
}

/// `str` equality usable in const context (`==` on `&str` is not const in 1.83).
const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

const _: () = {
    assert!(str_eq("bad", "bad"));
    assert!(!str_eq("bad", "bat"));
    assert!(!str_eq("bad", "bads"));
    // Every name must round-trip through the lookup, and nothing else may.
    assert!(flavor("climate").is_some());
    assert!(flavor("Climate").is_none());
    assert!(flavor("").is_none());
    // Flavor names have to fit the flash slot they are provisioned into.
    assert!("climate-co2".len() <= crate::config::NODE_NAME_MAX);
    // The power profiles decide which sensors are even legal: the SDS011 fan
    // and continuous CO₂ both need a node that stays awake.
    assert!(!CLIMATE.power.is_battery());
    assert!(!CLIMATE_CO2.power.is_battery());
    assert!(!CLIMATE_CO2.power.deep_sleeps());
    assert!(CLIMATE.power.deep_sleeps());
    assert!(BIRD_SCALE.power.is_battery());
    // Every flavor's own id and name must pass what a deployment's must.
    assert!(valid_id(AIR_QUALITY.id) && valid_name(AIR_QUALITY.name));
    assert!(!valid_id("Kitchen") && !valid_id("") && !valid_id("a/b"));
    assert!(!valid_name("say \"hi\"") && valid_name("Küche"));
};

#[cfg(test)]
mod tests {
    use super::*;

    /// The bar this has to clear: a working fleet must not feel it at all.
    #[test]
    fn a_healthy_node_keeps_its_cadence() {
        for profile in [
            PowerProfile::Mains,
            PowerProfile::MainsDutyCycled,
            PowerProfile::Battery,
        ] {
            assert_eq!(sleep_backoff_factor(0, profile), 1, "{}", profile.label());
        }
    }

    #[test]
    fn the_sleep_doubles_to_the_profile_ceiling() {
        // Mains: one doubling and no more. 120 s becomes 240 s and stays there,
        // because waking costs nothing and the readings do.
        assert_eq!(sleep_backoff_factor(1, PowerProfile::MainsDutyCycled), 2);
        assert_eq!(sleep_backoff_factor(2, PowerProfile::MainsDutyCycled), 2);
        assert_eq!(sleep_backoff_factor(99, PowerProfile::MainsDutyCycled), 2);

        // Battery: four doublings to the ceiling, where the radio is the
        // expensive part of the day.
        assert_eq!(sleep_backoff_factor(1, PowerProfile::Battery), 2);
        assert_eq!(sleep_backoff_factor(2, PowerProfile::Battery), 4);
        assert_eq!(sleep_backoff_factor(4, PowerProfile::Battery), 16);
        assert_eq!(sleep_backoff_factor(5, PowerProfile::Battery), 16);
    }

    /// A garbage or runaway count must saturate at the ceiling, never overflow
    /// into a short sleep -- the same class of bug as the shift in
    /// `wifi::backoff_ms`, where too large a count produced no wait at all.
    #[test]
    fn a_runaway_count_cannot_shorten_the_sleep() {
        for profile in [PowerProfile::MainsDutyCycled, PowerProfile::Battery] {
            let ceiling = profile.sleep_backoff_ceiling();
            assert_eq!(sleep_backoff_factor(u32::MAX, profile), ceiling);
            assert!(sleep_backoff_factor(u32::MAX, profile) >= 1);
        }
    }

    /// What the ceilings mean in minutes, so changing one has to be deliberate.
    #[test]
    fn the_ceilings_stay_inside_over_the_air_reach() {
        // `climate`: 120 s rounds, so at worst four minutes dark.
        let mains = CLIMATE.sample_secs * u64::from(CLIMATE.power.sleep_backoff_ceiling());
        assert_eq!(mains, 240);
        // `bird-scale`: a ten-minute heartbeat, so at worst 160 minutes. Long,
        // and the reason the ceiling is not higher.
        let battery = 600 * u64::from(BIRD_SCALE.power.sleep_backoff_ceiling());
        assert_eq!(battery, 9_600);
    }

    #[test]
    fn an_exempt_key_keeps_its_plain_form() {
        // The prefix is there to stop two sensors claiming one entity. `co2`
        // has no rival on the node, so prefixing it would only cost its Home
        // Assistant history.
        let slot = Slot::on_as("scd41_", "SCD41").keeping(&["co2"]);
        assert_eq!(slot.prefix_for("co2"), "");
        assert_eq!(slot.label_for("co2"), "");
        assert_eq!(slot.prefix_for("temperature"), "scd41_");
        assert_eq!(slot.label_for("temperature"), "SCD41");
    }

    #[test]
    fn the_key_and_the_name_are_exempted_together() {
        // An id that says `co2` under a name that says "SCD41 CO₂" would be a
        // confusing half-rename, so both sides consult the same rule.
        for slot in [
            Slot::on(),
            Slot::on_as("air_", "Luft"),
            Slot::on_as("scd41_", "SCD41").keeping(&["co2"]),
        ] {
            for key in ["co2", "temperature", "humidity"] {
                assert_eq!(
                    slot.prefix_for(key).is_empty(),
                    slot.label_for(key).is_empty(),
                    "{key} is prefixed on only one side"
                );
            }
        }
    }

    #[test]
    fn a_node_carrying_both_i2c_sensors_gives_the_room_readings_to_the_sht31() {
        // The SCD4x specifies its humidity at ±6 %RH against the SHT31-D's ±2 %,
        // so the plain `temperature`/`humidity` entities — the ones a dashboard
        // reaches for — must come from the SHT31-D.
        for name in ["climate-co2", "air-quality"] {
            let node = flavor(name).unwrap();
            assert!(node.sht31.enabled, "{name} has no SHT31-D");
            assert!(node.scd41.enabled, "{name} has no SCD41");
            assert_eq!(node.sht31.prefix_for("humidity"), "");
            assert_ne!(node.scd41.prefix_for("humidity"), "");
            // ...while CO₂, which only one of them measures, stays put.
            assert_eq!(node.scd41.prefix_for("co2"), "");
        }
    }

    #[test]
    fn every_flavor_resolves_and_nothing_else_does() {
        for (name, cfg) in FLAVORS {
            assert_eq!(flavor(name).expect("in the table").flavor, cfg.flavor);
        }
        // The room names boards were built and provisioned under before
        // flavors existed. None may resolve: a board still carrying one in its
        // provisioning sector has to fall back to what it was built as.
        for name in [
            "",
            "Climate",
            "climate ",
            "kueche",
            "bad",
            "schlafzimmer",
            "wohnzimmer",
            "terrasse",
            "solarleuchte",
            "draussen",
            "scale",
        ] {
            assert!(flavor(name).is_none(), "{name:?} resolved");
        }
    }

    #[test]
    fn known_flavors_lists_exactly_the_table() {
        // The message a mistyped `FLAVOR=` prints. It has to be a literal (it
        // is used in a const-eval panic), so nothing but this test ties it to
        // `FLAVORS`.
        let listed: Vec<&str> = KNOWN_FLAVORS.split(", ").collect();
        let actual: Vec<&str> = FLAVORS.iter().map(|(name, _)| *name).collect();
        assert_eq!(listed, actual);
    }

    #[test]
    fn every_flavor_is_its_own_default_identity() {
        // A plain build, with no deployment around it, publishes under the
        // flavor's own name -- so the key, the flavor and the id agree, and two
        // flavors never share one.
        for (i, (name, cfg)) in FLAVORS.iter().enumerate() {
            assert_eq!(*name, cfg.flavor);
            assert_eq!(*name, cfg.id);
            for (other, _) in &FLAVORS[i + 1..] {
                assert_ne!(name, other);
            }
        }
    }

    #[test]
    fn every_flavor_fits_the_flash_slot() {
        // A name that does not fit could be built but never provisioned.
        for (name, _) in FLAVORS {
            assert!(
                name.len() <= crate::config::NODE_NAME_MAX,
                "{name} is too long to store"
            );
        }
    }

    #[test]
    fn a_deployment_identity_replaces_only_id_and_name() {
        let kitchen = identify(CLIMATE, Some("kitchen"), Some("Kitchen"));
        assert_eq!(
            (kitchen.id, kitchen.name, kitchen.flavor),
            ("kitchen", "Kitchen", "climate")
        );
        assert_eq!(kitchen.sample_secs, CLIMATE.sample_secs);
        assert!(kitchen.sht31.enabled && !kitchen.scd41.enabled);
        assert_eq!(
            kitchen.state_topic("", "humidity").as_str(),
            "smarthome/kitchen/humidity"
        );
        // No name: the id is the next best thing to show.
        assert_eq!(identify(CLIMATE, Some("kitchen"), None).name, "kitchen");
        // Nothing: the flavor's own.
        let plain = identify(CLIMATE, None, None);
        assert_eq!((plain.id, plain.name), ("climate", "Climate"));
    }

    #[test]
    fn the_longest_identity_fits_every_topic() {
        let id = "abcdefghijklmnop";
        assert_eq!(id.len(), NODE_ID_MAX);
        for (_, cfg) in FLAVORS {
            let node = identify(*cfg, Some(id), None);
            assert!(node
                .state_topic("battery_", "heartbeat_interval")
                .ends_with("heartbeat_interval"));
            assert!(node.config_wildcard().ends_with('#'));
            assert!(node.ota_offer_topic().ends_with("/ota/offer"));
            assert!(node.ota_version_topic().ends_with("/ota/version"));
            assert!(node.meta_topic().ends_with("/meta/board"));
            assert_eq!(node.client_id().len(), 3 + id.len());
        }
    }

    #[test]
    fn ids_and_names_are_checked() {
        for ok in ["kitchen", "node-1", "a_b", "abcdefghijklmnop"] {
            assert!(valid_id(ok), "{ok}");
        }
        for bad in ["", "Kueche", "kü", "a b", "a/b", "a+", "abcdefghijklmnopq"] {
            assert!(!valid_id(bad), "{bad}");
        }
        for ok in ["Küche", "Wohnzimmer Süd", "x"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "",
            "a\"b",
            "a\\b",
            "line\nbreak",
            "this name is far too long!",
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn topics_are_built_from_the_identity() {
        let outdoor = identify(BIRD_SCALE, Some("garden"), None);
        assert_eq!(
            outdoor.state_topic("", "weight").as_str(),
            "smarthome/garden/weight"
        );
        assert_eq!(
            outdoor.state_topic("air_", "temperature").as_str(),
            "smarthome/garden/air_temperature"
        );
        assert_eq!(outdoor.config_prefix().as_str(), "smarthome/garden/config/");
        assert_eq!(
            outdoor.config_wildcard().as_str(),
            "smarthome/garden/config/#"
        );
        assert_eq!(outdoor.client_id().as_str(), "rs-garden");
        assert_eq!(
            outdoor.availability_topic().as_str(),
            "smarthome/garden/status"
        );
    }

    #[test]
    fn no_topic_is_truncated() {
        // The heapless buffers are fixed; a longer node would silently lose the
        // tail of its topic and publish somewhere unexpected.
        for (_, node) in FLAVORS {
            let longest_key = "heartbeat_interval";
            assert!(node.state_topic("air_", longest_key).ends_with(longest_key));
            assert!(node.config_wildcard().ends_with('#'));
            assert!(node.client_id().starts_with("rs-"));
        }
    }

    #[test]
    fn the_climate_flavor_sleeps_without_changing_what_it_publishes() {
        // The whole point of duty-cycling it: the board stops heating the air
        // its own SHT31 is measuring. What must *not* change is the cadence —
        // the boards running it have years of history at 120 s, and a node
        // that starts reporting every ten minutes instead has quietly thrown
        // that continuity away to fix a different problem.
        let id = "climate";
        let node = flavor(id).expect("in the table");
        assert!(node.power.deep_sleeps(), "{id} is still staying awake");
        assert!(!node.power.is_battery(), "{id} is on a cable, not a cell");
        assert_eq!(node.sample_secs, 120, "{id} changed cadence");
        // Asleep between rounds, so a will would declare it dead after
        // every publish; Home Assistant expires it instead.
        assert!(!node.uses_lwt(), "{id} kept a last-will it cannot honour");
    }

    #[test]
    fn the_power_profile_decides_sleep_and_availability() {
        for (name, node) in FLAVORS {
            // A sleeping node is offline by design between readings, so a last
            // will would declare it dead after every single publish. The
            // question is whether it sleeps, not what it is plugged into —
            // `climate` is on a cable and still takes the expiry path.
            assert_eq!(
                node.uses_lwt(),
                !node.power.deep_sleeps(),
                "{name} availability does not match its power profile"
            );
            // Single-shot CO₂ wherever the node sleeps; periodic is what ASC
            // expects, and it only means anything on a node that stays up.
            assert_eq!(
                node.scd41_mode() == crate::sensors::scd41::Mode::SingleShot,
                node.power.deep_sleeps(),
                "{name} SCD41 mode does not match its power profile"
            );
        }
    }

    #[test]
    fn bus_setup_follows_the_populated_sensors() {
        for (name, node) in FLAVORS {
            assert_eq!(
                node.uses_i2c(),
                node.sht31.enabled || node.scd41.enabled,
                "{name} I²C"
            );
            assert_eq!(node.uses_uart(), node.sds011.enabled, "{name} UART");
        }
    }

    #[test]
    fn the_fan_and_continuous_co2_never_land_on_battery() {
        // Both rule out deep sleep: the SDS011's fan has to spin up per sample,
        // and the SCD41's self-calibration assumes it keeps running.
        for (name, node) in FLAVORS {
            if node.sds011.enabled {
                assert!(!node.power.is_battery(), "{name} runs a fan on battery");
            }
        }
    }

    #[test]
    fn provision_topic_is_keyed_by_mac() {
        let topic = provision_topic([0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6]);
        assert_eq!(topic.as_str(), "smarthome/provision/a1b2c3d4e5f6");
        // Leading zeroes must survive, or two boards could share a topic.
        let topic = provision_topic([0x00, 0x01, 0x02, 0x03, 0x04, 0x05]);
        assert_eq!(topic.as_str(), "smarthome/provision/000102030405");
        assert!(topic.starts_with(PROVISION_PREFIX));
    }

    #[test]
    fn slots_disambiguate_colliding_keys() {
        // The bedroom has two temperature and humidity sources. Without a prefix
        // they would publish to the same topics and one would overwrite the
        // other.
        let bedroom = flavor("climate-co2").unwrap();
        assert!(bedroom.scd41.enabled && bedroom.sht31.enabled);
        assert_ne!(bedroom.scd41.prefix, bedroom.sht31.prefix);
    }

    #[test]
    fn nothing_double_books_the_one_pin_two_things_want() {
        // The runtime half of the const assert above: D2 belongs to the DS18B20
        // or to the battery divider, never both.
        for (name, node) in FLAVORS {
            assert!(
                !(node.ds18b20.enabled && node.battery.enabled),
                "{name} claims D2 twice"
            );
        }
    }

    #[test]
    fn only_a_battery_node_senses_a_cell() {
        // A mains node has no cell to read, so the entity would sit at whatever
        // the divider that is not fitted happens to leave on the pin.
        for (name, node) in FLAVORS {
            if node.battery.enabled {
                assert!(
                    node.power.is_battery(),
                    "{name} senses a battery it does not run on"
                );
            }
        }
    }

    #[test]
    fn the_outdoor_node_only_claims_hardware_it_has() {
        // Slots go on as parts are wired. What is guarded here is the pair
        // that is *not* fitted yet, because enabling either has consequences
        // beyond one more entity: the divider shares D2 with a DS18B20, and a
        // probe on a node that also wants cell voltage fails the build.
        let outdoor = identify(BIRD_SCALE, Some("garden"), None);
        assert!(outdoor.sht31.enabled);
        assert!(outdoor.scale.enabled);
        assert!(outdoor.battery.enabled);
        assert!(!outdoor.ds18b20.enabled);

        // No prefix. The `air_` its predecessor used existed only to leave
        // these keys to a probe, and there is no probe here — re-adding it
        // would put the entities somewhere nobody is looking.
        assert_eq!(outdoor.sht31.prefix, "");
        assert_eq!(
            outdoor
                .state_topic(outdoor.sht31.prefix_for("temperature"), "temperature")
                .as_str(),
            "smarthome/garden/temperature"
        );
    }

    // --- Provisioning -------------------------------------------------------

    #[test]
    fn an_empty_payload_is_not_a_request() {
        // How a retained message is deleted. Acting on it would re-provision
        // every board whose provisioning was just cleared.
        let outdoor = flavor("bird-scale").unwrap();
        assert_eq!(provision_request("", &outdoor, true), None);
        assert_eq!(provision_request("", &outdoor, false), None);
    }

    #[test]
    fn reset_only_does_something_when_there_is_an_override() {
        let outdoor = flavor("bird-scale").unwrap();
        assert_eq!(
            provision_request(PROVISION_RESET, &outdoor, true),
            Some(Provision::Reset)
        );
        // Nothing stored: the message is retained and arrives on every connect,
        // so obeying it would erase the same sector for ever.
        assert_eq!(provision_request(PROVISION_RESET, &outdoor, false), None);
    }

    #[test]
    fn being_told_what_it_already_is_changes_nothing() {
        let outdoor = flavor("bird-scale").unwrap();
        assert_eq!(provision_request("bird-scale", &outdoor, false), None);
        assert_eq!(provision_request("bird-scale", &outdoor, true), None);
    }

    #[test]
    fn a_different_node_is_adopted() {
        let outdoor = flavor("bird-scale").unwrap();
        for (name, _) in FLAVORS.iter().filter(|(n, _)| *n != "bird-scale") {
            assert_eq!(
                provision_request(name, &outdoor, false),
                Some(Provision::Become(String::try_from(*name).unwrap())),
                "{name}"
            );
        }
    }

    #[test]
    fn an_unknown_name_is_ignored() {
        let outdoor = flavor("bird-scale").unwrap();
        // Room names from before flavors, and the retired outdoor node's
        // `draussen` / `scale`; a stale provisioning sector carrying any of
        // them must be ignored.
        for value in [
            "terrasse",
            "kueche",
            "Bird-scale",
            "draussen",
            "scale",
            "  ",
            "0",
        ] {
            assert_eq!(provision_request(value, &outdoor, false), None, "{value}");
        }
    }

    #[test]
    fn a_board_is_matched_by_flavor_not_by_id() {
        // A deployed board's id never equals its flavor's name. Comparing ids
        // would make every retained provisioning message look like a change,
        // and the board would rewrite its flash and reboot on every connect.
        for (name, cfg) in FLAVORS {
            let deployed = identify(*cfg, Some("somewhere"), Some("Somewhere"));
            assert_eq!(provision_request(name, &deployed, false), None, "{name}");
            assert_eq!(provision_request(name, &deployed, true), None, "{name}");
        }
    }

    #[test]
    fn a_slot_with_no_period_of_its_own_runs_every_round() {
        for base in [1, 60, 120, 900] {
            assert_eq!(Slot::on().rounds_between(base), 1);
            assert_eq!(Slot::on().effective_secs(base), base);
        }
    }

    #[test]
    fn a_slow_slot_rounds_up_to_whole_base_rounds() {
        // The loop only wakes on the base cadence, so a period that is not a
        // multiple of it has to land somewhere. Up, never down: the fan is the
        // reason the knob exists, and running it early defeats the purpose.
        let base = 60;
        assert_eq!(Slot::on().every(900).rounds_between(base), 15);
        assert_eq!(Slot::on().every(900).effective_secs(base), 900);
        assert_eq!(Slot::on().every(100).rounds_between(base), 2);
        assert_eq!(Slot::on().every(100).effective_secs(base), 120);
        // A period shorter than one round cannot be honoured; every round is
        // the closest the loop can get, and it is never *slower* than asked.
        assert_eq!(Slot::on().every(5).rounds_between(base), 1);
        assert_eq!(Slot::on().every(60).rounds_between(base), 1);
        assert_eq!(Slot::on().every(61).rounds_between(base), 2);
    }

    #[test]
    fn a_zero_base_never_divides_by_zero() {
        // `sample_secs` is clamped to at least 1 by the loop, but the table is
        // hand-written and a 0 there must not take the arithmetic with it.
        assert_eq!(Slot::on().every(900).rounds_between(0), 900);
        assert_eq!(Slot::on().effective_secs(0), 1);
    }

    #[test]
    fn every_fleet_period_is_a_whole_number_of_base_rounds() {
        // Otherwise the node publishes on a cadence nobody wrote down: the
        // table says 900 s and the sensor actually runs every 960.
        for (name, node) in FLAVORS {
            for (what, slot) in [
                ("sht31", node.sht31),
                ("scd41", node.scd41),
                ("sds011", node.sds011),
            ] {
                if slot.period_secs == 0 {
                    continue;
                }
                assert_eq!(
                    slot.effective_secs(node.sample_secs),
                    slot.period_secs as u64,
                    "{name}/{what}: {} s does not divide into {} s rounds",
                    slot.period_secs,
                    node.sample_secs
                );
            }
        }
    }
}

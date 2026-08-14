//! SDS011 particulate-matter driver (UART, #15).
//!
//! Bus: UART **9600 8N1**, 3.3 V TTL logic (the sensor itself is powered from
//! 5 V; its TX is already 3.3 V-safe for the ESP32-C3 RX).
//!
//! Measurement frame (10 bytes), reported once per second in active mode:
//!   `AA C0 PM25_L PM25_H PM10_L PM10_H ID1 ID2 CHK AB`
//! where `CHK = (sum of bytes[2..=7]) & 0xFF` and:
//!   PM2.5[µg/m³] = (PM25_H·256 + PM25_L) / 10
//!   PM10 [µg/m³] = (PM10_H·256 + PM10_L) / 10
//!
//! **Fan duty-cycling (important):** the laser + fan are rated ~8000 h, so we
//! must not run them continuously. Command frame `AA B4 06 01 <mode> ... AB`
//! sets sleep/work; the cycle is: wake fan -> wait ~15–30 s for the airflow to
//! stabilise -> read a frame -> put the sensor back to sleep. The sensor must
//! also be kept dry (condensation ruins both reading and hardware).

use embassy_time::{with_timeout, Duration, Timer};
use embedded_io_async::{Read, Write};
use heapless::{String, Vec};

use super::{write_tenths, EntityDescriptor, Reading, Sensor, MAX_READINGS};

/// Frame markers.
pub const HEAD: u8 = 0xAA;
pub const TAIL: u8 = 0xAB;
/// Command byte for a measurement-data reply.
pub const CMD_DATA: u8 = 0xC0;
pub const FRAME_LEN: usize = 10;
/// Length of a host->sensor command frame.
pub const CMD_FRAME_LEN: usize = 19;
/// Seconds to run the fan before trusting a reading.
pub const WARMUP_SECS: u64 = 20;
/// How long to wait for a measurement frame once the fan has warmed up. The
/// sensor reports every ~1 s, so this is generous; exceeding it means silence.
pub const FRAME_TIMEOUT: Duration = Duration::from_secs(5);
/// A gap this long with no byte means the receive buffer is empty, i.e. the
/// stale pre-warm-up frames have been flushed (frames arrive ~1 s apart).
const DRAIN_QUIET: Duration = Duration::from_millis(100);

pub const DESCRIPTORS: &[EntityDescriptor] = &[
    EntityDescriptor {
        key: "pm25",
        name: "PM2.5",
        unit: "µg/m³",
        device_class: "pm25",
        state_class: "measurement",
    },
    EntityDescriptor {
        key: "pm10",
        name: "PM10",
        unit: "µg/m³",
        device_class: "pm10",
        state_class: "measurement",
    },
];

/// Little-endian PM word -> µg/m³ in **tenths** (raw is already tenths of µg/m³).
pub const fn pm_tenths(lo: u8, hi: u8) -> i32 {
    (hi as i32) * 256 + lo as i32
}

/// Validate a 10-byte frame: markers, command byte, and checksum.
pub fn frame_ok(f: &[u8; FRAME_LEN]) -> bool {
    if f[0] != HEAD || f[1] != CMD_DATA || f[FRAME_LEN - 1] != TAIL {
        return false;
    }
    let sum = f[2]
        .wrapping_add(f[3])
        .wrapping_add(f[4])
        .wrapping_add(f[5])
        .wrapping_add(f[6])
        .wrapping_add(f[7]);
    sum == f[8]
}

/// Build the sleep/work command frame: `AA B4 06 01 <mode> 00×10 FF FF CHK AB`,
/// where `mode` is 1 for "work" (fan + laser on) and 0 for "sleep", the two
/// `FF`s address every sensor on the line, and `CHK` sums bytes 2..=16.
pub const fn sleep_work_frame(work: bool) -> [u8; CMD_FRAME_LEN] {
    let mut f = [0u8; CMD_FRAME_LEN];
    f[0] = HEAD;
    f[1] = 0xB4;
    f[2] = 0x06; // set sleep/work
    f[3] = 0x01; // 1 = set value (0 would only query)
    f[4] = work as u8;
    f[15] = 0xFF; // device id: broadcast
    f[16] = 0xFF;
    let mut sum: u8 = 0;
    let mut i = 2;
    while i <= 16 {
        sum = sum.wrapping_add(f[i]);
        i += 1;
    }
    f[17] = sum;
    f[18] = TAIL;
    f
}

/// SDS011 on a UART, generic over the byte stream so the driver stays
/// HAL-agnostic (esp-hal's async `Uart` implements `embedded-io-async`).
pub struct Sds011<U> {
    uart: U,
    /// Seconds the fan runs before a frame is trusted. Configurable so a mains
    /// node that samples rarely can afford a longer, more accurate warm-up.
    pub warmup_secs: u64,
}

impl<U: Read + Write> Sds011<U> {
    pub fn new(uart: U) -> Self {
        Self {
            uart,
            warmup_secs: WARMUP_SECS,
        }
    }

    /// Turn the fan + laser on (`true`) or park the sensor (`false`).
    async fn set_work(&mut self, work: bool) -> Option<()> {
        self.uart.write_all(&sleep_work_frame(work)).await.ok()?;
        self.uart.flush().await.ok()?;
        Some(())
    }

    /// Read a single byte, awaiting the UART until one arrives.
    async fn read_byte(&mut self) -> Option<u8> {
        let mut b = [0u8; 1];
        loop {
            match self.uart.read(&mut b).await {
                Ok(0) => continue,
                Ok(_) => return Some(b[0]),
                Err(_) => return None,
            }
        }
    }

    /// Throw away whatever is already buffered: frames the sensor emitted
    /// during (or before) the warm-up describe air we don't want to report, and
    /// the reply to a sleep/work command sits in the same stream.
    async fn drain(&mut self) {
        let mut scratch = [0u8; 64];
        while with_timeout(DRAIN_QUIET, self.uart.read(&mut scratch))
            .await
            .is_ok()
        {}
    }

    /// Resynchronise on `HEAD` and read one complete, checksum-valid frame.
    /// Loops past malformed frames; the caller bounds it with a timeout.
    async fn read_frame(&mut self) -> Option<[u8; FRAME_LEN]> {
        loop {
            // Hunt for the start marker; anything else is mid-frame noise.
            while self.read_byte().await? != HEAD {}

            let mut frame = [0u8; FRAME_LEN];
            frame[0] = HEAD;
            for slot in frame.iter_mut().skip(1) {
                *slot = self.read_byte().await?;
            }
            if frame_ok(&frame) {
                return Some(frame);
            }
        }
    }

    fn push(readings: &mut Vec<Reading, MAX_READINGS>, key: &'static str, tenths: i32) {
        let mut value = String::new();
        write_tenths(&mut value, tenths);
        let _ = readings.push(Reading { key, value });
    }
}

impl<U: Read + Write> Sensor for Sds011<U> {
    fn kind(&self) -> &'static str {
        "SDS011"
    }

    fn descriptors(&self) -> &'static [EntityDescriptor] {
        DESCRIPTORS
    }

    async fn measure(&mut self) -> Vec<Reading, MAX_READINGS> {
        let mut out = Vec::new();

        if self.set_work(true).await.is_none() {
            return out;
        }
        // Let the airflow stabilise before believing anything the sensor says,
        // then discard everything it said while it was still spinning up.
        Timer::after(Duration::from_secs(self.warmup_secs)).await;
        self.drain().await;

        let frame = with_timeout(FRAME_TIMEOUT, self.read_frame()).await;

        // Park the fan again whatever happened — its 8000 h life is the scarce
        // resource here, so it must never be left running by an error path.
        let _ = self.set_work(false).await;

        if let Ok(Some(f)) = frame {
            Self::push(&mut out, "pm25", pm_tenths(f[2], f[3]));
            Self::push(&mut out, "pm10", pm_tenths(f[4], f[5]));
        }
        out
    }
}

const _: () = {
    // 0x00F5 = 245 -> 24.5 µg/m³ (245 tenths)
    assert!(pm_tenths(0xF5, 0x00) == 245);
    assert!(pm_tenths(0x00, 0x01) == 256);
    // Checksum of the canonical "start working" frame from the protocol sheet.
    let work = sleep_work_frame(true);
    assert!(
        work[4] == 1
            && work[17]
                == 0x06u8
                    .wrapping_add(0x01)
                    .wrapping_add(0x01)
                    .wrapping_add(0xFF)
                    .wrapping_add(0xFF)
    );
    let sleep = sleep_work_frame(false);
    assert!(sleep[4] == 0 && sleep[18] == TAIL);
};

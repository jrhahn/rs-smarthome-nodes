//! Fake buses for the driver tests.
//!
//! The drivers are generic over `embedded-hal-async` (I²C) and
//! `embedded-io-async` (UART) rather than over esp-hal types, which was
//! motivated by keeping them portable — this is the other half of that payoff:
//! the *real* drivers can be run on the host against a scripted bus, so the
//! byte-level protocol work (resync, CRCs, the fan duty cycle, what happens when
//! a sensor is absent) is covered without a board on the desk.
//!
//! What these do **not** model is timing, bus contention or electrical failure.
//! A passing test here says the driver handles the bytes correctly; it says
//! nothing about whether the SHT31 answers within 15 ms on the real bus. That
//! remains a bench question.

use std::collections::VecDeque;

use embedded_hal_async::i2c::{ErrorKind, ErrorType, I2c, Operation, SevenBitAddress};
use embedded_io_async::{ErrorType as IoErrorType, Read, Write};

/// A bus error. Which kind hardly matters: every driver treats any error as
/// "the sensor did not answer" and contributes no reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusError;

impl embedded_hal_async::i2c::Error for BusError {
    fn kind(&self) -> ErrorKind {
        ErrorKind::Other
    }
}

impl embedded_io_async::Error for BusError {
    fn kind(&self) -> embedded_io_async::ErrorKind {
        embedded_io_async::ErrorKind::Other
    }
}

// --- I²C ---------------------------------------------------------------------

/// One thing that happened on the bus, recorded so a test can assert on the
/// commands a driver actually issued rather than only on what it returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum I2cEvent {
    Write { addr: u8, data: Vec<u8> },
    Read { addr: u8, len: usize },
}

/// An I²C bus with canned replies.
///
/// Reads are served from `replies` in order. A device that is not present is
/// modelled by `nack`, which fails every transaction the way a missing sensor
/// does — with a NACK, not with silence.
pub struct FakeI2c {
    replies: VecDeque<Vec<u8>>,
    /// Addresses that answer. Anything else NACKs, which is what the bus probe
    /// in `platform.rs` relies on to find the sensor.
    present: Vec<u8>,
    pub events: Vec<I2cEvent>,
}

impl FakeI2c {
    /// A bus with one device at `addr` that will answer with `replies`, in order.
    pub fn new(addr: u8, replies: impl IntoIterator<Item = Vec<u8>>) -> Self {
        Self {
            replies: replies.into_iter().collect(),
            present: vec![addr],
            events: Vec::new(),
        }
    }

    /// A bus with nothing on it: every transaction NACKs.
    pub fn empty() -> Self {
        Self {
            replies: VecDeque::new(),
            present: Vec::new(),
            events: Vec::new(),
        }
    }

    /// Every byte sequence written, in order.
    pub fn writes(&self) -> Vec<&[u8]> {
        self.events
            .iter()
            .filter_map(|e| match e {
                I2cEvent::Write { data, .. } => Some(data.as_slice()),
                _ => None,
            })
            .collect()
    }
}

impl ErrorType for FakeI2c {
    type Error = BusError;
}

impl I2c<SevenBitAddress> for FakeI2c {
    async fn transaction(
        &mut self,
        address: SevenBitAddress,
        operations: &mut [Operation<'_>],
    ) -> Result<(), Self::Error> {
        if !self.present.contains(&address) {
            // Record the attempt anyway: a probe wants to know it was tried.
            for op in operations.iter() {
                self.events.push(match op {
                    Operation::Write(data) => I2cEvent::Write {
                        addr: address,
                        data: data.to_vec(),
                    },
                    Operation::Read(buf) => I2cEvent::Read {
                        addr: address,
                        len: buf.len(),
                    },
                });
            }
            return Err(BusError);
        }

        for op in operations.iter_mut() {
            match op {
                Operation::Write(data) => self.events.push(I2cEvent::Write {
                    addr: address,
                    data: data.to_vec(),
                }),
                Operation::Read(buf) => {
                    self.events.push(I2cEvent::Read {
                        addr: address,
                        len: buf.len(),
                    });
                    let reply = self.replies.pop_front().ok_or(BusError)?;
                    if reply.len() != buf.len() {
                        // A real sensor clocks out exactly as many bytes as it
                        // is asked for; a test script that disagrees is a bug in
                        // the test, so say so loudly.
                        panic!(
                            "scripted reply is {} bytes, driver asked for {}",
                            reply.len(),
                            buf.len()
                        );
                    }
                    buf.copy_from_slice(&reply);
                }
            }
        }
        Ok(())
    }
}

// --- UART --------------------------------------------------------------------

/// One stretch of bytes the sensor sends, and whether the line falls quiet
/// before it.
///
/// The gap matters: the SDS011 driver tells "stale frames buffered during the
/// fan warm-up" from "the frame I actually want" purely by draining until the
/// line goes quiet. A mock that handed over everything at once would never
/// exercise that.
pub struct Segment {
    pub data: Vec<u8>,
    pub quiet_before: bool,
}

impl Segment {
    pub fn now(data: Vec<u8>) -> Self {
        Self {
            data,
            quiet_before: false,
        }
    }

    pub fn after_a_gap(data: Vec<u8>) -> Self {
        Self {
            data,
            quiet_before: true,
        }
    }
}

/// A UART playing back a script.
///
/// When it runs dry it returns `Pending` for ever, which is what a real UART
/// does — that is what lets the driver's `with_timeout` fire. (It parks without
/// registering a waker, which is only sound because every read in the driver is
/// wrapped in a timeout whose timer *does* register one.)
pub struct FakeUart {
    current: VecDeque<u8>,
    segments: VecDeque<Segment>,
    pub written: Vec<Vec<u8>>,
    /// Fail every write, as a sensor that is not wired up would.
    pub write_fails: bool,
}

impl FakeUart {
    pub fn new(segments: impl IntoIterator<Item = Segment>) -> Self {
        Self {
            current: VecDeque::new(),
            segments: segments.into_iter().collect(),
            written: Vec::new(),
            write_fails: false,
        }
    }

    /// A sensor that is not connected: writing to it fails.
    pub fn disconnected() -> Self {
        let mut uart = Self::new([]);
        uart.write_fails = true;
        uart
    }

    /// Every command frame the driver sent.
    pub fn commands(&self) -> &[Vec<u8>] {
        &self.written
    }
}

impl IoErrorType for FakeUart {
    type Error = BusError;
}

impl Read for FakeUart {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        // Never completes with zero bytes: a real UART waits for one instead,
        // and a mock that returned `Ok(0)` would let a driver bug hide behind
        // the loop bound rather than showing up as the hang it really is.
        while self.current.is_empty() {
            match self.segments.pop_front() {
                Some(segment) => {
                    self.current = segment.data.into();
                    if segment.quiet_before {
                        // The line is idle until the driver's timeout gives up
                        // on it; the timeout drops this future, and the bytes
                        // are waiting for the read that comes after.
                        core::future::pending::<()>().await;
                    }
                }
                None => core::future::pending::<()>().await,
            }
        }
        let n = buf.len().min(self.current.len());
        for slot in buf.iter_mut().take(n) {
            *slot = self.current.pop_front().unwrap();
        }
        Ok(n)
    }
}

impl Write for FakeUart {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        if self.write_fails {
            return Err(BusError);
        }
        self.written.push(buf.to_vec());
        Ok(buf.len())
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        if self.write_fails {
            return Err(BusError);
        }
        Ok(())
    }
}

/// A UART that always completes a read with zero bytes — the pathological case
/// a naive driver loop would spin on for ever without yielding, so its timeout
/// could never fire.
pub struct StarvedUart;

impl IoErrorType for StarvedUart {
    type Error = BusError;
}

impl Read for StarvedUart {
    async fn read(&mut self, _buf: &mut [u8]) -> Result<usize, Self::Error> {
        Ok(0)
    }
}

impl Write for StarvedUart {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        Ok(buf.len())
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Run a driver future to completion on the host.
pub fn block_on<F: core::future::Future>(future: F) -> F::Output {
    futures_executor::block_on(future)
}

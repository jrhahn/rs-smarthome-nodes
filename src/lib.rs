//! rs-smarthome-nodes — the firmware's library half.
//!
//! Everything the binary is made of lives here; `main.rs` is only the entry
//! point, the peripheral wiring and the publish loop. The split exists so the
//! parts that are **pure computation** can be compiled — and therefore tested —
//! on the host:
//!
//! * frame decoding and CRCs (SDS011, Sensirion, Dallas, CRC-32),
//! * the flash blob layouts and their corruption handling,
//! * `Config::apply`, i.e. every retained value Home Assistant can send,
//! * the Home Assistant discovery payloads,
//! * the node table and the topics derived from it.
//!
//! That is most of the logic that can be silently *wrong* rather than failing to
//! build, and none of it needs a bus. Anything that does touch a peripheral —
//! the drivers' bus I/O, RTC RAM, flash, Wi-Fi — sits behind the `hal` feature,
//! which is on by default, so a normal `cargo build` is unaffected. With it off
//! the crate builds for the host and `cargo test` works:
//!
//! ```text
//! cargo test --lib --no-default-features --target x86_64-unknown-linux-gnu
//! ```
//!
//! Compile-time `const _: () = assert!(…)` checks stay where they are. They
//! cost nothing, they fail the *build* rather than a test run, and they cover
//! invariants a test cannot reach (that a node name fits its flash slot, say).
//! The tests cover what const-eval cannot: anything allocating a `String`,
//! negative cases, and inputs enumerated in a loop.

#![cfg_attr(not(test), no_std)]
// The library exposes plenty the binary happens not to use (helper accessors,
// protocol constants kept for symmetry); that is not dead code, it is the API.
#![allow(dead_code)]

pub mod battery;
pub mod config;
pub mod discovery;
pub mod ds18b20;
pub mod node;
pub mod presence;
pub mod sensors;
pub mod wifi;

#[cfg(feature = "drivers")]
pub mod hx711;
#[cfg(feature = "hal")]
pub mod platform;
#[cfg(feature = "hal")]
pub mod state;

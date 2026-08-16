//! Wi-Fi credentials: where they come from, and how to change them without a
//! rebuild.
//!
//! Credentials used to be compile-time only, which made the one failure mode
//! that matters unrecoverable in the field: a board that cannot join the
//! network cannot be told anything over the network either. MQTT provisioning
//! (as used for the node identity, see [`crate::node`]) is no help — reaching
//! the broker is the very thing that is broken.
//!
//! So the escape hatch is the **serial console**, the one channel that works
//! when nothing else does. Plug the board into USB, and on a cold boot it
//! listens briefly for:
//!
//! ```text
//! ssid MyNetwork
//! psk hunter2
//! save
//! ```
//!
//! and restarts into them. `clear` forgets them again and returns the board to
//! the credentials it was flashed with. Physical access already implies the
//! ability to reflash, so this gives away nothing that a USB cable did not
//! already give away.
//!
//! Resolution order at boot:
//!
//! 1. credentials stored in flash, if the sector checks out;
//! 2. otherwise the ones compiled in (`SSID=` / `PASSWORD=`).
//!
//! Stored credentials are not trusted blindly: after
//! [`FALLBACK_AFTER`] consecutive failed joins the connection task falls back
//! to the build-time pair for the rest of the run. That is the net that stops a
//! mistyped passphrase from taking a board off the network permanently — worth
//! having, because unlike a wrong node name a wrong PSK cannot be corrected
//! over the air.

use core::fmt::Write as _;
use core::ptr::addr_of;
#[cfg(feature = "hal")]
use core::ptr::addr_of_mut;

use heapless::String;

use crate::config::{PSK_MAX, SSID_MAX};

/// Placeholder the firmware is built with when no `SSID=` was given. A board
/// carrying this cannot join anything, which is what makes it the signal to
/// wait for the console rather than press on.
pub const PLACEHOLDER_SSID: &str = "your-ssid";

/// Consecutive failed joins after which the stored credentials are set aside in
/// favour of the build-time ones. Three, for the same reason three missed
/// rounds mark a node absent: one failure is an access point rebooting, three
/// is credentials that do not work.
pub const FALLBACK_AFTER: u32 = 3;

/// One network's credentials.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Credentials {
    pub ssid: String<SSID_MAX>,
    pub psk: String<PSK_MAX>,
}

impl Credentials {
    /// Build a pair, rejecting anything that will not fit the fixed buffers
    /// rather than truncating it into a different network.
    pub fn new(ssid: &str, psk: &str) -> Option<Credentials> {
        if ssid.is_empty() {
            return None;
        }
        Some(Credentials {
            ssid: String::try_from(ssid).ok()?,
            psk: String::try_from(psk).ok()?,
        })
    }

    /// Does this name a network at all? A board holding the build-time
    /// placeholder has nothing to try.
    pub fn is_placeholder(&self) -> bool {
        self.ssid.is_empty() || self.ssid == PLACEHOLDER_SSID
    }
}

/// Where the credentials in force came from — worth logging, since "it joined"
/// and "it joined with what I just typed" are different facts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// Stored in flash by console provisioning.
    Stored,
    /// Compiled in via `SSID=` / `PASSWORD=`.
    BuiltIn,
}

/// Pick the credentials to use, given what flash holds and what the image was
/// built with. Stored ones win; a stored pair that names nothing is ignored,
/// so a corrupt or half-written sector degrades to the build-time pair rather
/// than to a board that tries to join `""`.
pub fn resolve(stored: Option<Credentials>, built_in: Credentials) -> (Credentials, Source) {
    match stored {
        Some(credentials) if !credentials.is_placeholder() => (credentials, Source::Stored),
        _ => (built_in, Source::BuiltIn),
    }
}

// --- The credentials in force ------------------------------------------------

/// Resolved once at boot and then only read, like [`crate::node`]'s identity.
static mut ACTIVE: Option<Credentials> = None;
static mut BUILT_IN: Option<Credentials> = None;

/// The credentials in force. `None` before [`init`] has run.
pub fn active() -> Option<Credentials> {
    unsafe { (*addr_of!(ACTIVE)).clone() }
}

/// The credentials this image was compiled with — the fallback the connection
/// task reverts to when the stored ones keep failing.
pub fn built_in() -> Option<Credentials> {
    unsafe { (*addr_of!(BUILT_IN)).clone() }
}

/// Resolve the credentials for this boot. Call once, early, before the radio is
/// brought up.
#[cfg(feature = "hal")]
pub fn init(built_in_credentials: Credentials) -> Source {
    let stored =
        crate::config::load_credentials().and_then(|(ssid, psk)| Credentials::new(&ssid, &psk));
    let (chosen, source) = resolve(stored, built_in_credentials.clone());
    unsafe {
        addr_of_mut!(BUILT_IN).write(Some(built_in_credentials));
        addr_of_mut!(ACTIVE).write(Some(chosen));
    }
    source
}

// --- Console provisioning ----------------------------------------------------

/// Longest console line accepted. An SSID plus a passphrase plus the keyword
/// fits comfortably; anything longer is a paste accident.
pub const LINE_MAX: usize = 128;

/// One command from the serial console.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Command<'a> {
    /// Set the network name.
    Ssid(&'a str),
    /// Set the passphrase. May be empty, for an open network.
    Psk(&'a str),
    /// Store what has been entered and restart into it.
    Save,
    /// Forget the stored credentials, returning to the build-time ones.
    Clear,
    /// Report what is entered so far, without the passphrase.
    Show,
    /// Leave provisioning and carry on booting.
    Done,
    /// Nothing typed.
    Empty,
    /// Anything else.
    Unknown(&'a str),
}

/// Parse one console line.
///
/// The value is taken verbatim after the first space, so a passphrase may
/// contain spaces — only the surrounding whitespace and the line ending are
/// stripped. That matters: silently mangling a passphrase would look exactly
/// like a wrong one.
pub fn parse(line: &str) -> Command<'_> {
    let line = line.trim_matches(|c: char| c == '\r' || c == '\n');
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Command::Empty;
    }
    let (keyword, rest) = match trimmed.split_once(' ') {
        Some((keyword, rest)) => (keyword, rest),
        None => (trimmed, ""),
    };
    match keyword {
        "ssid" => Command::Ssid(rest.trim()),
        // Only the leading space after the keyword is consumed, so a passphrase
        // keeps any internal spacing. Trailing whitespace is stripped, since a
        // terminal that adds it is far likelier than a passphrase that ends in
        // a space.
        "psk" | "pass" | "password" => Command::Psk(rest.trim_end()),
        "save" => Command::Save,
        "clear" => Command::Clear,
        "show" => Command::Show,
        "done" | "exit" | "quit" => Command::Done,
        other => Command::Unknown(other),
    }
}

/// What a provisioning session decided.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Store these and restart.
    Save(Credentials),
    /// Erase the stored credentials and restart.
    Clear,
    /// Carry on booting with whatever was already in force.
    Continue,
}

/// The half-entered state of a provisioning session.
#[derive(Default)]
pub struct Session {
    ssid: String<SSID_MAX>,
    psk: String<PSK_MAX>,
}

impl Session {
    pub fn new() -> Session {
        Session::default()
    }

    /// A one-line description of what is entered, safe to print: the
    /// passphrase's length but never its content, since these lines go to a
    /// log that may well be scrolling in someone else's terminal.
    pub fn summary(&self) -> String<96> {
        let mut out = String::new();
        let _ = write!(
            out,
            "ssid '{}', psk {} characters",
            self.ssid,
            self.psk.len()
        );
        out
    }

    /// Apply one command. `Some` ends the session.
    ///
    /// Rejections are reported rather than silently ignored — a passphrase that
    /// was one character too long to store would otherwise fail as a join error
    /// much later, where nothing points back at the typo.
    pub fn apply(&mut self, command: Command<'_>) -> Result<Option<Outcome>, &'static str> {
        match command {
            Command::Ssid(value) => {
                self.ssid = String::try_from(value).map_err(|_| "ssid too long")?;
                Ok(None)
            }
            Command::Psk(value) => {
                self.psk = String::try_from(value).map_err(|_| "psk too long")?;
                Ok(None)
            }
            Command::Save => match Credentials::new(&self.ssid, &self.psk) {
                Some(credentials) => Ok(Some(Outcome::Save(credentials))),
                None => Err("no ssid entered"),
            },
            Command::Clear => Ok(Some(Outcome::Clear)),
            Command::Done => Ok(Some(Outcome::Continue)),
            Command::Show | Command::Empty => Ok(None),
            Command::Unknown(word) => {
                let _ = word;
                Err("unknown command; expected ssid / psk / save / clear / show / done")
            }
        }
    }
}

// --- Reading the console -----------------------------------------------------

/// Read one line, or `None` on a closed/failed console or when nothing arrives
/// before the deadline.
///
/// Generic over the byte stream for the same reason the sensor drivers are: it
/// keeps esp-hal out of the logic, and lets the whole session be exercised
/// against a scripted console on the host.
#[cfg(feature = "drivers")]
pub async fn read_line<R: embedded_io_async::Read>(
    console: &mut R,
    line: &mut String<LINE_MAX>,
    idle: embassy_time::Duration,
) -> Option<()> {
    use embassy_time::with_timeout;

    line.clear();
    let mut byte = [0u8; 1];
    loop {
        match with_timeout(idle, console.read(&mut byte)).await {
            Ok(Ok(1)) => {}
            // Silence, a closed console, or a read that returned nothing: the
            // same bound the sensor drivers keep, so a console that completes
            // instantly with no bytes cannot spin here for ever.
            _ => return None,
        }
        match byte[0] {
            b'\n' => return Some(()),
            // A bare CR ends a line too; a CRLF's LF then reads as an empty one,
            // which parses to `Empty` and is ignored.
            b'\r' => return Some(()),
            // Backspace, so a typo on a terminal without line editing is
            // recoverable.
            0x08 | 0x7F => {
                line.pop();
            }
            b => {
                // An over-long line is truncated rather than dropped: the user
                // sees the mangled value echoed back by `show` and can retype it,
                // which beats silently discarding what they typed.
                let _ = line.push(b as char);
            }
        }
    }
}

/// Run a provisioning session on the console until it ends or falls silent.
///
/// `idle` bounds the wait for each line. A board that has no usable credentials
/// has nothing better to do, so callers give it a generous window; one that is
/// merely offering the chance to intervene gives it a short one.
#[cfg(feature = "drivers")]
pub async fn provision<R: embedded_io_async::Read>(
    console: &mut R,
    idle: embassy_time::Duration,
) -> Outcome {
    use log::{info, warn};

    info!("wifi provisioning: ssid <name> / psk <passphrase> / save / clear / show / done");
    let mut session = Session::new();
    let mut line: String<LINE_MAX> = String::new();
    while read_line(console, &mut line, idle).await.is_some() {
        let command = parse(&line);
        let show = matches!(command, Command::Show);
        match session.apply(command) {
            Ok(Some(outcome)) => return outcome,
            Ok(None) => {
                if show {
                    info!("wifi: {}", session.summary());
                }
            }
            Err(problem) => warn!("wifi: {}", problem),
        }
    }
    info!("wifi provisioning: console idle, carrying on");
    Outcome::Continue
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials(ssid: &str, psk: &str) -> Credentials {
        Credentials::new(ssid, psk).expect("valid")
    }

    // --- Resolution ---------------------------------------------------------

    #[test]
    fn stored_credentials_win_over_the_built_in_ones() {
        let (chosen, source) = resolve(
            Some(credentials("Stored", "stored-psk")),
            credentials("BuiltIn", "built-in-psk"),
        );
        assert_eq!(chosen.ssid.as_str(), "Stored");
        assert_eq!(source, Source::Stored);
    }

    #[test]
    fn nothing_stored_falls_back_to_the_built_in_ones() {
        let (chosen, source) = resolve(None, credentials("BuiltIn", "psk"));
        assert_eq!(chosen.ssid.as_str(), "BuiltIn");
        assert_eq!(source, Source::BuiltIn);
    }

    #[test]
    fn a_stored_placeholder_is_not_treated_as_credentials() {
        // A sector that decoded but names nothing must not beat a working
        // build-time pair — that would take a board off the network for the
        // sake of a blob that cannot join anything.
        let (chosen, source) = resolve(
            Credentials::new(PLACEHOLDER_SSID, ""),
            credentials("BuiltIn", "psk"),
        );
        assert_eq!(chosen.ssid.as_str(), "BuiltIn");
        assert_eq!(source, Source::BuiltIn);
    }

    #[test]
    fn an_empty_ssid_is_never_credentials() {
        assert!(Credentials::new("", "psk").is_none());
        assert!(!credentials("Net", "").is_placeholder());
        assert!(Credentials::new(PLACEHOLDER_SSID, "")
            .expect("valid")
            .is_placeholder());
    }

    #[test]
    fn an_open_network_needs_no_passphrase() {
        let open = credentials("OpenNet", "");
        assert!(!open.is_placeholder());
        assert_eq!(open.psk.len(), 0);
    }

    #[test]
    fn credentials_that_do_not_fit_are_rejected_rather_than_truncated() {
        // A truncated SSID names a different network; a truncated passphrase
        // never authenticates. Either would surface as an unexplained join
        // failure much later.
        assert!(Credentials::new(&"x".repeat(SSID_MAX), "psk").is_some());
        assert!(Credentials::new(&"x".repeat(SSID_MAX + 1), "psk").is_none());
        assert!(Credentials::new("Net", &"x".repeat(PSK_MAX)).is_some());
        assert!(Credentials::new("Net", &"x".repeat(PSK_MAX + 1)).is_none());
    }

    // --- Console parsing ----------------------------------------------------

    #[test]
    fn the_keywords_parse() {
        assert_eq!(parse("ssid MyNetwork"), Command::Ssid("MyNetwork"));
        assert_eq!(parse("psk hunter2"), Command::Psk("hunter2"));
        assert_eq!(parse("save"), Command::Save);
        assert_eq!(parse("clear"), Command::Clear);
        assert_eq!(parse("show"), Command::Show);
        assert_eq!(parse("done"), Command::Done);
        assert_eq!(parse(""), Command::Empty);
        assert_eq!(parse("   "), Command::Empty);
    }

    #[test]
    fn a_line_ending_is_not_part_of_the_value() {
        // The obvious way to lose an evening: a passphrase silently carrying a
        // trailing \r from a terminal that sends CRLF.
        assert_eq!(parse("psk hunter2\r\n"), Command::Psk("hunter2"));
        assert_eq!(parse("psk hunter2\n"), Command::Psk("hunter2"));
        assert_eq!(parse("ssid MyNetwork\r"), Command::Ssid("MyNetwork"));
        assert_eq!(parse("save\r\n"), Command::Save);
    }

    #[test]
    fn a_passphrase_may_contain_spaces() {
        // "correct horse battery staple" is a perfectly good passphrase, and
        // mangling it would look exactly like a wrong one.
        assert_eq!(
            parse("psk correct horse battery staple"),
            Command::Psk("correct horse battery staple")
        );
        assert_eq!(parse("psk  leading"), Command::Psk(" leading"));
    }

    #[test]
    fn the_passphrase_keyword_has_the_obvious_synonyms() {
        for line in ["psk hunter2", "pass hunter2", "password hunter2"] {
            assert_eq!(parse(line), Command::Psk("hunter2"));
        }
    }

    #[test]
    fn an_unknown_word_is_reported_rather_than_ignored() {
        assert_eq!(parse("wifi MyNetwork"), Command::Unknown("wifi"));
        assert_eq!(parse("SSID MyNetwork"), Command::Unknown("SSID"));
    }

    #[test]
    fn an_empty_value_is_still_a_command() {
        // Clearing the passphrase for an open network.
        assert_eq!(parse("psk"), Command::Psk(""));
        assert_eq!(parse("psk "), Command::Psk(""));
        assert_eq!(parse("ssid"), Command::Ssid(""));
    }

    // --- Session ------------------------------------------------------------

    #[test]
    fn a_full_session_ends_in_credentials() {
        let mut session = Session::new();
        assert_eq!(session.apply(parse("ssid MyNetwork")), Ok(None));
        assert_eq!(session.apply(parse("psk hunter2")), Ok(None));
        assert_eq!(
            session.apply(parse("save")),
            Ok(Some(Outcome::Save(credentials("MyNetwork", "hunter2"))))
        );
    }

    #[test]
    fn saving_without_an_ssid_is_refused() {
        // Storing a blank SSID would take the board off the network with no way
        // back except the console it is already on — recoverable, but pointless.
        let mut session = Session::new();
        assert_eq!(session.apply(parse("psk hunter2")), Ok(None));
        assert_eq!(session.apply(parse("save")), Err("no ssid entered"));
    }

    #[test]
    fn an_open_network_can_be_saved_with_no_passphrase() {
        let mut session = Session::new();
        assert_eq!(session.apply(parse("ssid OpenNet")), Ok(None));
        assert_eq!(
            session.apply(parse("save")),
            Ok(Some(Outcome::Save(credentials("OpenNet", ""))))
        );
    }

    #[test]
    fn an_overlong_value_is_refused_at_the_point_it_is_typed() {
        let mut session = Session::new();
        let long_ssid = "x".repeat(SSID_MAX + 1);
        let mut line = std::string::String::from("ssid ");
        line.push_str(&long_ssid);
        assert_eq!(session.apply(parse(&line)), Err("ssid too long"));
        // ... and the session is still usable afterwards.
        assert_eq!(session.apply(parse("ssid Short")), Ok(None));
    }

    #[test]
    fn the_last_value_typed_wins() {
        let mut session = Session::new();
        let _ = session.apply(parse("ssid First"));
        let _ = session.apply(parse("ssid Second"));
        assert_eq!(
            session.apply(parse("save")),
            Ok(Some(Outcome::Save(credentials("Second", ""))))
        );
    }

    #[test]
    fn clear_and_done_are_distinct_endings() {
        // `clear` erases the stored pair; `done` leaves everything alone.
        assert_eq!(
            Session::new().apply(parse("clear")),
            Ok(Some(Outcome::Clear))
        );
        assert_eq!(
            Session::new().apply(parse("done")),
            Ok(Some(Outcome::Continue))
        );
    }

    #[test]
    fn an_unknown_command_does_not_end_the_session() {
        let mut session = Session::new();
        assert!(session.apply(parse("halp")).is_err());
        assert_eq!(session.apply(parse("ssid MyNetwork")), Ok(None));
    }

    #[test]
    fn the_summary_never_prints_the_passphrase() {
        // These lines land in a log that may be scrolling in a shared terminal.
        let mut session = Session::new();
        let _ = session.apply(parse("ssid MyNetwork"));
        let _ = session.apply(parse("psk hunter2"));
        let summary = session.summary();
        assert!(summary.contains("MyNetwork"));
        assert!(!summary.contains("hunter2"));
        assert!(summary.contains('7'));
    }

    // --- Console I/O --------------------------------------------------------

    #[cfg(feature = "drivers")]
    fn console(script: &str) -> crate::sensors::mock::FakeUart {
        use crate::sensors::mock::{FakeUart, Segment};
        FakeUart::new([Segment::now(script.as_bytes().to_vec())])
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn a_typed_session_is_read_back_off_the_console() {
        use crate::sensors::mock::block_on;
        use embassy_time::Duration;

        let mut c = console("ssid MyNetwork\r\npsk hunter2\r\nsave\r\n");
        let outcome = block_on(provision(&mut c, Duration::from_millis(50)));
        assert_eq!(outcome, Outcome::Save(credentials("MyNetwork", "hunter2")));
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn a_silent_console_carries_on_booting() {
        use crate::sensors::mock::block_on;
        use embassy_time::Duration;

        // Nobody is listening on the other end: the board must not sit at the
        // prompt for ever.
        let mut c = console("");
        assert_eq!(
            block_on(provision(&mut c, Duration::from_millis(20))),
            Outcome::Continue
        );
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn a_mistake_can_be_corrected_and_the_session_still_ends_well() {
        use crate::sensors::mock::block_on;
        use embassy_time::Duration;

        let mut c = console("halp\nssid Typo\nssid MyNetwork\npsk hunter2\nsave\n");
        assert_eq!(
            block_on(provision(&mut c, Duration::from_millis(50))),
            Outcome::Save(credentials("MyNetwork", "hunter2"))
        );
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn backspace_edits_the_line() {
        use crate::sensors::mock::block_on;
        use embassy_time::Duration;

        // Terminals without line editing send the raw keystrokes.
        // "MyNetwrok", three rubouts to drop "rok", then "ork".
        let mut c = console("ssid MyNetwrok\x7f\x7f\x7fork\nsave\n");
        assert_eq!(
            block_on(provision(&mut c, Duration::from_millis(50))),
            Outcome::Save(credentials("MyNetwork", ""))
        );
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn clearing_from_the_console_ends_the_session() {
        use crate::sensors::mock::block_on;
        use embassy_time::Duration;

        let mut c = console("clear\n");
        assert_eq!(
            block_on(provision(&mut c, Duration::from_millis(50))),
            Outcome::Clear
        );
    }

    #[cfg(feature = "drivers")]
    #[test]
    fn show_reports_without_ending_the_session() {
        use crate::sensors::mock::block_on;
        use embassy_time::Duration;

        let mut c = console("ssid MyNetwork\nshow\nsave\n");
        assert_eq!(
            block_on(provision(&mut c, Duration::from_millis(50))),
            Outcome::Save(credentials("MyNetwork", ""))
        );
    }
}

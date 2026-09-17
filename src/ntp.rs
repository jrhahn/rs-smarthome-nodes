//! SNTP: what the node asks a time server, and what it makes of the answer.
//!
//! An ESP32-C3 has no battery-backed clock, so a node that has just been
//! plugged in does not know the year. Every reading it publishes is therefore
//! timestamped by whoever receives it, which is fine while the receiver is
//! listening and wrong the moment it is not: a value the broker held back
//! lands with the time of its *delivery* rather than of its measurement.
//!
//! One UDP round trip fixes that. It costs a packet each way on a link that is
//! already up -- the radio is only ever on because we are about to publish --
//! and it happens once per round rather than once per boot, so the answer is
//! always fresh and nothing has to be kept between wakes. See [`crate::clock`]
//! for what is done with it, and for why storing it was tried and dropped.
//!
//! This is SNTP, not NTP: one request, one answer, no discipline loop and no
//! filtering across several servers. It also does not correct for the round
//! trip, which a full client would do by halving `t4 - t1`. On the LAN that
//! correction is a millisecond or two, against a reading cadence measured in
//! minutes and an RTC that drifts by whole seconds per hour anyway -- so the
//! server's transmit timestamp is taken as-is, and the error it leaves is far
//! below the error it removes.

/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch (1970-01-01).
const NTP_TO_UNIX: u64 = 2_208_988_800;

/// An SNTP packet, request and response alike.
pub const PACKET_LEN: usize = 48;

/// The well-known NTP port.
pub const PORT: u16 = 123;

/// LI = 0 (no warning), VN = 4, Mode = 3 (client).
const CLIENT_HEADER: u8 = 0b00_100_011;

/// Mode 4: the answer came from a server rather than being someone else's
/// question arriving on our port.
const MODE_SERVER: u8 = 4;

/// Why an answer was not usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    /// Fewer than [`PACKET_LEN`] bytes: not an SNTP packet at all.
    TooShort,
    /// The mode field does not say "server".
    NotAServer,
    /// The server is telling us its own clock is not to be trusted.
    Unsynchronised,
    /// A well-formed packet whose transmit timestamp is zero.
    NoTime,
}

impl Rejected {
    /// A short reason, for the log line that follows a failed sync.
    pub fn as_str(self) -> &'static str {
        match self {
            Rejected::TooShort => "short packet",
            Rejected::NotAServer => "not a server reply",
            Rejected::Unsynchronised => "server is unsynchronised",
            Rejected::NoTime => "no transmit timestamp",
        }
    }
}

/// The request. Every field but the leading one is zero -- a client has nothing
/// to tell the server, and SNTP says so explicitly.
pub fn request() -> [u8; PACKET_LEN] {
    let mut packet = [0u8; PACKET_LEN];
    packet[0] = CLIENT_HEADER;
    packet
}

/// The server's transmit timestamp, as Unix milliseconds.
///
/// Everything that would make the answer a lie is refused rather than returned
/// as a plausible number: a node that keeps its old (or no) clock is visibly
/// unsynced, while a node that believes a kiss-of-death packet would stamp
/// every reading it ever publishes with 1900.
pub fn parse_response(packet: &[u8]) -> Result<u64, Rejected> {
    if packet.len() < PACKET_LEN {
        return Err(Rejected::TooShort);
    }
    if packet[0] & 0b0000_0111 != MODE_SERVER {
        return Err(Rejected::NotAServer);
    }
    // LI 3 is the server saying it is not synchronised. Stratum 0 is a
    // "kiss of death" packet, which carries a four-letter reason where the
    // stratum would be and no usable time at all; 16 and above is the other
    // spelling of unsynchronised.
    let leap = packet[0] >> 6;
    let stratum = packet[1];
    if leap == 3 || stratum == 0 || stratum >= 16 {
        return Err(Rejected::Unsynchronised);
    }

    // Bytes 40..48: the transmit timestamp, seconds then binary fraction.
    let seconds = u32::from_be_bytes([packet[40], packet[41], packet[42], packet[43]]);
    let fraction = u32::from_be_bytes([packet[44], packet[45], packet[46], packet[47]]);
    if seconds == 0 {
        return Err(Rejected::NoTime);
    }
    Ok(unix_millis(seconds, fraction))
}

/// NTP's 32-bit second count plus its fraction, as Unix milliseconds.
///
/// The count is seconds since 1900 in a `u32`, so it rolls over on
/// 2036-02-07 and starts again at zero. Era 1 is recognised by the value
/// being *below* the 1970 offset: no answer a node can legitimately receive
/// predates the Unix epoch, so a small number means the counter has wrapped
/// rather than that the year is 1910. Without this the first sync after that
/// date would hand back a timestamp 136 years in the past, and every reading
/// would sort before the whole existing history.
pub fn unix_millis(ntp_seconds: u32, ntp_fraction: u32) -> u64 {
    let seconds = if u64::from(ntp_seconds) >= NTP_TO_UNIX {
        u64::from(ntp_seconds) - NTP_TO_UNIX
    } else {
        u64::from(ntp_seconds) + (1u64 << 32) - NTP_TO_UNIX
    };
    // The fraction is a binary fraction of a second (2^-32 units), so it is
    // scaled and shifted rather than divided.
    let millis = (u64::from(ntp_fraction) * 1_000) >> 32;
    seconds * 1_000 + millis
}

// --- The round trip ---------------------------------------------------------

/// How long to wait for the answer before giving up.
///
/// The server is one hop away on the LAN. This is not tuned for latency but
/// for the case where nothing is listening on port 123 at all: the reply then
/// never comes, and without a bound the publish round would park here for ever
/// instead of going out unstamped.
#[cfg(feature = "hal")]
pub const TIMEOUT_MS: u64 = 2_000;

/// Ask `server` for the time, once.
///
/// Best-effort by construction: every failure is a `&'static str` the caller
/// logs and carries on from. A node that cannot reach its time server must
/// still publish -- an unstamped reading is worth having, a skipped round is
/// not.
#[cfg(feature = "hal")]
pub async fn query<D: embassy_net::driver::Driver>(
    stack: &embassy_net::Stack<D>,
    server: embassy_net::Ipv4Address,
) -> Result<u64, &'static str> {
    use embassy_net::udp::{PacketMetadata, UdpSocket};
    use embassy_time::{with_timeout, Duration};

    // One datagram in each direction, and a receive buffer twice the packet
    // size so a longer (or stray) answer is caught rather than silently
    // truncated into something that still parses.
    let mut rx_meta = [PacketMetadata::EMPTY; 2];
    let mut rx_buffer = [0u8; PACKET_LEN * 2];
    let mut tx_meta = [PacketMetadata::EMPTY; 2];
    let mut tx_buffer = [0u8; PACKET_LEN];
    let mut socket = UdpSocket::new(
        stack,
        &mut rx_meta,
        &mut rx_buffer,
        &mut tx_meta,
        &mut tx_buffer,
    );

    // Port 0: let the stack pick an ephemeral one. Binding a fixed local port
    // would collide with itself if this were ever called twice in a round.
    socket.bind(0).map_err(|_| "ntp bind")?;
    socket
        .send_to(&request(), (server, PORT))
        .await
        .map_err(|_| "ntp send")?;

    let mut answer = [0u8; PACKET_LEN * 2];
    let (len, from) = with_timeout(
        Duration::from_millis(TIMEOUT_MS),
        socket.recv_from(&mut answer),
    )
    .await
    .map_err(|_| "ntp timeout")?
    .map_err(|_| "ntp receive")?;

    // Anyone on the LAN can send us a datagram; only the server we asked gets
    // to set this node's clock.
    if from.addr != server.into() {
        return Err("ntp reply from the wrong host");
    }
    parse_response(&answer[..len]).map_err(Rejected::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-formed server reply carrying `seconds`/`fraction`.
    fn reply(seconds: u32, fraction: u32) -> [u8; PACKET_LEN] {
        let mut p = [0u8; PACKET_LEN];
        p[0] = 0b00_100_100; // LI 0, VN 4, Mode 4 (server)
        p[1] = 3; // stratum
        p[40..44].copy_from_slice(&seconds.to_be_bytes());
        p[44..48].copy_from_slice(&fraction.to_be_bytes());
        p
    }

    #[test]
    fn the_request_is_a_client_packet_and_nothing_else() {
        let packet = request();
        assert_eq!(packet.len(), PACKET_LEN);
        assert_eq!(packet[0] >> 6, 0, "leap indicator must be 'no warning'");
        assert_eq!((packet[0] >> 3) & 0b111, 4, "version");
        assert_eq!(packet[0] & 0b111, 3, "mode must be 'client'");
        // A client sends no timestamps of its own; everything else stays zero.
        assert!(packet[1..].iter().all(|b| *b == 0));
    }

    #[test]
    fn a_servers_transmit_timestamp_becomes_unix_time() {
        // 2026-09-16T20:00:00Z is 1_789_675_200 in Unix seconds.
        let unix = 1_789_675_200u64;
        let ntp = (unix + NTP_TO_UNIX) as u32;
        assert_eq!(parse_response(&reply(ntp, 0)), Ok(unix * 1_000));
    }

    #[test]
    fn the_binary_fraction_becomes_milliseconds() {
        let ntp = (1_789_675_200u64 + NTP_TO_UNIX) as u32;
        // Half a second is the top bit of the fraction.
        let half = 1u32 << 31;
        assert_eq!(
            parse_response(&reply(ntp, half)),
            Ok(1_789_675_200_000 + 500)
        );
        // A quarter, to show it is scaled and not just thresholded.
        assert_eq!(
            parse_response(&reply(ntp, half / 2)),
            Ok(1_789_675_200_000 + 250)
        );
    }

    #[test]
    fn the_2036_rollover_is_read_as_the_next_era_not_as_1910() {
        // The first second after the NTP counter wraps. Naively subtracting the
        // epoch offset here underflows; the answer must be 2036-02-07, not a
        // date before the Unix epoch.
        let unix_at_wrap = (1u64 << 32) - NTP_TO_UNIX; // 2036-02-07T06:28:16Z
        assert_eq!(parse_response(&reply(0, 0)), Err(Rejected::NoTime));
        assert_eq!(parse_response(&reply(1, 0)), Ok((unix_at_wrap + 1) * 1_000));
        // And the last second before it still reads as 2036.
        let before = u32::MAX;
        assert_eq!(
            parse_response(&reply(before, 0)),
            Ok((u64::from(before) - NTP_TO_UNIX) * 1_000)
        );
    }

    #[test]
    fn a_server_that_admits_it_is_lost_is_refused() {
        // Leap indicator 3: "clock not synchronised".
        let mut p = reply(3_000_000_000, 0);
        p[0] |= 0b1100_0000;
        assert_eq!(parse_response(&p), Err(Rejected::Unsynchronised));

        // Stratum 0 is a kiss-of-death packet: the bytes where a timestamp
        // would be carry a reason code instead.
        let mut p = reply(3_000_000_000, 0);
        p[1] = 0;
        assert_eq!(parse_response(&p), Err(Rejected::Unsynchronised));

        // 16 and above is the other way a server says the same thing.
        let mut p = reply(3_000_000_000, 0);
        p[1] = 16;
        assert_eq!(parse_response(&p), Err(Rejected::Unsynchronised));
    }

    #[test]
    fn our_own_question_coming_back_is_not_an_answer() {
        // Mode 3 is a client packet. On a broadcast-happy LAN this is exactly
        // what turns up on an open UDP port, and its transmit timestamp is
        // zero -- which would set the clock to 1900 if the mode were not
        // checked first.
        let mut p = reply(3_000_000_000, 0);
        p[0] = CLIENT_HEADER;
        assert_eq!(parse_response(&p), Err(Rejected::NotAServer));
    }

    #[test]
    fn a_truncated_or_empty_datagram_is_refused() {
        let full = reply(3_000_000_000, 0);
        assert_eq!(parse_response(&full[..47]), Err(Rejected::TooShort));
        assert_eq!(parse_response(&[]), Err(Rejected::TooShort));
        // Exactly long enough still works, so the bound is not off by one.
        assert!(parse_response(&full[..PACKET_LEN]).is_ok());
    }

    #[test]
    fn a_well_formed_packet_without_a_time_is_refused() {
        assert_eq!(parse_response(&reply(0, 12345)), Err(Rejected::NoTime));
    }
}

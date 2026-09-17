//! Just enough HTTP to fetch a firmware image.
//!
//! One `GET`, one response, a body streamed straight into flash. No chunked
//! transfer decoding, no redirects, no TLS — the server at the other end is
//! nginx on the home server, on the LAN, serving a static file, and everything
//! this leaves out is something that would have to be tested against a server
//! that will never send it.
//!
//! It is generic over the stream rather than written against `embassy-net`, for
//! the same reason the sensor drivers are generic over their bus: a canned
//! response can then be pushed through the whole parser on the host, where a
//! truncated header or a 404 that arrives as a body is cheap to reproduce.
//!
//! **Nothing here holds the image.** The body is handed to a sink a few
//! kilobytes at a time; 740 KB does not fit in 400 KB of SRAM, so the only
//! design that works is the one where the bytes are never all in memory at
//! once.

/// A URL, broken into the three things a connection needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Url<'a> {
    /// A dotted-quad address. Deliberately not a hostname: this firmware
    /// configures no DNS server — the broker and the time server are both baked
    /// in as literals — so a hostname here would fail at connect time with
    /// something far less obvious than the error below.
    pub ip: [u8; 4],
    pub port: u16,
    pub path: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlError {
    /// `https://`, which this firmware has no TLS stack for.
    NotHttp,
    /// A hostname rather than an address, which needs the DNS this node has not
    /// been given.
    NotAnAddress,
    BadPort,
}

impl UrlError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotHttp => "url is not http:// (there is no TLS stack here)",
            Self::NotAnAddress => "url host must be a dotted IPv4 address (no DNS is configured)",
            Self::BadPort => "url port is not a number",
        }
    }
}

pub fn parse_url(url: &str) -> Result<Url<'_>, UrlError> {
    let rest = url.strip_prefix("http://").ok_or(UrlError::NotHttp)?;
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().map_err(|_| UrlError::BadPort)?),
        None => (authority, 80u16),
    };
    Ok(Url {
        ip: parse_ipv4(host).ok_or(UrlError::NotAnAddress)?,
        port,
        path,
    })
}

fn parse_ipv4(host: &str) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut seen = 0;
    for (i, part) in host.split('.').enumerate() {
        if i >= 4 || part.is_empty() {
            return None;
        }
        octets[i] = part.parse().ok()?;
        seen = i + 1;
    }
    (seen == 4).then_some(octets)
}

/// The parts of a response head this cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Head {
    pub status: u16,
    pub content_length: Option<u32>,
    /// Offset of the first body byte within the buffer the head was parsed from.
    pub body_at: usize,
}

/// Parse a response head, or `None` if the blank line has not arrived yet —
/// which is the caller's signal to read more rather than to give up.
pub fn parse_head(buf: &[u8]) -> Option<Head> {
    let end = find(buf, b"\r\n\r\n")?;
    let head = core::str::from_utf8(&buf[..end]).ok()?;
    let mut lines = head.split("\r\n");

    // "HTTP/1.1 200 OK"
    let status_line = lines.next()?;
    let mut parts = status_line.split(' ');
    let version = parts.next()?;
    if !version.starts_with("HTTP/1.") {
        return None;
    }
    let status = parts.next()?.parse().ok()?;

    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().ok();
        }
    }

    Some(Head {
        status,
        content_length,
        body_at: end + 4,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Fetch `url` over an already-connected stream, handing the body to `sink` as
/// it arrives.
///
/// `from` resumes a partial download with a `Range` request, which is what makes
/// a lost association cost the bytes since the last chunk rather than the whole
/// image. Returns how many body bytes were passed to the sink.
///
/// `scratch` is the read buffer, and its size is the granularity the sink sees.
/// Pass a whole flash sector: the writer erases and rewrites a sector per call,
/// so feeding it in smaller pieces multiplies both the wear and the time spent
/// with the cache disabled.
#[cfg(feature = "drivers")]
pub async fn fetch<S, F>(
    stream: &mut S,
    url: &Url<'_>,
    from: u32,
    scratch: &mut [u8],
    mut sink: F,
) -> Result<u32, &'static str>
where
    S: embedded_io_async::Read + embedded_io_async::Write,
    F: FnMut(&[u8]) -> Result<(), &'static str>,
{
    use core::fmt::Write as _;

    let mut request = heapless::String::<256>::new();
    write!(
        request,
        "GET {} HTTP/1.1\r\nHost: {}.{}.{}.{}\r\nConnection: close\r\n",
        url.path, url.ip[0], url.ip[1], url.ip[2], url.ip[3]
    )
    .map_err(|_| "url is too long for a request line")?;
    if from > 0 {
        write!(request, "Range: bytes={from}-\r\n").map_err(|_| "range header")?;
    }
    request.push_str("\r\n").map_err(|_| "request")?;

    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|_| "http write failed")?;
    stream.flush().await.map_err(|_| "http flush failed")?;

    // The head arrives in the same reads as the start of the body, so it is
    // accumulated separately and whatever follows it is handed on.
    let mut head_buf = [0u8; 512];
    let mut head_len = 0usize;
    let head = loop {
        if head_len == head_buf.len() {
            return Err("response head is longer than 512 bytes");
        }
        let n = stream
            .read(&mut head_buf[head_len..])
            .await
            .map_err(|_| "http read failed")?;
        if n == 0 {
            return Err("connection closed before the response head");
        }
        head_len += n;
        if let Some(head) = parse_head(&head_buf[..head_len]) {
            break head;
        }
    };

    // 200 for a whole file, 206 for the range a resumed download asks for. A
    // server that ignores `Range` answers 200 and starts from zero, which would
    // silently write the beginning of the image over the middle of the slot —
    // so a resumed request that comes back 200 is refused, not accepted.
    let expected_status = if from > 0 { 206 } else { 200 };
    if head.status != expected_status {
        return Err(match head.status {
            404 => "server has no such image (404)",
            200 => "server ignored the Range header on a resumed download",
            416 => "server says the range is past the end of the file (416)",
            _ => "server refused the request",
        });
    }

    let mut total = 0u32;
    let leftover = &head_buf[head.body_at..head_len];
    if !leftover.is_empty() {
        sink(leftover)?;
        total += leftover.len() as u32;
    }

    loop {
        let n = stream
            .read(scratch)
            .await
            .map_err(|_| "http read failed")?;
        if n == 0 {
            break;
        }
        sink(&scratch[..n])?;
        total += n as u32;
    }

    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_the_offer_will_carry() {
        assert_eq!(
            parse_url("http://192.168.1.67/fw/kueche.bin"),
            Ok(Url {
                ip: [192, 168, 1, 67],
                port: 80,
                path: "/fw/kueche.bin"
            })
        );
        assert_eq!(
            parse_url("http://192.168.1.67:8080/x"),
            Ok(Url {
                ip: [192, 168, 1, 67],
                port: 8080,
                path: "/x"
            })
        );
        // No path at all is the server's root, not an error.
        assert_eq!(parse_url("http://10.0.0.1").map(|u| u.path), Ok("/"));
    }

    #[test]
    fn urls_this_firmware_cannot_fetch_say_so() {
        // Each of these fails at a different, much later point if it is not
        // caught here: TLS never negotiates, DNS never resolves.
        assert_eq!(
            parse_url("https://192.168.1.67/fw.bin"),
            Err(UrlError::NotHttp)
        );
        assert_eq!(
            parse_url("http://home-server/fw.bin"),
            Err(UrlError::NotAnAddress)
        );
        assert_eq!(
            parse_url("http://192.168.1/fw.bin"),
            Err(UrlError::NotAnAddress)
        );
        assert_eq!(
            parse_url("http://192.168.1.67.5/fw.bin"),
            Err(UrlError::NotAnAddress)
        );
        assert_eq!(
            parse_url("http://999.1.1.1/fw.bin"),
            Err(UrlError::NotAnAddress)
        );
        assert_eq!(
            parse_url("http://192.168.1.67:http/fw.bin"),
            Err(UrlError::BadPort)
        );
    }

    #[test]
    fn a_response_head_is_read_when_it_is_complete_and_not_before() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 42\r\nServer: nginx\r\n\r\nbody";
        // Every prefix short of the blank line must ask for more rather than
        // guessing — this is the loop condition in `fetch`.
        for len in 0..response.len() - 4 {
            assert_eq!(parse_head(&response[..len]), None, "at {len} bytes");
        }
        let head = parse_head(response).unwrap();
        assert_eq!(head.status, 200);
        assert_eq!(head.content_length, Some(42));
        assert_eq!(&response[head.body_at..], b"body");
    }

    #[test]
    fn headers_are_case_insensitive_and_a_missing_length_is_not_fatal() {
        let head = parse_head(b"HTTP/1.0 206 Partial Content\r\ncONTENT-lENGTH:  7 \r\n\r\n")
            .unwrap();
        assert_eq!(head.status, 206);
        assert_eq!(head.content_length, Some(7));

        let head = parse_head(b"HTTP/1.1 200 OK\r\n\r\n").unwrap();
        assert_eq!(head.content_length, None);
    }

    #[test]
    fn something_that_is_not_a_response_is_refused() {
        assert_eq!(parse_head(b"<html>not http</html>\r\n\r\n"), None);
        assert_eq!(parse_head(b"HTTP/1.1 twohundred OK\r\n\r\n"), None);
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    use crate::sensors::mock::block_on;

    #[derive(Debug)]
    struct Closed;

    impl embedded_io_async::Error for Closed {
        fn kind(&self) -> embedded_io_async::ErrorKind {
            embedded_io_async::ErrorKind::Other
        }
    }

    /// A stream that plays back a canned response in fixed-size pieces and then
    /// reports end-of-file, which is how a `Connection: close` download ends.
    struct Canned {
        response: Vec<u8>,
        at: usize,
        piece: usize,
        pub request: Vec<u8>,
    }

    impl Canned {
        fn new(response: &[u8], piece: usize) -> Self {
            Self {
                response: response.to_vec(),
                at: 0,
                piece,
                request: Vec::new(),
            }
        }
    }

    impl embedded_io_async::ErrorType for Canned {
        type Error = Closed;
    }

    impl embedded_io_async::Read for Canned {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            let left = self.response.len() - self.at;
            let n = left.min(self.piece).min(buf.len());
            buf[..n].copy_from_slice(&self.response[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    impl embedded_io_async::Write for Canned {
        async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
            self.request.extend_from_slice(buf);
            Ok(buf.len())
        }
    }

    fn body_of(response: &[u8], piece: usize, from: u32) -> (Vec<u8>, Canned) {
        let mut stream = Canned::new(response, piece);
        let mut got = Vec::new();
        let mut scratch = [0u8; 8];
        let url = parse_url("http://192.168.1.67/fw/x.bin").unwrap();
        let n = block_on(fetch(&mut stream, &url, from, &mut scratch, |chunk| {
            got.extend_from_slice(chunk);
            Ok(())
        }))
        .unwrap();
        assert_eq!(n as usize, got.len());
        (got, stream)
    }

    const BODY: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

    #[test]
    fn the_body_arrives_whole_however_the_reads_land() {
        // The split that matters is one that cuts the header in half, and one
        // that puts head and body in the same read.
        for piece in [1usize, 7, 40, 4096] {
            let mut response = Vec::from(
                &b"HTTP/1.1 200 OK\r\nContent-Length: 36\r\n\r\n"[..],
            );
            response.extend_from_slice(BODY);
            let (got, _) = body_of(&response, piece, 0);
            assert_eq!(got, BODY, "piece size {piece}");
        }
    }

    #[test]
    fn a_resumed_download_asks_for_a_range_and_insists_on_getting_one() {
        let mut response = Vec::from(
            &b"HTTP/1.1 206 Partial Content\r\nContent-Length: 6\r\n\r\n"[..],
        );
        response.extend_from_slice(b"uvwxyz");
        let (got, stream) = body_of(&response, 16, 30);
        assert_eq!(got, b"uvwxyz");
        let request = core::str::from_utf8(&stream.request).unwrap();
        assert!(request.contains("Range: bytes=30-"), "{request}");

        // A server that ignores the range answers 200 from the start of the
        // file. Accepting that would write the head of the image over its
        // middle, so it has to be refused.
        let mut stream = Canned::new(b"HTTP/1.1 200 OK\r\n\r\nwhole file", 64);
        let url = parse_url("http://192.168.1.67/f").unwrap();
        let mut scratch = [0u8; 16];
        let err = block_on(fetch(&mut stream, &url, 30, &mut scratch, |_| Ok(()))).unwrap_err();
        assert!(err.contains("ignored the Range"), "{err}");
    }

    #[test]
    fn an_error_page_is_an_error_and_not_an_image() {
        // The failure this exists for: nginx answering 404 with a perfectly
        // valid HTML body, which would otherwise be written into a slot.
        let mut stream = Canned::new(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot here!",
            64,
        );
        let url = parse_url("http://192.168.1.67/f").unwrap();
        let mut scratch = [0u8; 16];
        let mut sunk = 0;
        let err = block_on(fetch(&mut stream, &url, 0, &mut scratch, |c| {
            sunk += c.len();
            Ok(())
        }))
        .unwrap_err();
        assert!(err.contains("404"), "{err}");
        assert_eq!(sunk, 0, "nothing may reach the sink from a failed response");
    }

    #[test]
    fn a_sink_that_refuses_stops_the_download() {
        // The writer refusing — a bad magic byte, a full slot — must end the
        // fetch rather than be swallowed.
        let mut response =
            Vec::from(&b"HTTP/1.1 200 OK\r\nContent-Length: 36\r\n\r\n"[..]);
        response.extend_from_slice(BODY);
        let mut stream = Canned::new(&response, 4);
        let url = parse_url("http://192.168.1.67/f").unwrap();
        let mut scratch = [0u8; 8];
        let err = block_on(fetch(&mut stream, &url, 0, &mut scratch, |_| {
            Err("slot is full")
        }))
        .unwrap_err();
        assert_eq!(err, "slot is full");
    }

    #[test]
    fn a_connection_that_dies_mid_head_is_not_a_zero_length_image() {
        let mut stream = Canned::new(b"HTTP/1.1 200 OK\r\nContent-Len", 64);
        let url = parse_url("http://192.168.1.67/f").unwrap();
        let mut scratch = [0u8; 16];
        let err = block_on(fetch(&mut stream, &url, 0, &mut scratch, |_| Ok(()))).unwrap_err();
        assert!(err.contains("closed before"), "{err}");
    }
}

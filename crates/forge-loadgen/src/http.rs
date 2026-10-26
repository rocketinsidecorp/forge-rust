//! A minimal HTTP/1.1 client over `std::net`: one keep-alive connection per
//! worker, `Content-Length` and chunked bodies. It speaks to the application
//! under test only (loopback, plain HTTP).

use std::io::{self, Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// Longest response head accepted.
const MAX_HEAD: usize = 64 * 1024;
/// Most of a body kept for the caller (the rest is read and dropped).
const MAX_BODY: usize = 1 << 20;
const TIMEOUT: Duration = Duration::from_secs(30);

/// A request as it goes on the wire, built once and sent many times.
#[derive(Clone, Debug)]
pub struct Request {
    /// `GET /path` for messages.
    pub label: String,
    /// The status that counts as success.
    pub want: u16,
    wire: Vec<u8>,
}

impl Request {
    /// `method path` with `headers` and an optional form or JSON `body`.
    #[must_use]
    pub fn new(
        host: &str,
        method: &str,
        path: &str,
        headers: &[(&str, String)],
        body: &str,
        want: u16,
    ) -> Self {
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: forge-loadgen\r\nAccept: */*\r\n"
        );
        for (name, value) in headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        if !body.is_empty() || method == "POST" {
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        head.push_str("\r\n");
        let mut wire = head.into_bytes();
        wire.extend_from_slice(body.as_bytes());
        Self {
            label: format!("{method} {path}"),
            want,
            wire,
        }
    }
}

/// A response: the status, the headers the load generator uses and the body.
#[derive(Debug, Default)]
pub struct Reply {
    pub status: u16,
    /// Every header, in order, names as received; kept only when the caller
    /// asked to keep the body.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    /// The first header called `name`, case-insensitively.
    #[cfg(test)]
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The value of cookie `name` among the `Set-Cookie` headers, if one sets it.
    #[must_use]
    pub fn set_cookie(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .filter(|(candidate, _)| candidate.eq_ignore_ascii_case("set-cookie"))
            .find_map(|(_, value)| {
                let pair = value.split(';').next()?;
                let (cookie, content) = pair.split_once('=')?;
                (cookie.trim() == name && !content.is_empty()).then(|| content.trim().to_owned())
            })
    }

    /// The body as text (lossy).
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// One keep-alive connection that reconnects when the server closed it.
pub struct Connection {
    addr: SocketAddr,
    stream: Option<TcpStream>,
    buffer: Vec<u8>,
}

impl Connection {
    /// A connection to `addr`, opened on first use.
    #[must_use]
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            stream: None,
            buffer: Vec::with_capacity(32 * 1024),
        }
    }

    /// Sends `request` and reads the whole response. When `keep_body` is
    /// false the body is read and dropped.
    ///
    /// A reused connection that the server closed in the meantime fails
    /// before any response byte arrives; the request is then sent once more on
    /// a new connection, as every HTTP client does.
    ///
    /// # Errors
    ///
    /// On a connection, protocol or timeout failure.
    pub fn send(&mut self, request: &Request, keep_body: bool) -> io::Result<Reply> {
        let reused = self.stream.is_some();
        match self.exchange(request, keep_body) {
            // Only when no response byte arrived: a request the server may
            // have processed (a POST that creates a row) is never re-sent.
            Err(error) if reused && is_stale(&error) && self.buffer.is_empty() => {
                self.exchange(request, keep_body)
            }
            other => other,
        }
    }

    fn exchange(&mut self, request: &Request, keep_body: bool) -> io::Result<Reply> {
        if self.stream.is_none() {
            let stream = TcpStream::connect_timeout(&self.addr, TIMEOUT)?;
            stream.set_nodelay(true)?;
            stream.set_read_timeout(Some(TIMEOUT))?;
            stream.set_write_timeout(Some(TIMEOUT))?;
            self.stream = Some(stream);
        }
        let result = self.exchange_on_stream(request, keep_body);
        match &result {
            Ok((_, close)) if !close => {}
            _ => self.stream = None,
        }
        result.map(|(reply, _)| reply)
    }

    fn exchange_on_stream(
        &mut self,
        request: &Request,
        keep_body: bool,
    ) -> io::Result<(Reply, bool)> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "no connection"));
        };
        // Cleared before writing, so a failed write leaves it empty too and
        // `send` can tell "nothing received" from "failed mid-response".
        self.buffer.clear();
        stream.write_all(&request.wire)?;
        read_reply(stream, &mut self.buffer, keep_body)
    }
}

/// An error that means the server had already closed the idle connection.
fn is_stale(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
    )
}

fn protocol(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Reads one response from `stream`; the flag says the connection must not be
/// reused.
fn read_reply(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
    keep_body: bool,
) -> io::Result<(Reply, bool)> {
    buffer.clear();
    let mut reader = Reader {
        stream,
        buffer,
        position: 0,
    };
    let head_end = loop {
        if let Some(end) = find(&reader.buffer[reader.position..], b"\r\n\r\n") {
            break reader.position + end;
        }
        if reader.buffer.len() > MAX_HEAD {
            return Err(protocol("response head too large"));
        }
        if reader.fill()? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
    };
    // The head is parsed in place; headers are kept only for callers that
    // keep the body (the sign-in steps), so a measured request allocates
    // nothing for them.
    let mut reply = Reply::default();
    let mut length = None;
    let mut chunked = false;
    let mut close;
    {
        let head = String::from_utf8_lossy(&reader.buffer[..head_end]);
        let mut lines = head.split("\r\n");
        let status_line = lines.next().ok_or_else(|| protocol("empty response"))?;
        reply.status = status_line
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .ok_or_else(|| protocol("bad status line"))?;
        close = status_line.starts_with("HTTP/1.0");
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let (name, value) = (name.trim(), value.trim());
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| protocol("bad content-length"))?,
                );
            } else if name.eq_ignore_ascii_case("transfer-encoding") {
                chunked = value.to_ascii_lowercase().contains("chunked");
            } else if name.eq_ignore_ascii_case("connection") {
                close = value.eq_ignore_ascii_case("close");
            }
            if keep_body {
                reply.headers.push((name.to_owned(), value.to_owned()));
            }
        }
    }
    reader.position = head_end + 4;
    let status = reply.status;

    let mut body = Vec::new();
    let no_body = status == 204 || status == 304 || (100..200).contains(&status);
    if chunked {
        reader.chunked(&mut body, keep_body)?;
    } else if let Some(length) = length {
        reader.exact(length, &mut body, keep_body)?;
    } else if !no_body {
        // No framing: the body ends with the connection.
        close = true;
        while reader.fill()? > 0 {}
        let rest = &reader.buffer[reader.position..];
        if keep_body {
            body.extend_from_slice(&rest[..rest.len().min(MAX_BODY)]);
        }
    }
    reply.body = body;
    Ok((reply, close))
}

struct Reader<'a> {
    stream: &'a mut TcpStream,
    buffer: &'a mut Vec<u8>,
    /// Start of what has not been consumed.
    position: usize,
}

impl Reader<'_> {
    /// Reads more bytes into the buffer; zero at the end of the stream.
    fn fill(&mut self) -> io::Result<usize> {
        let mut chunk = [0_u8; 16 * 1024];
        let read = self.stream.read(&mut chunk)?;
        self.buffer.extend_from_slice(&chunk[..read]);
        Ok(read)
    }

    /// Makes at least `count` unconsumed bytes available.
    fn ensure(&mut self, count: usize) -> io::Result<()> {
        while self.buffer.len() - self.position < count {
            if self.fill()? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
        Ok(())
    }

    /// Consumes `count` bytes, appending them to `body` while it stays under
    /// the cap and `keep` is set.
    fn take(&mut self, count: usize, body: &mut Vec<u8>, keep: bool) -> io::Result<()> {
        let mut remaining = count;
        while remaining > 0 {
            if self.buffer.len() == self.position && self.fill()? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let available = (self.buffer.len() - self.position).min(remaining);
            if keep && body.len() < MAX_BODY {
                let room = MAX_BODY - body.len();
                let slice = &self.buffer[self.position..self.position + available.min(room)];
                body.extend_from_slice(slice);
            }
            self.position += available;
            remaining -= available;
        }
        Ok(())
    }

    fn exact(&mut self, length: usize, body: &mut Vec<u8>, keep: bool) -> io::Result<()> {
        self.take(length, body, keep)
    }

    /// A line up to `\r\n`, consumed.
    fn line(&mut self) -> io::Result<String> {
        loop {
            if let Some(end) = find(&self.buffer[self.position..], b"\r\n") {
                let line =
                    String::from_utf8_lossy(&self.buffer[self.position..self.position + end])
                        .into_owned();
                self.position += end + 2;
                return Ok(line);
            }
            if self.buffer.len() - self.position > MAX_HEAD {
                return Err(protocol("chunk line too long"));
            }
            if self.fill()? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
    }

    fn chunked(&mut self, body: &mut Vec<u8>, keep: bool) -> io::Result<()> {
        loop {
            let line = self.line()?;
            let size = line.split(';').next().unwrap_or_default().trim();
            let size = usize::from_str_radix(size, 16).map_err(|_| protocol("bad chunk size"))?;
            if size == 0 {
                // Trailers, then the empty line.
                while !self.line()?.is_empty() {}
                return Ok(());
            }
            self.take(size, body, keep)?;
            self.ensure(2)?;
            self.position += 2;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    /// A server that answers each connection's requests with `responses` in
    /// turn, then closes.
    fn serve(responses: Vec<&'static [u8]>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").map(|l| (l.local_addr(), l));
        let Ok((Ok(addr), listener)) = listener else {
            panic!("bind");
        };
        thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buffer = [0_u8; 4096];
            for response in responses {
                if stream.read(&mut buffer).unwrap_or(0) == 0 {
                    return;
                }
                let _ = stream.write_all(response);
            }
        });
        addr
    }

    fn get(path: &str) -> Request {
        Request::new("127.0.0.1", "GET", path, &[], "", 200)
    }

    #[test]
    fn reads_content_length_bodies_on_a_kept_alive_connection() {
        let addr = serve(vec![
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nSet-Cookie: a=1; Path=/\r\n\r\nhello",
            b"HTTP/1.1 303 See Other\r\nLocation: /x/7\r\nContent-Length: 0\r\n\r\n",
        ]);
        let mut connection = Connection::new(addr);
        let first = connection.send(&get("/"), true);
        let Ok(first) = first else {
            panic!("{first:?}");
        };
        assert_eq!((first.status, first.text().as_str()), (200, "hello"));
        assert_eq!(first.set_cookie("a").as_deref(), Some("1"));
        let second = connection.send(&get("/"), true);
        let Ok(second) = second else {
            panic!("{second:?}");
        };
        assert_eq!(second.status, 303);
        assert_eq!(second.header("location"), Some("/x/7"));
    }

    #[test]
    fn reads_chunked_bodies_and_drops_them_on_request() {
        const CHUNKED: &[u8] = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n4;ext=1\r\ndefg\r\n0\r\nX-Trailer: t\r\n\r\n";
        let mut connection = Connection::new(serve(vec![CHUNKED, CHUNKED]));
        let kept = connection.send(&get("/"), true);
        assert_eq!(
            kept.map(|reply| reply.text()).ok().as_deref(),
            Some("abcdefg")
        );
        let dropped = connection.send(&get("/"), false);
        assert_eq!(dropped.map(|reply| reply.body.len()).ok(), Some(0));
    }

    #[test]
    fn a_response_without_framing_ends_with_the_connection() {
        let addr = serve(vec![
            b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nrest of it",
        ]);
        let mut connection = Connection::new(addr);
        let reply = connection.send(&get("/"), true);
        assert_eq!(
            reply.map(|reply| reply.text()).ok().as_deref(),
            Some("rest of it")
        );
    }

    #[test]
    fn a_closed_idle_connection_is_replaced_once() {
        // The first connection answers once and closes; the second answers too.
        let listener = TcpListener::bind("127.0.0.1:0");
        let Ok(listener) = listener else {
            panic!("bind");
        };
        let Ok(addr) = listener.local_addr() else {
            panic!("addr");
        };
        thread::spawn(move || {
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut buffer = [0_u8; 4096];
                let _ = stream.read(&mut buffer);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
            }
        });
        let mut connection = Connection::new(addr);
        assert_eq!(
            connection.send(&get("/"), true).map(|r| r.status).ok(),
            Some(200)
        );
        // real-clock: lets the server side of a real socket close an idle keep-alive connection
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            connection.send(&get("/"), true).map(|r| r.status).ok(),
            Some(200)
        );
    }

    #[test]
    fn requests_carry_their_headers_and_body() {
        let request = Request::new(
            "h:1",
            "POST",
            "/p",
            &[("Cookie", "a=b".to_owned())],
            "x=1",
            303,
        );
        let wire = String::from_utf8_lossy(&request.wire).into_owned();
        assert!(
            wire.starts_with("POST /p HTTP/1.1\r\nHost: h:1\r\n"),
            "{wire}"
        );
        assert!(wire.contains("Cookie: a=b\r\n") && wire.contains("Content-Length: 3\r\n"));
        assert!(wire.ends_with("\r\n\r\nx=1"));
        assert_eq!(request.want, 303);
        assert_eq!(request.label, "POST /p");
    }
}

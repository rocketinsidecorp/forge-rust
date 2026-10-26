//! Dependency-free liveness probe for container `HEALTHCHECK` instructions.
//!
//! Distroless runtime images have no shell or `curl`, so the application
//! binary probes itself with a blocking, time-bounded HTTP/1.1 request.

use std::{
    io::{self, BufRead, BufReader, Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream},
    time::Duration,
};

use thiserror::Error;

/// Connect, write, and read deadline for each probe step.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

const LIVENESS_REQUEST: &[u8] =
    b"GET /health/live HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
const MAX_STATUS_LINE_BYTES: u64 = 1024;

/// Reason a probe did not observe a healthy server.
#[derive(Debug, Error)]
pub(crate) enum ProbeError {
    /// No TCP connection could be established.
    #[error("could not connect to {address}: {error}")]
    Connect {
        /// Probed address.
        address: SocketAddr,
        /// Underlying I/O failure.
        error: io::Error,
    },
    /// The connection failed while sending or receiving.
    #[error("request to {address} failed: {error}")]
    Io {
        /// Probed address.
        address: SocketAddr,
        /// Underlying I/O failure.
        error: io::Error,
    },
    /// The server answered with a non-200 status.
    #[error("unhealthy response: {0}")]
    Unhealthy(String),
    /// The server did not answer with an HTTP/1.x status line.
    #[error("malformed response status line {0:?}")]
    Malformed(String),
}

/// Address to probe for a configured bind address.
///
/// Unspecified addresses (`0.0.0.0`, `::`) are replaced by the matching
/// loopback address; any other address is probed as configured.
pub(crate) fn probe_target(bind: SocketAddr) -> SocketAddr {
    let ip = match bind.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, bind.port())
}

/// Requests `GET /health/live` and succeeds only on an HTTP/1.x `200` status.
pub(crate) fn probe(address: SocketAddr, timeout: Duration) -> Result<(), ProbeError> {
    let io_error = |error| ProbeError::Io { address, error };
    let mut stream = TcpStream::connect_timeout(&address, timeout)
        .map_err(|error| ProbeError::Connect { address, error })?;
    stream.set_read_timeout(Some(timeout)).map_err(io_error)?;
    stream.set_write_timeout(Some(timeout)).map_err(io_error)?;
    stream.write_all(LIVENESS_REQUEST).map_err(io_error)?;

    let mut status_line = String::new();
    BufReader::new(stream.take(MAX_STATUS_LINE_BYTES))
        .read_line(&mut status_line)
        .map_err(|error| match error.kind() {
            io::ErrorKind::InvalidData => ProbeError::Malformed("non-UTF-8 data".to_owned()),
            _ => io_error(error),
        })?;
    check_status_line(status_line.trim_end())
}

fn check_status_line(line: &str) -> Result<(), ProbeError> {
    let mut parts = line.splitn(3, ' ');
    match (parts.next(), parts.next()) {
        (Some("HTTP/1.1" | "HTTP/1.0"), Some("200")) => Ok(()),
        (Some("HTTP/1.1" | "HTTP/1.0"), Some(code))
            if code.len() == 3 && code.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            Err(ProbeError::Unhealthy(line.to_owned()))
        }
        _ => Err(ProbeError::Malformed(line.to_owned())),
    }
}

#[cfg(test)]
mod probe_tests;

#[cfg(test)]
mod tests {
    use std::{net::TcpListener, thread::JoinHandle};

    use super::*;

    /// Serves exactly one connection with `response` and returns the request.
    fn one_shot_server(response: &'static [u8]) -> (SocketAddr, JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener must bind");
        let address = listener.local_addr().expect("listener has an address");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("probe must connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("read timeout must be settable");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 256];
            while !request.ends_with(b"\r\n\r\n") {
                let read = stream.read(&mut buffer).expect("request must arrive");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            stream
                .write_all(response)
                .expect("response must be written");
            request
        });
        (address, handle)
    }

    #[test]
    fn healthy_server_passes_and_receives_exact_request() {
        let (address, server) =
            one_shot_server(b"HTTP/1.1 200 OK\r\ncontent-length: 17\r\n\r\n{\"status\":\"live\"}");

        probe(address, PROBE_TIMEOUT).expect("200 must be healthy");

        let request = server.join().expect("server thread must not panic");
        assert_eq!(request, LIVENESS_REQUEST);
    }

    #[test]
    fn http_1_0_success_passes() {
        let (address, server) = one_shot_server(b"HTTP/1.0 200 OK\r\n\r\n");

        probe(address, PROBE_TIMEOUT).expect("HTTP/1.0 200 must be healthy");
        server.join().expect("server thread must not panic");
    }

    #[test]
    fn unavailable_status_fails() {
        let (address, server) =
            one_shot_server(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n");

        let error = probe(address, PROBE_TIMEOUT).expect_err("503 must be unhealthy");

        assert!(matches!(error, ProbeError::Unhealthy(ref line) if line.contains("503")));
        server.join().expect("server thread must not panic");
    }

    #[test]
    fn garbage_response_fails() {
        let (address, server) = one_shot_server(b"SSH-2.0-OpenSSH_9.9\r\n");

        let error = probe(address, PROBE_TIMEOUT).expect_err("garbage must be unhealthy");

        assert!(matches!(error, ProbeError::Malformed(_)));
        server.join().expect("server thread must not panic");
    }

    #[test]
    fn empty_response_fails() {
        let (address, server) = one_shot_server(b"");

        let error = probe(address, PROBE_TIMEOUT).expect_err("empty reply must be unhealthy");

        assert!(matches!(error, ProbeError::Malformed(_)));
        server.join().expect("server thread must not panic");
    }

    #[test]
    fn refused_connection_fails() {
        // Bound and never listening: connecting is refused, and unlike a
        // probed port that was closed, nobody else can take it meanwhile.
        let refusing = tokio::net::TcpSocket::new_v4().expect("socket");
        refusing
            .bind("127.0.0.1:0".parse().expect("valid address"))
            .expect("test socket must bind");
        let address = refusing.local_addr().expect("socket has an address");

        let error = probe(address, PROBE_TIMEOUT).expect_err("closed port must be unhealthy");

        assert!(matches!(error, ProbeError::Connect { .. }));
    }

    #[test]
    fn status_line_requires_exact_200() {
        assert!(check_status_line("HTTP/1.1 200").is_ok());
        assert!(check_status_line("HTTP/1.1 2000 OK").is_err());
        assert!(check_status_line("HTTP/2 200").is_err());
        assert!(check_status_line("HTTP/1.1 204 No Content").is_err());
    }

    #[test]
    fn unspecified_bind_addresses_probe_loopback() {
        let v4: SocketAddr = "0.0.0.0:8080".parse().expect("valid address");
        let v6: SocketAddr = "[::]:8080".parse().expect("valid address");

        assert_eq!(
            probe_target(v4),
            "127.0.0.1:8080".parse().expect("valid address")
        );
        assert_eq!(
            probe_target(v6),
            "[::1]:8080".parse().expect("valid address")
        );
    }

    #[test]
    fn specific_bind_addresses_are_probed_as_configured() {
        for bind in ["127.0.0.1:3000", "10.1.2.3:8080", "[::1]:9000"] {
            let bind: SocketAddr = bind.parse().expect("valid address");
            assert_eq!(probe_target(bind), bind);
        }
    }
}

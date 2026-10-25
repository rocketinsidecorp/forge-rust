//! Probe failures that are not a refused connection or a bad status line.

use std::net::TcpListener;

use super::*;

#[test]
fn a_status_line_that_is_not_text_is_malformed() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let address = listener.local_addr().expect("address");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("probe connects");
        let mut request = [0_u8; 128];
        let _ = stream.read(&mut request).expect("request");
        stream
            .write_all(b"HTTP/1.1 200 \xff\xfe\r\n\r\n")
            .expect("response");
    });

    let error = probe(address, Duration::from_secs(5)).expect_err("not UTF-8");
    server.join().expect("server thread");

    assert!(matches!(error, ProbeError::Malformed(_)), "{error:?}");
    assert_eq!(
        error.to_string(),
        "malformed response status line \"non-UTF-8 data\""
    );
}

#[test]
fn a_server_that_never_answers_times_out_as_an_io_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let address = listener.local_addr().expect("address");
    let (release, held) = std::sync::mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("probe connects");
        // Hold the connection open, silent, until the probe gave up.
        let _ = held.recv();
        drop(stream);
    });

    let error = probe(address, Duration::from_millis(100)).expect_err("no answer");
    release.send(()).expect("server still waiting");
    server.join().expect("server thread");

    assert!(matches!(error, ProbeError::Io { .. }), "{error:?}");
    assert!(
        error
            .to_string()
            .starts_with(&format!("request to {address} failed")),
        "{error}"
    );
}

#[test]
fn a_connection_closed_without_an_answer_is_malformed() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let address = listener.local_addr().expect("address");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("probe connects");
        let mut request = [0_u8; 128];
        let _ = stream.read(&mut request).expect("request");
    });

    let error = probe(address, Duration::from_secs(5)).expect_err("empty answer");
    server.join().expect("server thread");

    assert!(matches!(error, ProbeError::Malformed(_)), "{error:?}");
}

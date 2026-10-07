use super::*;

use std::net::{TcpListener, TcpStream};

/// A connected server/client socket pair on loopback.
fn pair() -> (TcpStream, TcpStream) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let client = TcpStream::connect(addr).unwrap();
    let (server, _) = l.accept().unwrap();
    (server, client)
}

#[test]
fn sse_failure_is_a_response_failed_frame() {
    // Turn errors ride the stream: the host's retry must re-enter through
    // `provider/chat` (a fresh relay), never re-POST into this consumed
    // admission — so post-admission failures are SSE, not HTTP statuses.
    let (mut server, mut client) = pair();
    assert!(write_sse_head(&mut server));
    write_sse_failure(&mut server, "Antigravity quota exhausted");
    drop(server); // close the stream so read_to_string sees EOF
    let mut buf = String::new();
    client.read_to_string(&mut buf).unwrap();
    assert!(buf.starts_with("HTTP/1.1 200 OK"), "{buf}");
    assert!(buf.contains("Content-Type: text/event-stream"), "{buf}");
    assert!(buf.contains("\"type\":\"response.failed\""), "{buf}");
    assert!(buf.contains("Antigravity quota exhausted"), "{buf}");
    assert!(buf.ends_with("data: [DONE]\n\n"), "{buf}");
}

#[test]
fn admission_rejection_is_still_http() {
    // Pre-admission rejections (bad bearer/path, consumed admission) keep
    // their HTTP status — only the admitted turn's outcome rides SSE.
    let (mut server, mut client) = pair();
    write_resp(&mut server, 400, b"{}");
    drop(server);
    let mut buf = String::new();
    client.read_to_string(&mut buf).unwrap();
    assert!(buf.starts_with("HTTP/1.1 400 Bad Request"), "{buf}");
}

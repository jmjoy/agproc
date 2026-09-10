//! Probes: a minimal HTTP/1.1 GET and a TCP connect.
//!
//! Only the status line of the HTTP response is needed, so this deliberately
//! avoids pulling in an HTTP client (and TLS): agproc probes local dev
//! endpoints, which are plain HTTP by design.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use crate::config::ProbeTarget;

/// One probe attempt. `Ok(())` means ready; `Err(reason)` is a short,
/// agent-readable explanation of why not.
pub fn attempt(target: &ProbeTarget, timeout: Duration) -> Result<(), String> {
    match target {
        ProbeTarget::Http {
            host, port, path, ..
        } => http_get(host, *port, path, timeout),
        ProbeTarget::Tcp { host, port } => tcp_connect(host, *port, timeout),
        ProbeTarget::None => Ok(()),
    }
}

fn connect(host: &str, port: u16, timeout: Duration) -> Result<TcpStream, String> {
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|err| format!("cannot resolve {host}:{port}: {err}"))?;
    let mut last = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => return Ok(stream),
            Err(err) => last = Some(err),
        }
    }
    Err(match last {
        Some(err) => describe_io(&err, timeout),
        None => format!("cannot resolve {host}:{port}"),
    })
}

fn describe_io(err: &std::io::Error, timeout: Duration) -> String {
    match err.kind() {
        ErrorKind::ConnectionRefused => "connection refused".to_string(),
        // A SO_RCVTIMEO expiry surfaces as WouldBlock on Linux.
        ErrorKind::TimedOut | ErrorKind::WouldBlock => {
            format!("timed out after {}", format_secs(timeout))
        }
        ErrorKind::PermissionDenied => "permission denied".to_string(),
        _ => err.to_string(),
    }
}

fn format_secs(timeout: Duration) -> String {
    let ms = timeout.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", timeout.as_secs_f64())
    }
}

fn tcp_connect(host: &str, port: u16, timeout: Duration) -> Result<(), String> {
    connect(host, port, timeout).map(|_| ())
}

fn http_get(host: &str, port: u16, path: &str, timeout: Duration) -> Result<(), String> {
    let mut stream = connect(host, port, timeout)?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|err| err.to_string())?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|err| err.to_string())?;

    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUser-Agent: agproc/{}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        env!("CARGO_PKG_VERSION")
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|err| describe_io(&err, timeout))?;

    let deadline = Instant::now() + timeout;
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    loop {
        if let Some(status) = parse_status(&buf) {
            return if (200..400).contains(&status) {
                Ok(())
            } else {
                Err(format!("HTTP {status}"))
            };
        }
        if buf.len() > 8192 {
            return Err("malformed HTTP response".to_string());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!("timed out after {}", format_secs(timeout)));
        }
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|err| err.to_string())?;
        match stream.read(&mut chunk) {
            Ok(0) => return Err("connection closed before the status line".to_string()),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => return Err(describe_io(&err, timeout)),
        }
    }
}

/// Extract the status code from the first line of an HTTP response.
fn parse_status(buf: &[u8]) -> Option<u16> {
    let end = buf
        .windows(2)
        .position(|w| w == b"\r\n")
        .or_else(|| buf.iter().position(|b| *b == b'\n'))?;
    let line = std::str::from_utf8(&buf[..end]).ok()?;
    let mut parts = line.split_whitespace();
    let version = parts.next()?;
    if !version.starts_with("HTTP/") {
        return None;
    }
    parts.next()?.parse::<u16>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn serve_once(response: &'static [u8]) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(4) {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(response);
                let _ = stream.flush();
            }
        });
        port
    }

    fn http_target(port: u16) -> ProbeTarget {
        ProbeTarget::Http {
            scheme: "http".to_string(),
            host: "127.0.0.1".to_string(),
            port,
            path: "/healthz".to_string(),
        }
    }

    #[test]
    fn http_2xx_and_3xx_pass() {
        let port = serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        assert_eq!(attempt(&http_target(port), Duration::from_secs(2)), Ok(()));

        let port = serve_once(b"HTTP/1.1 302 Found\r\nLocation: /x\r\n\r\n");
        assert_eq!(attempt(&http_target(port), Duration::from_secs(2)), Ok(()));
    }

    #[test]
    fn http_5xx_fails_with_status() {
        let port = serve_once(b"HTTP/1.1 503 Service Unavailable\r\n\r\n");
        assert_eq!(
            attempt(&http_target(port), Duration::from_secs(2)),
            Err("HTTP 503".to_string())
        );
    }

    #[test]
    fn closed_port_reports_connection_refused() {
        // Bind then drop, so the port is almost certainly free.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert_eq!(
            attempt(&http_target(port), Duration::from_millis(500)),
            Err("connection refused".to_string())
        );
    }

    #[test]
    fn silent_listener_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            // Hold connections open without answering.
            let _kept: Vec<_> = listener.incoming().take(4).flatten().collect();
            std::thread::sleep(Duration::from_secs(5));
        });
        let start = Instant::now();
        let result = attempt(&http_target(port), Duration::from_millis(300));
        assert!(result.is_err(), "{result:?}");
        assert!(start.elapsed() < Duration::from_secs(3));
        assert!(result.unwrap_err().contains("timed out"), "unexpected reason");
    }

    #[test]
    fn tcp_connect_succeeds_against_a_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let _ = listener.accept();
            std::thread::sleep(Duration::from_millis(200));
        });
        assert_eq!(
            attempt(
                &ProbeTarget::Tcp {
                    host: "127.0.0.1".to_string(),
                    port
                },
                Duration::from_secs(2)
            ),
            Ok(())
        );
    }

    #[test]
    fn parse_status_handles_partial_and_newline_only() {
        assert_eq!(parse_status(b"HTTP/1.1 200"), None);
        assert_eq!(parse_status(b"HTTP/1.1 200 OK\r\n"), Some(200));
        assert_eq!(parse_status(b"HTTP/1.0 404 Not Found\n"), Some(404));
        assert_eq!(parse_status(b"garbage"), None);
    }
}

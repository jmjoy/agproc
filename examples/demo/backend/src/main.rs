//! Minimal HTTP service for the agproc demo: no dependencies, one endpoint.

use std::io::{Read, Write};
use std::net::TcpListener;

fn main() {
    let port = std::env::args()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(38080);

    // Printed before the probe passed: agproc shows this and a tty keeps it line
    // buffered even though `cargo run` style tooling would block-buffer a pipe.
    println!("demo-backend listening on http://127.0.0.1:{port}");
    eprintln!("demo-backend: ready for requests");

    // Comes from `env-file = ".env"` in examples/demo/agproc.toml: proof that the
    // file reaches the run-cmd environment, not just the shell you started it from.
    let greeting =
        std::env::var("DEMO_GREETING").unwrap_or_else(|_| "(DEMO_GREETING unset)".to_string());
    println!("demo-backend greeting: {greeting}");

    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind 127.0.0.1");
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let request = String::from_utf8_lossy(&buf);
        let path = request
            .split_whitespace()
            .nth(1)
            .unwrap_or("/")
            .to_string();

        let (status, body) = if path.starts_with("/healthz") {
            ("200 OK", "ok".to_string())
        } else {
            ("200 OK", format!("demo-backend is up (you asked for {path})\n"))
        };
        println!("served {path}");
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }
}

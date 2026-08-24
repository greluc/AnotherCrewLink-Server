//! Verifies the wire format against the reference Socket.IO implementation.
//!
//! The server is started as a child process on a port of its own, and `wire.mjs` drives
//! it with the same `socket.io-client` the shipping clients use. Checking our encoder
//! against another copy of our own decoder would prove nothing; this is the only test in
//! the crate that can fail because of a protocol mistake rather than a logic one.
//!
//! Skipped, loudly, if Node or `socket.io-client` is not present — a developer without
//! them should see why the coverage is missing rather than a green run that checked less
//! than it appears to.

use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const PORT: u16 = 19_737;

struct ServerProcess(Child);

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn repo_root() -> PathBuf {
    // The Node dependencies live in the repository this crate sits inside.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("server-rs has a parent directory")
        .to_path_buf()
}

fn node_available() -> bool {
    let client = repo_root().join("node_modules/socket.io-client/package.json");
    if !client.exists() {
        eprintln!("skipping: socket.io-client is not installed; run npm ci in the repository root");
        return false;
    }
    match Command::new("node").arg("--version").output() {
        Ok(output) if output.status.success() => true,
        _ => {
            eprintln!("skipping: node is not on PATH");
            false
        }
    }
}

fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[test]
fn the_wire_format_matches_the_reference_client() {
    if !node_available() {
        return;
    }

    let binary = env!("CARGO_BIN_EXE_aucl-server");
    let child = Command::new(binary)
        .env("PORT", PORT.to_string())
        .env("BIND", "127.0.0.1")
        .env("NAME", "wire-test")
        // A path that does not exist, so the test runs against the documented defaults
        // rather than against whatever the developer has configured locally.
        .env("PEER_CONFIG", "config/does-not-exist.toml")
        .env("RUST_LOG", "warn")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the server binary starts");
    let mut server = ServerProcess(child);

    assert!(
        wait_for_port(PORT, Duration::from_secs(20)),
        "the server did not begin listening on {PORT}"
    );

    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wire.mjs");
    let output = Command::new("node")
        .arg(&script)
        .arg(PORT.to_string())
        .current_dir(repo_root())
        .output()
        .expect("node runs the wire script");

    let report = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The server's own complaints are worth seeing when a check fails: a refused signal
    // logs a warning, and that is often the fastest explanation.
    if !output.status.success()
        && let Some(err) = server.0.stderr.take()
    {
        for line in BufReader::new(err).lines().map_while(Result::ok).take(60) {
            eprintln!("server: {line}");
        }
    }

    assert!(
        output.status.success(),
        "the reference client reported failures\n--- report ---\n{report}\n--- node stderr ---\n{stderr}"
    );

    // A report with no checks in it would pass the exit-code assertion above while
    // proving nothing at all.
    let parsed: serde_json::Value =
        serde_json::from_str(report.trim()).expect("the wire script prints a JSON report");
    let checks = parsed["checks"]
        .as_object()
        .expect("the report carries named checks");
    assert!(
        checks.len() >= 15,
        "expected the full set of checks, got {}: {report}",
        checks.len()
    );
    assert!(
        checks.values().all(|passed| passed == true),
        "every check passes: {report}"
    );
}

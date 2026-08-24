//! The container health probe.
//!
//! The final image has no shell and no curl, so `HEALTHCHECK CMD curl …` cannot work
//! there. This is the replacement: a single static binary that speaks just enough
//! HTTP/1.0 to ask the server's own `/health` endpoint whether it is alive, and exits
//! 0 or 1 the way Docker expects.
//!
//! It is deliberately not part of the `acl-server` crate. `rustc` compiles this one
//! file in the builder stage with no dependencies at all, so nothing in this file can
//! reach the server binary, its lockfile or its test suite.
//!
//! Build:
//!     rustc --edition 2024 -O -C panic=abort -C strip=symbols healthcheck.rs

use std::env;
use std::io::{Error, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::process::ExitCode;
use std::time::Duration;

/// Everything — connect, write, read — gets this long. Docker's own `--timeout` is the
/// outer bound; this one exists so a half-open socket fails as a probe rather than
/// hanging until Docker kills it, which reads as a different fault in the logs.
const TIMEOUT: Duration = Duration::from_secs(2);

/// `/health` is a small JSON object. Anything larger than this is not the endpoint we
/// asked for, and reading it to the end would be an unbounded read against whatever is
/// actually listening on that port.
const MAX_RESPONSE: u64 = 64 * 1024;

fn main() -> ExitCode {
    let addr = target();
    match probe(addr) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // Docker keeps the last few lines of a failing probe's output in
            // `docker inspect`, which is the only diagnostic channel an image with no
            // shell has.
            eprintln!("acl-healthcheck: {addr}: {err}");
            ExitCode::FAILURE
        }
    }
}

/// The same two environment variables the server reads, so a probe cannot drift away
/// from the socket the server actually bound.
fn target() -> SocketAddr {
    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(9736);

    // A wildcard bind is not an address to connect to: the probe runs inside the
    // container's own network namespace, so it goes to the loopback interface. A server
    // listening on `::` accepts a v4-mapped loopback connection on Linux, and one
    // listening on `0.0.0.0` accepts a v4 one, so this covers both wildcards.
    let host = env::var("BIND")
        .ok()
        .and_then(|value| value.parse::<IpAddr>().ok())
        .filter(|ip| !ip.is_unspecified())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));

    SocketAddr::new(host, port)
}

fn probe(addr: SocketAddr) -> Result<(), Error> {
    let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    stream.set_nodelay(true)?;

    // HTTP/1.0 with an explicit close: the server then ends the body at EOF and this
    // needs no chunked decoder and no content-length parser to know it has the whole
    // response.
    stream.write_all(
        b"GET /health HTTP/1.0\r\n\
          Host: localhost\r\n\
          User-Agent: acl-healthcheck\r\n\
          Connection: close\r\n\
          \r\n",
    )?;
    stream.flush()?;

    let mut response = Vec::new();
    stream.take(MAX_RESPONSE).read_to_end(&mut response)?;

    let status_line = response
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    let status_line = String::from_utf8_lossy(status_line);
    let status_line = status_line.trim_end();

    let mut fields = status_line.split(' ');
    let version = fields.next().unwrap_or_default();
    let code = fields.next().unwrap_or_default();

    if version.starts_with("HTTP/1.") && code == "200" {
        Ok(())
    } else if status_line.is_empty() {
        Err(Error::other("the server accepted the connection and said nothing"))
    } else {
        Err(Error::other(format!("unhealthy: {status_line}")))
    }
}

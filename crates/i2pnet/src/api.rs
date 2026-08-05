//! Local control-API transport.
//!
//! `cloved` serves its `/v1/` API here and `clove` connects here, so — like
//! every other socket in clove — the construction lives in `i2pnet` (Layer 1,
//! SCOPE §5). Two transports:
//!
//! - **unix socket** (default): local by nature, created `0600`.
//! - **loopback TCP** (opt-in): the address is parsed and *rejected unless it
//!   is a loopback IP*, so this is the loopback-validating helper the crate
//!   root promises. Token auth (in `cloved`) applies regardless.
//!
//! An [`ApiStream`] is a plain blocking `Read + Write`; the API is one request
//! and one response per connection, so no split is needed.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

/// A bound control-API listener: unix socket or loopback TCP.
#[derive(Debug)]
pub enum ApiListener {
    /// A unix-domain socket listener.
    Unix(UnixListener),
    /// A loopback TCP listener.
    Tcp(TcpListener),
}

impl ApiListener {
    /// Bind a unix-socket listener at `path`, replacing any stale socket file
    /// from a previous run and restricting it to the owner (`0600`).
    ///
    /// # Errors
    ///
    /// The stale socket cannot be removed, the bind fails, or the mode cannot
    /// be set.
    pub fn bind_unix(path: &Path) -> io::Result<ApiListener> {
        // A leftover socket file makes bind fail with EADDRINUSE; clear it.
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(ApiListener::Unix(listener))
    }

    /// Bind a TCP listener at `addr`, refusing any non-loopback address.
    ///
    /// # Errors
    ///
    /// `addr` is not a valid `host:port`, is not a loopback address, or the
    /// bind fails.
    pub fn bind_loopback_tcp(addr: &str) -> io::Result<ApiListener> {
        let parsed = parse_loopback(addr)?;
        Ok(ApiListener::Tcp(TcpListener::bind(parsed)?))
    }

    /// Accept one connection.
    ///
    /// # Errors
    ///
    /// The underlying accept fails.
    pub fn accept(&self) -> io::Result<ApiStream> {
        match self {
            ApiListener::Unix(l) => Ok(ApiStream::Unix(l.accept()?.0)),
            ApiListener::Tcp(l) => Ok(ApiStream::Tcp(l.accept()?.0)),
        }
    }
}

/// Connect to a unix-socket control API at `path`.
///
/// # Errors
///
/// The daemon is not listening there, or the connect fails.
pub fn connect_unix(path: &Path) -> io::Result<ApiStream> {
    Ok(ApiStream::Unix(UnixStream::connect(path)?))
}

/// Connect to a loopback-TCP control API at `addr` (validates loopback).
///
/// # Errors
///
/// `addr` is not a valid loopback `host:port`, or the connect fails.
pub fn connect_loopback_tcp(addr: &str) -> io::Result<ApiStream> {
    let parsed = parse_loopback(addr)?;
    Ok(ApiStream::Tcp(TcpStream::connect(parsed)?))
}

/// Parse `host:port` and require a loopback IP.
fn parse_loopback(addr: &str) -> io::Result<SocketAddr> {
    let parsed: SocketAddr = addr.parse().map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "API address must be host:port")
    })?;
    if !parsed.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "API TCP address must be a loopback address",
        ));
    }
    Ok(parsed)
}

/// An accepted or dialed control-API connection.
#[derive(Debug)]
pub enum ApiStream {
    /// A unix-socket connection.
    Unix(UnixStream),
    /// A loopback-TCP connection.
    Tcp(TcpStream),
}

impl Read for ApiStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            ApiStream::Unix(s) => s.read(buf),
            ApiStream::Tcp(s) => s.read(buf),
        }
    }
}

impl Write for ApiStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            ApiStream::Unix(s) => s.write(buf),
            ApiStream::Tcp(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            ApiStream::Unix(s) => s.flush(),
            ApiStream::Tcp(s) => s.flush(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn loopback_tcp_binds_and_rejects_public() {
        // Loopback binds.
        assert!(ApiListener::bind_loopback_tcp("127.0.0.1:0").is_ok());
        // Non-loopback and garbage are refused before any bind.
        assert_eq!(
            ApiListener::bind_loopback_tcp("0.0.0.0:0")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            ApiListener::bind_loopback_tcp("8.8.8.8:80")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            ApiListener::bind_loopback_tcp("not-an-addr")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn unix_socket_round_trips() {
        let dir = std::env::temp_dir().join(format!("clove-api-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("api.sock");

        let listener = ApiListener::bind_unix(&path).unwrap();
        let server = thread::spawn(move || {
            let mut conn = listener.accept().unwrap();
            let mut buf = [0u8; 4];
            conn.read_exact(&mut buf).unwrap();
            conn.write_all(&buf).unwrap();
        });

        let mut client = connect_unix(&path).unwrap();
        client.write_all(b"ping").unwrap();
        let mut back = [0u8; 4];
        client.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"ping");

        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

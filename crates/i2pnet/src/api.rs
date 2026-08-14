//! Local control-API transport.
//!
//! `cloved` serves its `/v1/` API here and `clove` connects here, so — like
//! every other socket in clove — the construction lives in `i2pnet` (Layer 1,
//! SCOPE §5). One transport: a **unix socket**, local by nature, created
//! `0600`. Token auth (in `cloved`) applies on top of that.
//!
//! An [`ApiStream`] is a plain blocking `Read + Write`; the API is one request
//! and one response per connection, so no split is needed.

use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

/// A bound control-API listener.
#[derive(Debug)]
pub struct ApiListener(UnixListener);

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
        Ok(ApiListener(listener))
    }

    /// Accept one connection.
    ///
    /// # Errors
    ///
    /// The underlying accept fails.
    pub fn accept(&self) -> io::Result<ApiStream> {
        Ok(ApiStream(self.0.accept()?.0))
    }
}

/// Connect to the control API at `path`.
///
/// # Errors
///
/// The daemon is not listening there, or the connect fails.
pub fn connect_unix(path: &Path) -> io::Result<ApiStream> {
    Ok(ApiStream(UnixStream::connect(path)?))
}

/// An accepted or dialed control-API connection.
#[derive(Debug)]
pub struct ApiStream(UnixStream);

impl ApiStream {
    /// Wrap an already-connected socket, for tests that make their own pair.
    #[must_use]
    pub fn from_unix(stream: UnixStream) -> ApiStream {
        ApiStream(stream)
    }
}

impl Read for ApiStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for ApiStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

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

    #[test]
    fn the_socket_is_owner_only() {
        let dir = std::env::temp_dir().join(format!("clove-api-mode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("api.sock");
        let _listener = ApiListener::bind_unix(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the control socket is not owner-only");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

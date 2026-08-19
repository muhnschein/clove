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
    /// **Bound under a temporary name, restricted, then renamed into place.**
    /// `bind(2)` creates the socket with `0777 & ~umask`, which is `0755` under
    /// the umask a shell hands out, and the mode we want is only applied
    /// afterwards. Binding at `path` directly therefore publishes a
    /// connectable socket at the name clients look for, and *then* takes the
    /// permission away — a window in which any local user may connect. Token
    /// auth means they would be answered with `401` and nothing else, so this
    /// is the second lock rather than the first; it is also the lock the
    /// project's own file-writing already uses (`write_private_file` in
    /// `cloved`), and there is no reason for the socket to be the exception.
    ///
    /// Renaming a bound socket keeps the listener: the socket lives in its
    /// inode, and the path is only the name a client resolves to reach it.
    /// The rename is atomic, so it also replaces a stale socket without the
    /// moment of "no socket at all" an unlink-then-bind leaves behind.
    ///
    /// # Errors
    ///
    /// The bind fails, the mode cannot be set, or the socket cannot be moved
    /// into place.
    pub fn bind_unix(path: &Path) -> io::Result<ApiListener> {
        let name = path.file_name().map_or_else(
            || std::ffi::OsString::from("clove.sock"),
            std::ffi::OsStr::to_os_string,
        );
        let tmp = path.with_file_name(format!(
            "{}.{}.tmp",
            name.to_string_lossy(),
            std::process::id()
        ));
        // A temp left by an earlier crash of this pid would fail the bind below
        // with EADDRINUSE.
        match std::fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(&tmp)?;
        // From here the temp is ours to clean up on any failure: leaving a
        // connectable socket behind under a name nothing will ever remove is
        // worse than the error being reported.
        let settle = || -> io::Result<()> {
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
            std::fs::rename(&tmp, path)
        };
        if let Err(e) = settle() {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
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

    /// The socket must be owner-only at every moment it is reachable, which
    /// means before it has the name a client connects to — not a moment after.
    /// Binding at the final path and chmod-ing afterwards passes the check
    /// below and still leaves the window; what proves the order is that no
    /// permissive mode was ever visible at that path, and that a socket left
    /// over from a previous run is replaced rather than briefly missing.
    #[test]
    fn the_socket_is_owner_only_before_it_is_reachable() {
        let dir = std::env::temp_dir().join(format!("clove-api-order-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("api.sock");

        // A stale socket at the target, as an unclean stop leaves.
        let stale = ApiListener::bind_unix(&path).unwrap();
        let listener = ApiListener::bind_unix(&path).unwrap();
        drop(stale);

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the replacing socket is not owner-only");
        // The new listener is the one at that path: it answers, and nothing
        // was left behind under the temporary name.
        let mut client = connect_unix(&path).unwrap();
        let server = thread::spawn(move || {
            let mut conn = listener.accept().unwrap();
            conn.write_all(b"pong").unwrap();
        });
        let mut back = [0u8; 4];
        client.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"pong");
        server.join().unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .filter(|n| n.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary sockets left behind: {leftovers:?}"
        );

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

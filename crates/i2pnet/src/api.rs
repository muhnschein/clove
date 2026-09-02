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
    /// Bind a unix-socket listener at `path`, replacing a *stale* socket file
    /// from a previous run and restricting it to the owner (`0600`).
    ///
    /// Only a stale socket is replaced. A socket somebody still answers on is
    /// refused, and so is anything at `path` that is not a socket at all:
    /// unlinking whatever was there was how a second daemon on the same
    /// configuration quietly took the socket away from the first, which kept
    /// running with nobody able to reach it.
    ///
    /// # Errors
    ///
    /// Something other than a socket is at `path`, another process is
    /// listening there, the stale socket cannot be removed, the bind fails, or
    /// the mode cannot be set.
    pub fn bind_unix(path: &Path) -> io::Result<ApiListener> {
        clear_stale_socket(path)?;
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

/// Remove the socket file at `path` if, and only if, it is a socket nobody is
/// listening on.
///
/// The probe is a `connect`: a live listener accepts it, a leftover file from
/// a daemon that is gone refuses it with `ECONNREFUSED`. Anything else in the
/// way — a regular file, a directory, a symlink (`symlink_metadata`, so the
/// link itself is judged rather than its target) — is not ours to delete.
fn clear_stale_socket(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::FileTypeExt;

    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !meta.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "something that is not a socket is in the way; not removing it",
        ));
    }
    match UnixStream::connect(path) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "another process is already listening here (is cloved running?)",
        )),
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => std::fs::remove_file(path),
        Err(e) => Err(e),
    }
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

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("clove-api-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A socket somebody is still serving on is not "stale", and a second
    /// bind must fail rather than unlink it from under the first.
    #[test]
    fn a_live_socket_is_not_taken_away_from_its_listener() {
        let dir = scratch("live");
        let path = dir.join("api.sock");
        let first = ApiListener::bind_unix(&path).unwrap();

        let err = ApiListener::bind_unix(&path).expect_err("a second bind on a live socket");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse, "{err}");

        // The first listener still owns the path: a client reaches *it*. The
        // probe's own connect sits in the backlog too, as a connection that
        // hung up at once, so the first accept may be that rather than ours.
        let server = thread::spawn(move || {
            loop {
                let mut conn = first.accept().unwrap();
                let mut buf = [0u8; 5];
                if conn.read_exact(&mut buf).is_ok() {
                    return buf;
                }
            }
        });
        let mut client = connect_unix(&path).unwrap();
        client.write_all(b"still").unwrap();
        assert_eq!(&server.join().unwrap(), b"still");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only a socket is ever removed. A regular file at the socket path is a
    /// misconfiguration, not a leftover, and is left exactly as found.
    #[test]
    fn a_file_where_the_socket_belongs_is_refused_not_deleted() {
        let dir = scratch("notasocket");
        let path = dir.join("api.sock");
        std::fs::write(&path, b"precious").unwrap();

        let err = ApiListener::bind_unix(&path).expect_err("bound over a regular file");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), b"precious");

        // A symlink to nowhere is judged as a symlink, not followed to "absent".
        let link = dir.join("link.sock");
        std::os::unix::fs::symlink(dir.join("nowhere"), &link).unwrap();
        let err = ApiListener::bind_unix(&link).expect_err("bound over a symlink");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        assert!(
            std::fs::symlink_metadata(&link).is_ok(),
            "the symlink was removed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The case the unlink exists for: a daemon that died left its socket
    /// file behind, and the next start must be able to bind there.
    #[test]
    fn a_stale_socket_is_replaced() {
        let dir = scratch("stale");
        let path = dir.join("api.sock");
        drop(ApiListener::bind_unix(&path).unwrap());
        assert!(
            path.exists(),
            "dropping a listener does not unlink its path"
        );

        let listener = ApiListener::bind_unix(&path).expect("rebind over a stale socket");
        drop(connect_unix(&path).expect("the new listener answers"));
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

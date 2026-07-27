//! In-memory implementation of the `i2pnet` traits (Phase B,
//! `docs/PLAN.md`): a process-local "network" where endpoints connect to
//! each other over piped in-memory streams. This is the substrate for all
//! engine tests and chaos tests — no router required.
//!
//! Fault injection mirrors what the real network does to us:
//!
//! - [`FaultHandle::kill_session`] — the router died or dropped the
//!   session: pending and future accepts fail, this endpoint's streams
//!   collapse, inbound dials are refused.
//! - [`FaultHandle::set_black_hole`] — lease set unreachable: dials to
//!   this endpoint consume their full timeout and fail.
//! - [`MockStream::set_read_stalled`] — tunnel stall / slow-loris peer:
//!   reads block despite buffered data until unstalled or killed.
//! - Bounded per-direction buffers (see [`MockNet::with_capacity`]):
//!   writes exert real backpressure, as they do over a saturated tunnel.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread;
use std::time::{Duration, Instant};

use crate::{DestHash, I2pDialer, I2pListener, I2pNamingLookup, I2pStream};

/// Default per-direction stream buffer (bytes). Small enough that tests
/// exercise backpressure without moving megabytes.
pub const DEFAULT_CAPACITY: usize = 256 * 1024;

/// Pending inbound connections an endpoint can queue before dials to it
/// are refused, like a listen backlog.
const ACCEPT_BACKLOG: usize = 16;

/// Mutex helper: a poisoned lock means a panicking test thread, and the
/// state itself (plain flags and byte buffers) is never left mid-update.
fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn wait<'a, T>(cond: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    cond.wait(guard).unwrap_or_else(PoisonError::into_inner)
}

/// A process-local I2P network. Cheap to clone; clones share the network.
#[derive(Clone, Default)]
pub struct MockNet {
    inner: Arc<NetInner>,
}

#[derive(Default)]
struct NetInner {
    endpoints: Mutex<HashMap<DestHash, Registration>>,
    names: Mutex<HashMap<String, DestHash>>,
    next_dest: Mutex<u64>,
    capacity: Mutex<usize>,
}

struct Registration {
    sender: SyncSender<(MockStream, DestHash)>,
    shared: Arc<EndpointShared>,
}

#[derive(Default)]
struct EndpointShared {
    flags: Mutex<Flags>,
    /// Pipes of every stream this endpoint is party to, closed on session
    /// kill. Weak: a dropped stream cleans itself up.
    pipes: Mutex<Vec<Weak<Pipe>>>,
}

#[derive(Default)]
struct Flags {
    dead: bool,
    black_hole: bool,
}

impl MockNet {
    /// A fresh, empty network with [`DEFAULT_CAPACITY`] stream buffers.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// A fresh network whose stream buffers hold `capacity` bytes per
    /// direction — small values make backpressure tests immediate.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let net = MockNet {
            inner: Arc::default(),
        };
        *locked(&net.inner.capacity) = capacity.max(1);
        net
    }

    /// Join the network as a new destination.
    #[must_use]
    pub fn endpoint(&self) -> Endpoint {
        let dest = {
            let mut next = locked(&self.inner.next_dest);
            *next += 1;
            let mut hash = [0u8; 32];
            hash[..8].copy_from_slice(&next.to_le_bytes());
            DestHash(hash)
        };
        let (sender, incoming) = sync_channel(ACCEPT_BACKLOG);
        let shared = Arc::new(EndpointShared::default());
        locked(&self.inner.endpoints).insert(
            dest,
            Registration {
                sender,
                shared: Arc::clone(&shared),
            },
        );
        Endpoint {
            net: Arc::clone(&self.inner),
            dest,
            incoming,
            shared,
        }
    }

    /// Publish an I2P name for [`I2pNamingLookup`] resolution.
    pub fn register_name(&self, name: &str, dest: DestHash) {
        locked(&self.inner.names).insert(name.to_owned(), dest);
    }
}

/// One session on the [`MockNet`]: implements [`I2pDialer`],
/// [`I2pListener`], and [`I2pNamingLookup`].
///
/// Holds the accept queue, so it is `Send` but not `Sync` — the same shape
/// the engine must live with on the real SAM implementation. Fault
/// injection from other threads goes through [`Endpoint::fault_handle`].
pub struct Endpoint {
    net: Arc<NetInner>,
    dest: DestHash,
    incoming: Receiver<(MockStream, DestHash)>,
    shared: Arc<EndpointShared>,
}

/// Clonable, thread-safe handle for injecting faults into an [`Endpoint`].
#[derive(Clone)]
pub struct FaultHandle {
    net: Arc<NetInner>,
    dest: DestHash,
    shared: Arc<EndpointShared>,
}

impl Endpoint {
    /// This endpoint's destination hash.
    #[must_use]
    pub fn dest(&self) -> DestHash {
        self.dest
    }

    /// Block for the next inbound stream.
    ///
    /// The mock's own convenience shape — one outcome, no "this connection was
    /// unusable" case, because nothing in an in-memory network can produce one.
    /// [`I2pListener::accept`] wraps it. Inherent, so it takes precedence at a
    /// call site: tests read straightforwardly, the engine still goes through
    /// the trait.
    ///
    /// # Errors
    /// The session was killed or the endpoint is gone.
    pub fn accept(&self) -> io::Result<(MockStream, DestHash)> {
        self.incoming
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::NotConnected, "mock: session lost"))
    }

    /// A handle for injecting faults from other threads.
    #[must_use]
    pub fn fault_handle(&self) -> FaultHandle {
        FaultHandle {
            net: Arc::clone(&self.net),
            dest: self.dest,
            shared: Arc::clone(&self.shared),
        }
    }
}

impl FaultHandle {
    /// The router dropped this session: unblocks pending `accept` with an
    /// error, refuses future inbound dials, errors outbound dials, and
    /// collapses every stream the endpoint is party to.
    pub fn kill_session(&self) {
        locked(&self.shared.flags).dead = true;
        // Removing the registration drops the accept queue's sender,
        // waking a blocked accept with "session lost".
        locked(&self.net.endpoints).remove(&self.dest);
        for pipe in locked(&self.shared.pipes).drain(..) {
            if let Some(pipe) = pipe.upgrade() {
                pipe.close();
            }
        }
    }

    /// Make dials to this endpoint consume their full timeout and fail,
    /// as an unreachable lease set does. Accepted/existing streams are
    /// unaffected.
    pub fn set_black_hole(&self, on: bool) {
        locked(&self.shared.flags).black_hole = on;
    }
}

/// A clonable, thread-safe outbound-dial handle for an [`Endpoint`] — the
/// mock's analogue of sharing the real `SamSession` between the swarm's dial
/// path and other threads while the endpoint itself (the accept queue) stays
/// with its acceptor.
#[derive(Clone)]
pub struct MockDialer {
    net: Arc<NetInner>,
    dest: DestHash,
    shared: Arc<EndpointShared>,
}

impl Endpoint {
    /// A dial handle sharing this endpoint's identity and session fate.
    #[must_use]
    pub fn dialer(&self) -> MockDialer {
        MockDialer {
            net: Arc::clone(&self.net),
            dest: self.dest,
            shared: Arc::clone(&self.shared),
        }
    }
}

/// Dial `peer` on behalf of the endpoint identified by `dest`/`shared`.
fn dial_from(
    net: &Arc<NetInner>,
    dest: DestHash,
    shared: &Arc<EndpointShared>,
    peer: DestHash,
    timeout: Duration,
) -> io::Result<MockStream> {
    if locked(&shared.flags).dead {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "mock: session is down",
        ));
    }
    let target = locked(&net.endpoints)
        .get(&peer)
        .map(|r| (r.sender.clone(), Arc::clone(&r.shared)));
    let Some((sender, target_shared)) = target else {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "mock: no such destination",
        ));
    };
    if locked(&target_shared.flags).black_hole {
        thread::sleep(timeout);
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "mock: destination unreachable (black-holed)",
        ));
    }
    let capacity = *locked(&net.capacity);
    let (ours, theirs) = MockStream::pair(capacity);
    for pipe in [&ours.read, &ours.write] {
        locked(&shared.pipes).push(Arc::downgrade(pipe));
        locked(&target_shared.pipes).push(Arc::downgrade(pipe));
    }
    if sender.try_send((theirs, dest)).is_err() {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "mock: destination not accepting (backlog full or session gone)",
        ));
    }
    Ok(ours)
}

impl I2pDialer for Endpoint {
    type Stream = MockStream;

    fn dial(&self, peer: DestHash, timeout: Duration) -> io::Result<MockStream> {
        dial_from(&self.net, self.dest, &self.shared, peer, timeout)
    }
}

impl I2pDialer for MockDialer {
    type Stream = MockStream;

    fn dial(&self, peer: DestHash, timeout: Duration) -> io::Result<MockStream> {
        dial_from(&self.net, self.dest, &self.shared, peer, timeout)
    }
}

impl I2pNamingLookup for MockDialer {
    fn lookup(&self, name: &str) -> io::Result<DestHash> {
        if locked(&self.shared.flags).dead {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "mock: session is down",
            ));
        }
        locked(&self.net.names)
            .get(name)
            .copied()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "mock: name not found"))
    }
}

impl I2pListener for Endpoint {
    type Stream = MockStream;

    fn local_dest(&self) -> DestHash {
        self.dest
    }

    fn accept(&self) -> io::Result<Option<(MockStream, DestHash)>> {
        Endpoint::accept(self).map(Some)
    }
}

impl I2pNamingLookup for Endpoint {
    fn lookup(&self, name: &str) -> io::Result<DestHash> {
        if locked(&self.shared.flags).dead {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "mock: session is down",
            ));
        }
        locked(&self.net.names)
            .get(name)
            .copied()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "mock: name not found"))
    }
}

/// One in-memory stream endpoint. Cloned handles (via
/// [`MockStream::try_clone`] or [`I2pStream::split`]) share the stream; the
/// last drop closes it.
pub struct MockStream {
    read: Arc<Pipe>,
    write: Arc<Pipe>,
    closer: Arc<Closer>,
    /// How long this endpoint's reads and writes may block, in milliseconds;
    /// zero means block indefinitely.
    ///
    /// Per *endpoint*, not per direction: a socket's receive timeout is a local
    /// option, and one that leaked across the connection would give the peer a
    /// write timeout it never asked for. Shared between handles onto the same
    /// endpoint, as the option is shared by duplicated descriptors.
    timeout_ms: Arc<std::sync::atomic::AtomicU64>,
}

impl std::fmt::Debug for MockStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MockStream")
    }
}

impl MockStream {
    fn pair(capacity: usize) -> (MockStream, MockStream) {
        let a_to_b = Pipe::new(capacity);
        let b_to_a = Pipe::new(capacity);
        let a = MockStream {
            read: Arc::clone(&b_to_a),
            write: Arc::clone(&a_to_b),
            closer: Arc::new(Closer(Arc::clone(&a_to_b), Arc::clone(&b_to_a))),
            timeout_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        let b = MockStream {
            read: Arc::clone(&a_to_b),
            write: Arc::clone(&b_to_a),
            closer: Arc::new(Closer(a_to_b, b_to_a)),
            timeout_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        (a, b)
    }

    /// Fault injection: while stalled, reads on this handle's inbound
    /// direction block even when data is buffered — a stalled tunnel or a
    /// slow-loris peer. Unstalling (or session kill) releases them.
    pub fn set_read_stalled(&self, on: bool) {
        self.read.set_stalled(on);
    }

    /// Bound how long reads and writes on this stream may block, as a real
    /// socket's `SO_RCVTIMEO`/`SO_SNDTIMEO` would; `None` restores blocking.
    /// A timed-out read or write returns [`io::ErrorKind::WouldBlock`].
    ///
    /// Worth setting in any test that waits for a message it expects: without
    /// it, a message the engine *fails* to send is a test that hangs rather
    /// than one that fails, and a hang looks the same as slow progress. Applies
    /// to every handle on this side of the stream, again like a socket.
    pub fn set_timeouts(&self, timeout: Option<Duration>) {
        let ms = timeout.map_or(0, |t| {
            u64::try_from(t.as_millis()).unwrap_or(u64::MAX).max(1)
        });
        self.timeout_ms
            .store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    /// This endpoint's current blocking bound.
    fn timeout(&self) -> Option<Duration> {
        match self.timeout_ms.load(std::sync::atomic::Ordering::Relaxed) {
            0 => None,
            ms => Some(Duration::from_millis(ms)),
        }
    }

    /// Another handle to the same stream. Infallible for the mock; used by
    /// its own tests and to build the [`I2pStream::split`] halves.
    #[must_use]
    pub fn try_clone(&self) -> MockStream {
        MockStream {
            read: Arc::clone(&self.read),
            write: Arc::clone(&self.write),
            closer: Arc::clone(&self.closer),
            timeout_ms: Arc::clone(&self.timeout_ms),
        }
    }
}

impl Read for MockStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read.read(buf, self.timeout())
    }
}

impl Write for MockStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write.write(buf, self.timeout())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl crate::I2pClose for MockStream {
    /// Both directions, like a socket shutdown: the `Closer` only fires when
    /// every handle on this side drops, and the reader thread is holding one.
    fn close(&self) {
        self.read.close();
        self.write.close();
    }
}

impl I2pStream for MockStream {
    type Reader = MockStream;
    type Writer = MockStream;

    fn split(self) -> io::Result<(MockStream, MockStream)> {
        let reader = self.try_clone();
        Ok((reader, self))
    }

    fn set_timeouts(&self, timeout: Option<Duration>) -> io::Result<()> {
        MockStream::set_timeouts(self, timeout);
        Ok(())
    }
}

/// Closes both directions when the last handle of one side drops.
struct Closer(Arc<Pipe>, Arc<Pipe>);

impl Drop for Closer {
    fn drop(&mut self) {
        self.0.close();
        self.1.close();
    }
}

/// The error a bounded read or write gives up with, matching what a socket
/// with `SO_RCVTIMEO` set does.
fn timed_out(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::WouldBlock, format!("mock: {what} timed out"))
}

/// One direction of a stream: a bounded byte queue with blocking
/// semantics on both ends.
struct Pipe {
    state: Mutex<PipeState>,
    readable: Condvar,
    writable: Condvar,
}

struct PipeState {
    buf: VecDeque<u8>,
    capacity: usize,
    closed: bool,
    stalled: bool,
}

impl Pipe {
    fn new(capacity: usize) -> Arc<Pipe> {
        Arc::new(Pipe {
            state: Mutex::new(PipeState {
                buf: VecDeque::new(),
                capacity,
                closed: false,
                stalled: false,
            }),
            readable: Condvar::new(),
            writable: Condvar::new(),
        })
    }

    fn close(&self) {
        locked(&self.state).closed = true;
        self.readable.notify_all();
        self.writable.notify_all();
    }

    fn set_stalled(&self, on: bool) {
        locked(&self.state).stalled = on;
        self.readable.notify_all();
    }

    /// Blocking read: buffered data first (even after close), zero at
    /// EOF, and nothing at all while stalled. Bounded by the direction's
    /// timeout if one is set, which surfaces as `WouldBlock`.
    fn read(&self, out: &mut [u8], timeout: Option<Duration>) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut state = locked(&self.state);
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            if !state.stalled {
                if !state.buf.is_empty() {
                    break;
                }
                if state.closed {
                    return Ok(0);
                }
            } else if state.closed {
                return Ok(0);
            }
            state = match deadline {
                None => wait(&self.readable, state),
                Some(at) => {
                    let left = at.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(timed_out("read"));
                    }
                    let (guard, _) = self
                        .readable
                        .wait_timeout(state, left)
                        .unwrap_or_else(PoisonError::into_inner);
                    guard
                }
            };
        }
        let n = out.len().min(state.buf.len());
        for slot in out.iter_mut().take(n) {
            // The queue holds >= n bytes; guarded by the length check.
            *slot = state.buf.pop_front().unwrap_or_default();
        }
        self.writable.notify_all();
        Ok(n)
    }

    /// Blocking write: waits for room (backpressure), fails on a closed
    /// pipe as a broken connection.
    fn write(&self, data: &[u8], timeout: Option<Duration>) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let mut state = locked(&self.state);
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            if state.closed {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "mock: stream is closed",
                ));
            }
            if state.buf.len() < state.capacity {
                break;
            }
            state = match deadline {
                None => wait(&self.writable, state),
                Some(at) => {
                    let left = at.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(timed_out("write"));
                    }
                    let (guard, _) = self
                        .writable
                        .wait_timeout(state, left)
                        .unwrap_or_else(PoisonError::into_inner);
                    guard
                }
            };
        }
        let room = state.capacity - state.buf.len();
        let n = room.min(data.len());
        state.buf.extend(&data[..n]);
        self.readable.notify_all();
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    const TICK: Duration = Duration::from_millis(50);
    const LONG: Duration = Duration::from_secs(5);

    fn read_exact_string(stream: &mut MockStream, len: usize) -> String {
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn endpoints_exchange_bytes_both_directions() {
        let net = MockNet::new();
        let alice = net.endpoint();
        let bob = net.endpoint();
        let bob_dest = bob.dest();

        let acceptor = thread::spawn(move || {
            let (mut stream, from) = bob.accept().unwrap();
            let hello = read_exact_string(&mut stream, 5);
            stream.write_all(b"world").unwrap();
            (hello, from)
        });

        let mut stream = alice.dial(bob_dest, LONG).unwrap();
        stream.write_all(b"hello").unwrap();
        let reply = read_exact_string(&mut stream, 5);

        let (hello, from) = acceptor.join().unwrap();
        assert_eq!(hello, "hello");
        assert_eq!(reply, "world");
        assert_eq!(from, alice.dest());
    }

    #[test]
    fn drop_gives_eof_then_broken_pipe() {
        let net = MockNet::new();
        let alice = net.endpoint();
        let bob = net.endpoint();

        let mut ours = alice.dial(bob.dest(), LONG).unwrap();
        let (mut theirs, _) = bob.accept().unwrap();

        theirs.write_all(b"bye").unwrap();
        drop(theirs);

        // Buffered data still arrives, then clean EOF.
        assert_eq!(read_exact_string(&mut ours, 3), "bye");
        let mut buf = [0u8; 8];
        assert_eq!(ours.read(&mut buf).unwrap(), 0);
        assert_eq!(
            ours.write(b"x").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn dial_failures() {
        let net = MockNet::new();
        let alice = net.endpoint();

        let err = alice.dial(DestHash([0xEE; 32]), LONG).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);

        let bob = net.endpoint();
        bob.fault_handle().set_black_hole(true);
        let err = alice.dial(bob.dest(), TICK).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        bob.fault_handle().set_black_hole(false);
        assert!(alice.dial(bob.dest(), LONG).is_ok());
    }

    #[test]
    fn naming_lookup() {
        let net = MockNet::new();
        let alice = net.endpoint();
        let tracker = net.endpoint();
        net.register_name("tracker.example.i2p", tracker.dest());

        assert_eq!(alice.lookup("tracker.example.i2p").unwrap(), tracker.dest());
        assert_eq!(
            alice.lookup("nope.i2p").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn kill_session_unblocks_accept_and_collapses_streams() {
        let net = MockNet::new();
        let alice = net.endpoint();
        let bob = net.endpoint();
        let bob_faults = bob.fault_handle();

        let mut stream = alice.dial(bob.dest(), LONG).unwrap();

        let (tx, rx) = mpsc::sync_channel(1);
        let acceptor = thread::spawn(move || {
            let (pair, _) = bob.accept().unwrap();
            drop(pair); // keep the first, then block on the next accept
            tx.send(()).unwrap();
            bob.accept().map(|_| ())
        });
        rx.recv_timeout(LONG).unwrap();

        bob_faults.kill_session();

        // Blocked accept wakes with "session lost"...
        let err = acceptor.join().unwrap().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotConnected);
        // ...dials to the dead endpoint are refused...
        assert_eq!(
            alice.dial(bob_faults.dest, LONG).unwrap_err().kind(),
            io::ErrorKind::ConnectionRefused
        );
        // ...and the established stream collapsed from alice's side too.
        let mut buf = [0u8; 4];
        assert_eq!(stream.read(&mut buf).unwrap(), 0);
        assert_eq!(
            stream.write(b"x").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn dead_endpoint_cannot_dial_or_lookup() {
        let net = MockNet::new();
        let alice = net.endpoint();
        let bob = net.endpoint();
        net.register_name("t.i2p", bob.dest());
        alice.fault_handle().kill_session();

        assert_eq!(
            alice.dial(bob.dest(), LONG).unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        assert_eq!(
            alice.lookup("t.i2p").unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
    }

    #[test]
    fn stalled_reads_block_until_released() {
        let net = MockNet::new();
        let alice = net.endpoint();
        let bob = net.endpoint();

        let ours = alice.dial(bob.dest(), LONG).unwrap();
        let (mut theirs, _) = bob.accept().unwrap();
        theirs.write_all(b"data").unwrap();

        ours.set_read_stalled(true);
        let mut reader = ours.try_clone();
        let (tx, rx) = mpsc::sync_channel(1);
        let handle = thread::spawn(move || {
            let got = read_exact_string(&mut reader, 4);
            tx.send(()).unwrap();
            got
        });

        // Data is buffered, but the stalled read must not see it.
        assert_eq!(rx.recv_timeout(TICK), Err(mpsc::RecvTimeoutError::Timeout));
        ours.set_read_stalled(false);
        rx.recv_timeout(LONG).unwrap();
        assert_eq!(handle.join().unwrap(), "data");
    }

    #[test]
    fn cloned_handles_share_one_stream() {
        let net = MockNet::new();
        let alice = net.endpoint();
        let bob = net.endpoint();

        let mut writer = alice.dial(bob.dest(), LONG).unwrap();
        let mut reader = writer.try_clone();
        let (mut theirs, _) = bob.accept().unwrap();

        writer.write_all(b"ab").unwrap();
        assert_eq!(read_exact_string(&mut theirs, 2), "ab");

        theirs.write_all(b"cd").unwrap();
        assert_eq!(read_exact_string(&mut reader, 2), "cd");

        // One handle dropped: the stream survives via the other.
        drop(writer);
        theirs.write_all(b"ef").unwrap();
        assert_eq!(read_exact_string(&mut reader, 2), "ef");
    }

    #[test]
    fn small_buffers_exert_backpressure() {
        let net = MockNet::with_capacity(4);
        let alice = net.endpoint();
        let bob = net.endpoint();

        let mut ours = alice.dial(bob.dest(), LONG).unwrap();
        let (mut theirs, _) = bob.accept().unwrap();

        let writer = thread::spawn(move || {
            ours.write_all(&[7u8; 64]).unwrap();
            64usize
        });

        let mut received = 0usize;
        let mut buf = [0u8; 16];
        while received < 64 {
            received += theirs.read(&mut buf).unwrap();
        }
        assert_eq!(writer.join().unwrap(), 64);
        assert_eq!(received, 64);
    }

    #[test]
    fn a_bounded_read_gives_up_instead_of_blocking() {
        let net = MockNet::new();
        let alice = net.endpoint();
        let bob = net.endpoint();

        let mut ours = alice.dial(bob.dest(), LONG).unwrap();
        let (mut theirs, _) = bob.accept().unwrap();

        // Nothing sent: an unbounded read would block here for ever, which in a
        // test is a hang rather than a failure.
        ours.set_timeouts(Some(TICK));
        let mut buf = [0u8; 4];
        let err = ours.read(&mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);

        // Data that does arrive is still read, and clearing the timeout puts
        // the blocking behaviour back.
        theirs.write_all(b"ping").unwrap();
        assert_eq!(read_exact_string(&mut ours, 4), "ping");
        ours.set_timeouts(None);
        theirs.write_all(b"pong").unwrap();
        assert_eq!(read_exact_string(&mut ours, 4), "pong");

        // The timeout is this endpoint's alone. A receive timeout that leaked
        // into the peer's writes would hand it a failure it never asked for —
        // and would quietly make "the engine dropped a silent peer" pass for
        // the wrong reason in any test that bounded its own reads.
        let net = MockNet::new();
        let alice = net.endpoint();
        let bob = net.endpoint();
        let ours = alice.dial(bob.dest(), LONG).unwrap();
        let (mut theirs, _) = bob.accept().unwrap();
        ours.set_timeouts(Some(TICK));
        let mut idle = theirs.try_clone();
        let waited = thread::spawn(move || {
            let mut buf = [0u8; 1];
            idle.read(&mut buf).map(|n| n == 0)
        });
        thread::sleep(TICK * 4);
        assert!(
            !waited.is_finished(),
            "the peer's read inherited our timeout"
        );
        theirs.write_all(b"x").unwrap();
        drop(ours);
        let _ = waited.join();

        // A write to a full pipe is bounded the same way.
        let net = MockNet::with_capacity(4);
        let alice = net.endpoint();
        let bob = net.endpoint();
        let mut ours = alice.dial(bob.dest(), LONG).unwrap();
        let (_theirs, _) = bob.accept().unwrap();
        ours.set_timeouts(Some(TICK));
        ours.write_all(&[0u8; 4]).unwrap();
        let err = ours.write(&[1u8; 4]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn accept_backlog_refuses_excess_dials() {
        let net = MockNet::new();
        let alice = net.endpoint();
        let bob = net.endpoint();

        let mut held = Vec::new();
        for _ in 0..ACCEPT_BACKLOG {
            held.push(alice.dial(bob.dest(), LONG).unwrap());
        }
        assert_eq!(
            alice.dial(bob.dest(), LONG).unwrap_err().kind(),
            io::ErrorKind::ConnectionRefused
        );
    }
}

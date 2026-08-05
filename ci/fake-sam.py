#!/usr/bin/env python3
"""A fake SAMv3 bridge — enough of one to bring cloved's whole network path up.

Every other test in this repository stops at the `i2pnet` boundary. The mock
network exercises the engine, the hostile-bridge unit tests exercise every way a
bridge can *misbehave*, and `ci/smoke.sh` points the daemon at a dead port on
purpose. Nothing drives `SamSession`, `SamListener` and the announce path against
a bridge that actually answers, because doing that used to need a router — and a
test that needs a router is a test that does not run.

This is that missing piece. It speaks the small subset of SAMv3 the daemon
actually uses, in the shapes a real router uses them:

    HELLO VERSION ...      -> HELLO REPLY RESULT=OK VERSION=3.3
    SESSION CREATE ...     -> SESSION STATUS RESULT=OK DESTINATION=<key blob>
    STREAM FORWARD ID=..   -> STREAM STATUS RESULT=OK, then dials the daemon's
                              forward port with a SILENT=false destination
                              header, so the inbound accept path runs
    STREAM CONNECT DEST=.. -> STREAM STATUS RESULT=OK, then plays a tracker
                              (a canned HTTP announce naming one peer) or a
                              BitTorrent peer, depending on who was dialled
    NAMING LOOKUP NAME=..  -> NAMING REPLY RESULT=OK VALUE=<destination>

Two things it is deliberately careful about, because they are the two the daemon
has been wrong about before:

  - The `DESTINATION=` it returns is a **private key blob** — a public
    destination with private key material behind it — because that is what a real
    `SESSION STATUS` carries, and clove once published the whole thing to a
    tracker (`docs/PROTOCOL.i2p-bt` §5.1c). A bridge that returned a bare
    destination would let that bug back in unnoticed.
  - It answers `HELLO` on *every* connection, one per operation, because that is
    how the daemon speaks SAM: a stream is its own connection with its own
    handshake, which is what stops one poisoned exchange killing every later dial
    (§2.12).

Driven by `ci/router.sh`; not useful on its own. Python because this is a test
fixture rather than shipped code, and a hundred lines of socket plumbing is not
worth a Rust crate and a build.

Usage: fake-sam.py <port> [<info-hash-hex>]
"""

import base64
import hashlib
import socket
import socketserver
import struct
import sys
import threading
import time

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 7656
INFO_HASH = bytes.fromhex(sys.argv[2]) if len(sys.argv) > 2 else b"\x5a" * 20

# How long a played peer holds its connection open. Long enough for the daemon to
# handshake and register it, short enough not to outlive the caller.
PEER_LINGER = 8.0
# The router does not forward a connection the instant it is asked to; a short
# delay is more like the real thing and gives the accept loop time to be running.
FORWARD_DELAY = 1.5


def i2p_b64(raw: bytes) -> str:
    """base64 in I2P's alphabet: standard, with '+' -> '-' and '/' -> '~'."""
    return base64.b64encode(raw).decode().replace("+", "-").replace("/", "~")


def key_blob(fill: int) -> bytes:
    """A session private key blob: 384 bytes of key material, a KEY certificate,
    then private crypto and signing keys.

    391 bytes of destination and 288 of private material, which is the shape
    i2pd returned when this was captured. `destination_of` is the public prefix —
    the only part that may ever leave the process.
    """
    return bytes([fill]) * 384 + b"\x05\x00\x04" + b"\x00\x07\x00\x00" + b"\xaa" * 288


def destination_of(blob: bytes) -> bytes:
    return blob[:391]


def b32_of(dest: bytes) -> str:
    """The `<b32>` label of a destination: base32(SHA-256(destination))."""
    return base64.b32encode(hashlib.sha256(dest).digest()).decode().lower().rstrip("=")


OUR_BLOB = key_blob(0x42)  # the daemon's own session identity
TRACKER_DEST = destination_of(key_blob(0x11))  # what a NAMING LOOKUP resolves to
PEER_DEST = destination_of(key_blob(0x33))  # the peer the tracker hands back

TRACKER_B32 = b32_of(TRACKER_DEST)
PEER_B32 = b32_of(PEER_DEST)

log_lock = threading.Lock()


def log(*a):
    with log_lock:
        print("fake-sam:", *a, file=sys.stderr, flush=True)


def readline(f) -> str:
    line = f.readline()
    return line.decode("utf-8", "replace").strip() if line else ""


def announce_response() -> bytes:
    """A bencoded announce reply carrying one peer as a 32-byte destination hash —
    the I2P compact form, with no port."""
    peers = hashlib.sha256(PEER_DEST).digest()
    body = b"d8:intervali1800e5:peers" + str(len(peers)).encode() + b":" + peers + b"e"
    return (
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: "
        + str(len(body)).encode()
        + b"\r\nConnection: close\r\n\r\n"
        + body
    )


def play_peer(sock: socket.socket):
    """A minimal BitTorrent peer: handshake, claim nothing, then hold the
    connection. Enough for the daemon to register it and count it."""
    try:
        sock.sendall(
            b"\x13BitTorrent protocol" + b"\x00" * 8 + INFO_HASH + b"-XX0000-fakefakefake"
        )
        sock.settimeout(10)
        sock.recv(68)
        sock.sendall(struct.pack(">IB", 1, 15))  # have-none
        time.sleep(PEER_LINGER)
    except OSError:
        pass
    finally:
        sock.close()


def dial_forward(port: int):
    """Connect to the daemon's forward port, as the router does for an inbound
    peer, prefixing the SILENT=false destination header."""
    time.sleep(FORWARD_DELAY)
    try:
        sock = socket.create_connection(("127.0.0.1", port), timeout=5)
    except OSError as e:
        log("could not reach forward port", port, e)
        return
    log("dialled the forward port", port)
    try:
        sock.sendall(i2p_b64(key_blob(0x33)).encode() + b" FROM_PORT=0 TO_PORT=0\n")
        play_peer(sock)
    except OSError:
        sock.close()


def hold_open(sock: socket.socket):
    """Keep a connection open until the peer closes it.

    Not idle politeness: SAMv3 ties a session to its control connection and
    forwarding to the socket `STREAM FORWARD` was issued on, so closing either
    would tear down what the daemon just set up.
    """
    try:
        sock.settimeout(None)
        while sock.recv(4096):
            pass
    except OSError:
        pass


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        sock = self.request
        stream = sock.makefile("rb")

        # Every connection starts with its own handshake — see the module docs.
        if not readline(stream).startswith("HELLO"):
            log("connection did not open with HELLO")
            return
        sock.sendall(b"HELLO REPLY RESULT=OK VERSION=3.3\n")

        command = readline(stream)
        log("command:", command[:90])

        if command.startswith("SESSION CREATE"):
            sock.sendall(
                b"SESSION STATUS RESULT=OK DESTINATION="
                + i2p_b64(OUR_BLOB).encode()
                + b"\n"
            )
            hold_open(sock)
        elif command.startswith("STREAM FORWARD"):
            port = next(
                (int(f[5:]) for f in command.split() if f.startswith("PORT=")), None
            )
            sock.sendall(b"STREAM STATUS RESULT=OK\n")
            if port:
                threading.Thread(target=dial_forward, args=(port,), daemon=True).start()
            hold_open(sock)
        elif command.startswith("STREAM CONNECT"):
            dest = next(
                (f[12:] for f in command.split() if f.startswith("DESTINATION=")), ""
            )
            sock.sendall(b"STREAM STATUS RESULT=OK\n")
            if dest.startswith(TRACKER_B32):
                log("serving a tracker announce")
                try:
                    sock.settimeout(20)
                    sock.recv(8192)  # the announce request
                    sock.sendall(announce_response())
                except OSError:
                    pass
                sock.close()
            else:
                log("serving a peer")
                play_peer(sock)
        elif command.startswith("NAMING LOOKUP"):
            name = command.split("NAME=", 1)[1].strip() if "NAME=" in command else ""
            sock.sendall(
                b"NAMING REPLY RESULT=OK NAME="
                + name.encode()
                + b" VALUE="
                + i2p_b64(TRACKER_DEST).encode()
                + b"\n"
            )
            sock.close()
        else:
            log("unhandled command:", command[:80])


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


if __name__ == "__main__":
    log(f"listening on 127.0.0.1:{PORT}")
    log("tracker b32:", TRACKER_B32)
    log("peer b32:", PEER_B32)
    Server(("127.0.0.1", PORT), Handler).serve_forever()

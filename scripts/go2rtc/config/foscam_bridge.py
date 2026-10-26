#!/usr/bin/env python3
"""A talk-back bridge between go2rtc's exec source and a Foscam camera.

A Foscam camera that has no ONVIF back channel still takes speech over a
protocol of its own: a plain TCP connection to the camera's port 88, an
HTTP-shaped ``SERVERPUSH`` request, a login block with the credentials in clear,
and then 976-byte frames every 60 ms carrying 480 samples of 16-bit PCM. This
process is that protocol, driven by go2rtc: when a stream carries
``#backchannel=1``, go2rtc writes the viewer's microphone audio to this
process's stdin, and this process frames it and sends it to the camera.

Usage
-----
Add the bridge as a second source beside the RTSP stream in go2rtc.yaml:

    streams:
      cam_living_room:
        - rtsp://admin:PASS@192.168.1.16:88/videoMain#backchannel=0
        - exec:python3 /config/foscam_bridge.py "rtsp://admin:PASS@192.168.1.16:88/videoMain"#backchannel=1#audio=alaw/8000

**`#backchannel=0` on the RTSP line is not optional.** A camera that advertises
an ONVIF back channel - the MGS5030 is the one in this fleet - makes go2rtc
carry the viewer's audio over that RTSP back channel instead of this bridge.
On that camera the channel negotiates and takes packets but nothing comes out
of the speaker, so a call stays silent and this bridge is never asked. Turning
the RTSP source's back channel off leaves the bridge as the only one go2rtc can
use, which is the protocol that is known to be heard. A camera that advertises
no back channel (the other eight) does not need the parameter, but it costs
nothing there either.

The URL is read only for the camera's host, port and credentials, so it can be
the same URL the video source uses: a Foscam serves RTSP and this protocol on
the same port, and its talk port is the RTSP port.

``#audio=alaw/8000`` makes go2rtc write A-law 8 kHz mono, one byte per sample,
which is what this bridge reads by default. If the line says
``#audio=s16le/8000`` instead, pass ``--pcm`` and the bridge reads 16-bit
little-endian samples directly.

Credentials may be left out of the URL and given as ``FOSCAM_USER`` /
``FOSCAM_PASS`` in the environment, which keeps a password out of go2rtc's
configuration and away from the ``#`` that separates exec parameters.

The official go2rtc image is built on ``python:3.13-alpine``, so no extra
runtime has to be installed; only the standard library is used. The script is
the same on amd64 and arm64, so a multi-platform deployment needs no
cross-compilation.

Notes
-----
- The camera answers the login and then says nothing. Audio frames are not
  acknowledged, so a write that succeeds is the only sign the connection lives;
  a write that fails is the camera hanging up.
- The camera plays what it is given whether or not a speaker is connected, so a
  silent bridge is not evidence of a quiet camera. Use the viewer's own test
  tone to check the hardware.
- The login block's fields are 64 bytes each, not 32. With 32 the password
  lands sixteen bytes early and every reply carries a one where a successful
  login carries a zero, while the camera silently drops the frames.
"""

import os
import signal
import socket
import struct
import sys
import threading
import time
from urllib.parse import unquote, urlparse

# The camera's own cadence: 480 samples of 8 kHz audio every 60 ms, 16-bit PCM.
RATE = 8000
FRAME_INTERVAL = 0.060
FRAME_SAMPLES = 480
PCM_BYTES = FRAME_SAMPLES * 2
FRAME_BYTES = 16 + PCM_BYTES
DEFAULT_PORT = 88

# A-law's silence code. A short frame is padded with it rather than refused:
# silence is what the camera would have played anyway, and a frame of the wrong
# length would desync it.
ALAW_SILENCE = 0xD5

# How much audio may sit between stdin and the socket before the oldest is
# dropped. Half a second: enough to ride out a scheduling hiccup, short enough
# that the camera's speaker is not half a second behind the viewer's voice.
MAX_BUFFER = RATE // 2

# How long the camera is given to answer the login. Measured at 30-60 ms on the
# fleet; the wait is what turns "a web server on the same port" into a failure
# instead of a socket that swallows audio forever.
LOGIN_TIMEOUT = 2.0


def log(message):
    # stderr, because go2rtc collects it into its own log and stdout is not
    # ours to write here.
    print(f"foscam-bridge: {message}", file=sys.stderr, flush=True)


def alaw_to_pcm(byte):
    """One A-law byte to one signed 16-bit sample (ITU G.711)."""
    byte ^= 0x55
    value = (byte & 0x0F) << 4
    segment = (byte & 0x70) >> 4
    if segment == 0:
        value += 8
    else:
        value = (value + 0x108) << (segment - 1)
    return value if (byte & 0x80) else -value


def alaw_to_pcm_bytes(chunk):
    """A run of A-law bytes to little-endian 16-bit PCM, two bytes a sample."""
    return struct.pack("<%dh" % len(chunk), *(alaw_to_pcm(b) for b in chunk))


def padded(text):
    """A credential as the login block carries it: 64 bytes, NUL padded."""
    raw = text.encode("utf-8")[:64]
    return raw + b"\x00" * (64 - len(raw))


def login_block(user, password):
    """The 172-byte login block: magic, credentials in clear, token, padding."""
    block = bytearray(b"\x04\x00\x00\x00" + b"FOSC" + b"\xa0\x00\x00\x00")
    block += padded(user)
    block += padded(password)
    block += b"\x00" * 8  # the client's token; all zeros are accepted
    block += b"\x00" * (172 - len(block))
    return bytes(block)


def request(host, port):
    """The HTTP-shaped request that opens the connection."""
    return (
        f"SERVERPUSH / HTTP/1.1\r\n"
        f"Host: {host}:{port}\r\n"
        f"Accept:*/*\r\n"
        f"Connection: Close\r\n"
        f"\r\n"
    ).encode("ascii")


def build_frame(pcm):
    """One 976-byte frame: sixteen bytes of header and 960 bytes of audio."""
    return (
        b"\x06\x00\x00\x00"
        + b"FOSC"
        + struct.pack("<I", FRAME_BYTES - 12)  # 964: the frame less its first 12
        + struct.pack("<I", PCM_BYTES)  # 960: the audio
        + pcm
    )


def parse_source(url):
    """Host, port and credentials, out of the same URL the video uses."""
    parsed = urlparse(url)
    if not parsed.hostname:
        raise SystemExit("foscam-bridge: the argument is not a URL with a host")
    host = parsed.hostname
    port = parsed.port or DEFAULT_PORT
    user = unquote(parsed.username) if parsed.username else os.environ.get("FOSCAM_USER", "")
    password = unquote(parsed.password) if parsed.password else os.environ.get("FOSCAM_PASS", "")
    if not user:
        raise SystemExit("foscam-bridge: no user name in the URL and FOSCAM_USER is unset")
    return host, port, user, password


def connect(host, port, user, password):
    """Opens the talk connection and logs in.

    The camera does not check the password - a wrong one is answered like the
    right one - so what this checks is that something speaking this protocol
    answered at all.
    """
    sock = socket.create_connection((host, port), timeout=5.0)
    # Audio leaves in 60 ms bursts and the picture is on another socket
    # entirely; Nagle would only add latency to speech.
    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    sock.sendall(request(host, port))
    sock.sendall(login_block(user, password))

    sock.settimeout(LOGIN_TIMEOUT)
    reply = b""
    while len(reply) < 16:
        try:
            part = sock.recv(64)
        except socket.timeout:
            break
        if not part:
            break
        reply += part

    if len(reply) < 8:
        sock.close()
        raise RuntimeError("the camera did not answer the login")
    if reply[4:8] != b"FOSC":
        sock.close()
        raise RuntimeError("the camera answered, but not with this protocol")
    # The fourth word is the answer: zero accepted, one refused. A refusal is
    # worth saying out loud, but the frames are still sent - the fleet does not
    # agree on what a refusal means, and a quiet call beats a dropped one.
    if len(reply) >= 16 and reply[12:16] != b"\x00\x00\x00\x00":
        log(
            "warning: the camera's login reply is not zero (%s); sending anyway"
            % reply[12:16].hex()
        )
    sock.settimeout(1.0)
    return sock


class Buffer:
    """What stdin has delivered and the socket has not taken yet."""

    def __init__(self):
        self._data = bytearray()
        self._lock = threading.Lock()
        self._closed = threading.Event()

    def feed(self, data):
        with self._lock:
            self._data.extend(data)
            overflow = len(self._data) - MAX_BUFFER
            if overflow > 0:
                # Drop the oldest: the speaker wants the present, which is the
                # rule the rest of the audio path follows too.
                del self._data[:overflow]

    def take(self, count):
        with self._lock:
            chunk = bytes(self._data[:count])
            del self._data[:count]
            return chunk

    def close(self):
        self._closed.set()

    def wait(self, timeout):
        """Sleeps, but wakes at once when stdin closes."""
        return self._closed.wait(timeout)

    @property
    def closed(self):
        return self._closed.is_set()


def read_stdin(buffer):
    """Reads go2rtc's audio until stdin closes, which is the client leaving."""
    fd = sys.stdin.fileno()
    while not buffer.closed:
        try:
            data = os.read(fd, 4096)
        except OSError:
            break
        if not data:
            break
        buffer.feed(data)
    buffer.close()


def send_loop(sock, buffer, pcm):
    """Sends one frame every 60 ms for as long as stdin stays open."""
    need = PCM_BYTES if pcm else FRAME_SAMPLES
    silence = (b"\x00" * need) if pcm else (bytes([ALAW_SILENCE]) * need)
    next_send = time.monotonic()

    while not buffer.closed:
        now = time.monotonic()
        if now < next_send:
            # Wait for the frame boundary; a closed stdin wakes this at once.
            buffer.wait(next_send - now)
            continue

        chunk = buffer.take(need)
        if not chunk:
            # Nothing to say: do not spend the camera's bandwidth on silence.
            next_send += FRAME_INTERVAL
            if next_send < now:
                next_send = now + FRAME_INTERVAL
            continue

        if len(chunk) < need:
            chunk += silence[: need - len(chunk)]

        frame = build_frame(chunk if pcm else alaw_to_pcm_bytes(chunk))
        sock.sendall(frame)

        next_send += FRAME_INTERVAL
        if next_send < now:
            # Behind by a frame or more: skip the backlog rather than fall
            # further behind. The speaker wants the present.
            next_send = now + FRAME_INTERVAL


def main():
    args = sys.argv[1:]
    pcm = "--pcm" in args
    args = [a for a in args if a != "--pcm"]
    if len(args) != 1:
        raise SystemExit(
            "usage: foscam_bridge.py <rtsp-url> [--pcm]\n"
            "  the URL supplies the camera's host, port and credentials;\n"
            "  --pcm reads s16le/8000 instead of alaw/8000"
        )

    host, port, user, password = parse_source(args[0])
    log(f"bridging {host}:{port} as {user} ({'s16le' if pcm else 'alaw'}/8000)")

    buffer = Buffer()
    threading.Thread(target=read_stdin, args=(buffer,), daemon=True).start()

    def stop(_signum, _frame):
        buffer.close()

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)

    while not buffer.closed:
        try:
            sock = connect(host, port, user, password)
        except Exception as err:
            log(f"cannot open the talk connection: {err}")
            if buffer.wait(2.0):
                break
            continue

        try:
            log("the talk connection is open")
            send_loop(sock, buffer, pcm)
        except Exception as err:
            log(f"the talk connection broke: {err}")
        finally:
            try:
                sock.close()
            except OSError:
                pass

        if not buffer.closed:
            buffer.wait(1.0)


if __name__ == "__main__":
    main()

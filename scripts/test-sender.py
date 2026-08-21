#!/usr/bin/env python3
"""Reference sender for Remote Input Bridge - a protocol test tool, not a product.

It speaks the exact wire protocol of the Windows sender (docs/PROTOCOL.md), so it can pair with
the macOS receiver, drive the cursor, click, scroll and type from any machine on the LAN. Use it
to verify a receiver build without a Windows box, and to reproduce packet loss deliberately.

    # first time: press "Show pairing code" on the Mac, then
    ./scripts/test-sender.py --host 127.0.0.1 --pair ABCD-EFGH --circle 5

    # afterwards the stored key is reused
    ./scripts/test-sender.py --host 192.168.1.123 --circle 10 --loss 3
"""

import argparse
import hashlib
import hmac
import json
import math
import os
import random
import socket
import struct
import sys
import time

PROTOCOL_VERSION = 1
UDP_MAGIC = 0x5249
TAG_LEN = 16

MSG_SESSION_START = 0x01
MSG_PING = 0x02
MSG_PONG = 0x03
MSG_TARGET_ACTIVE = 0x04
MSG_MOUSE_BUTTON = 0x05
MSG_SCROLL = 0x06
MSG_KEY = 0x07
MSG_MODIFIER_SYNC = 0x08
MSG_RELEASE_ALL = 0x09
MSG_MOUSE_MOVE_REL = 0x0A
MSG_BYE = 0x0B
MSG_EDGE_HIT = 0x0C

KEYS_PATH = os.path.expanduser("~/.config/remote-input-bridge/test-sender-keys.json")


# --- crypto (must match crypto.rs and Crypto.swift byte for byte) ---------------------

def hkdf32(ikm: bytes, salt: bytes, info: bytes) -> bytes:
    prk = hmac.new(salt, ikm, hashlib.sha256).digest()
    out, block, counter = b"", b"", 1
    while len(out) < 32:
        block = hmac.new(prk, block + info + bytes([counter]), hashlib.sha256).digest()
        out += block
        counter += 1
    return out[:32]


def mac(key: bytes, data: bytes) -> bytes:
    return hmac.new(key, data, hashlib.sha256).digest()


def normalize_code(code: str) -> str:
    return "".join(c for c in code if c.isalnum()).upper()


def pairing_key(code: str) -> bytes:
    return hkdf32(normalize_code(code).encode(), b"rib-pair-v1", b"pairing")


def session_keys(device_key: bytes, cn: bytes, sn: bytes):
    salt = cn + sn
    return (
        hkdf32(device_key, salt, b"rib-session-v1|tcp"),
        hkdf32(device_key, salt, b"rib-session-v1|udp"),
    )


# --- framing -------------------------------------------------------------------------

class Sender:
    def __init__(self, host, tcp_port, udp_port, name):
        self.host = host
        self.tcp_port = tcp_port
        self.udp_port = udp_port
        self.name = name
        self.sock = None
        self.udp = None
        self.tcp_key = b""
        self.udp_key = b""
        self.session_id = 0
        self.send_counter = 0
        self.recv_counter = 0
        self.sequence = 0
        self.total_x = 0
        self.total_y = 0
        self.started = time.monotonic()

    def now_us(self):
        return int((time.monotonic() - self.started) * 1_000_000)

    # framing
    def write_frame(self, payload: bytes):
        self.sock.sendall(struct.pack(">I", len(payload)) + payload)

    def read_frame(self) -> bytes:
        header = self.read_exact(4)
        (length,) = struct.unpack(">I", header)
        if not 0 < length <= 65536:
            raise RuntimeError(f"bad frame length {length}")
        return self.read_exact(length)

    def read_exact(self, count: int) -> bytes:
        chunks = b""
        while len(chunks) < count:
            piece = self.sock.recv(count - len(chunks))
            if not piece:
                raise ConnectionError("receiver closed the connection")
            chunks += piece
        return chunks

    def send_json(self, obj):
        self.write_frame(json.dumps(obj).encode())

    def read_json(self):
        text = self.read_frame().decode()
        obj = json.loads(text)
        if obj.get("t") == "ERROR":
            raise RuntimeError(f"receiver refused us: {obj.get('code')} - {obj.get('message')}")
        return obj

    def send_reliable(self, msg_type: int, body: bytes = b""):
        self.send_counter += 1
        signed = struct.pack(">Q", self.send_counter) + bytes([msg_type]) + body
        self.write_frame(signed + mac(self.tcp_key, signed)[:TAG_LEN])

    def read_reliable(self):
        payload = self.read_frame()
        signed, tag = payload[:-TAG_LEN], payload[-TAG_LEN:]
        if not hmac.compare_digest(mac(self.tcp_key, signed)[:TAG_LEN], tag):
            raise RuntimeError("bad tag on a frame from the receiver")
        (counter,) = struct.unpack(">Q", signed[:8])
        if counter <= self.recv_counter:
            raise RuntimeError(f"replayed counter {counter}")
        self.recv_counter = counter
        return signed[8], signed[9:]

    # handshake
    def connect(self, pair_code=None):
        keys = load_keys()
        client_id = keys.setdefault("client_id", os.urandom(16).hex())
        self.sock = socket.create_connection((self.host, self.tcp_port), timeout=5)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        client_nonce = os.urandom(32)

        self.send_json({
            "t": "HELLO",
            "protocol_version": PROTOCOL_VERSION,
            "client_name": self.name,
            "client_id": client_id,
            "client_nonce": client_nonce.hex(),
            "capabilities": ["mouse_move_udp", "mouse_buttons", "scroll_hires", "keyboard",
                             "heartbeat", "edge_switch"],
        })
        ack = self.read_json()
        assert ack["t"] == "HELLO_ACK", ack
        if ack.get("protocol_version") != PROTOCOL_VERSION:
            raise RuntimeError(f"receiver speaks protocol {ack.get('protocol_version')}")
        server_nonce = bytes.fromhex(ack["server_nonce"])
        print(f"    receiver: {ack.get('server_name')!r} known={ack.get('known_client')} "
              f"pairing_mode={ack.get('pairing_mode')}")

        device_key = None
        stored = keys.get("devices", {}).get(self.host)
        if pair_code:
            if not ack.get("pairing_mode"):
                raise RuntimeError("the Mac is not in pairing mode; press 'Show pairing code'")
            pk = pairing_key(pair_code)
            proof = mac(pk, b"pair" + client_nonce + server_nonce)
            self.send_json({"t": "PAIR_REQUEST", "proof": proof.hex()})
            resp = self.read_json()
            assert resp["t"] == "PAIR_RESPONSE", resp
            wrapped = bytes.fromhex(resp["wrapped_key"])
            expected = mac(pk, b"wrap" + client_nonce + server_nonce + wrapped)
            if not hmac.compare_digest(expected, bytes.fromhex(resp["tag"])):
                raise RuntimeError("wrong pairing code")
            mask = hkdf32(pk, b"rib-pair-v1", b"wrap")
            device_key = bytes(a ^ b for a, b in zip(wrapped, mask))
            keys.setdefault("devices", {})[self.host] = device_key.hex()
            save_keys(keys)
            print("    paired; the device key is stored")
        elif stored:
            device_key = bytes.fromhex(stored)
        else:
            raise RuntimeError("no stored key for this host: run once with --pair CODE")

        proof = mac(device_key, b"auth" + client_nonce + server_nonce)
        self.send_json({"t": "AUTH", "proof": proof.hex()})
        ok = self.read_json()
        assert ok["t"] == "AUTH_OK", ok
        self.session_id = int(ok["session_id"])
        expected = mac(device_key, b"auth-ack" + client_nonce + server_nonce
                       + struct.pack(">Q", self.session_id))
        if not hmac.compare_digest(expected, bytes.fromhex(ok["server_proof"])):
            raise RuntimeError("the receiver could not prove it holds our device key")
        self.tcp_key, self.udp_key = session_keys(device_key, client_nonce, server_nonce)
        self.udp_port = int(ok.get("udp_port") or self.udp_port)
        print(f"    session 0x{self.session_id:x}, udp port {self.udp_port}")

        self.udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.udp.connect((self.host, self.udp_port))

    # realtime
    def send_move(self, dx, dy, loss_percent=0.0):
        self.total_x = (self.total_x + dx) & 0xFFFFFFFF
        self.total_y = (self.total_y + dy) & 0xFFFFFFFF
        self.sequence += 1
        packet = struct.pack(
            ">HBBQQQii",
            UDP_MAGIC, 1, 1, self.session_id, self.sequence, self.now_us(),
            struct.unpack(">i", struct.pack(">I", self.total_x))[0],
            struct.unpack(">i", struct.pack(">I", self.total_y))[0],
        )
        packet += mac(self.udp_key, packet)[:8]
        assert len(packet) == 44, len(packet)
        # Deliberate loss, so "does the cursor drift after a dropped packet?" is testable.
        if loss_percent > 0 and random.random() * 100 < loss_percent:
            return False
        self.udp.send(packet)
        return True

    def close(self):
        try:
            self.send_reliable(MSG_TARGET_ACTIVE, b"\x00")
            self.send_reliable(MSG_RELEASE_ALL)
            self.send_reliable(MSG_BYE, b"\x00")
        except Exception:
            pass
        if self.sock:
            self.sock.close()


def load_keys():
    try:
        with open(KEYS_PATH) as handle:
            return json.load(handle)
    except Exception:
        return {}


def save_keys(keys):
    os.makedirs(os.path.dirname(KEYS_PATH), exist_ok=True)
    with open(KEYS_PATH, "w") as handle:
        json.dump(keys, handle, indent=2)
    os.chmod(KEYS_PATH, 0o600)


def drain_incoming(sender, deadline_stats):
    """Non-blocking read of whatever the receiver sent us (PONG, EDGE_HIT)."""
    sender.sock.settimeout(0)
    try:
        while True:
            try:
                msg_type, body = sender.read_reliable()
            except (BlockingIOError, socket.timeout):
                return
            except BlockingIOError:
                return
            if msg_type == MSG_PONG:
                sent, applied, received, dropped = struct.unpack(">QQII", body[:24])
                rtt = (sender.now_us() - sent) / 1000.0
                deadline_stats.append((rtt, applied, received, dropped))
            elif msg_type == MSG_EDGE_HIT:
                print("    receiver reports a right-edge hit")
    finally:
        sender.sock.settimeout(5)


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--tcp-port", type=int, default=47821)
    parser.add_argument("--udp-port", type=int, default=47822)
    parser.add_argument("--pair", metavar="CODE", help="pairing code shown on the Mac")
    parser.add_argument("--name", default=socket.gethostname())
    parser.add_argument("--rate", type=int, default=500, help="packets per second (default 500)")
    parser.add_argument("--circle", type=float, default=0, metavar="SECONDS",
                        help="move the cursor in a circle for N seconds")
    parser.add_argument("--radius", type=float, default=120)
    parser.add_argument("--loss", type=float, default=0, metavar="PERCENT",
                        help="drop this share of movement packets on purpose")
    parser.add_argument("--click", action="store_true", help="left click once")
    parser.add_argument("--scroll", type=int, default=0, metavar="NOTCHES")
    parser.add_argument("--type", dest="type_text", metavar="TEXT",
                        help="type ASCII letters/digits/space")
    parser.add_argument("--handshake-only", action="store_true")
    parser.add_argument("--stuck-modifier", action="store_true",
                        help="hold left Shift and then hang: kill -9 this process to verify the "
                             "receiver releases it on heartbeat timeout (spec test 5)")
    parser.add_argument("--vectors", action="store_true",
                        help="print crypto test vectors and exit")
    args = parser.parse_args()

    if args.vectors:
        pk = pairing_key("A4C9-K2MN")
        print("pairing_key(A4C9-K2MN) =", pk.hex())
        print("wrap_mask              =", hkdf32(pk, b"rib-pair-v1", b"wrap").hex())
        tcp, udp = session_keys(bytes(range(32)), b"client-nonce", b"server-nonce")
        print("session_tcp            =", tcp.hex())
        print("session_udp            =", udp.hex())
        print("hmac(k=01*32, 'abc')   =", mac(bytes([1] * 32), b"abc").hex())
        return 0

    sender = Sender(args.host, args.tcp_port, args.udp_port, args.name)
    print(f"==> connecting to {args.host}:{args.tcp_port}")
    sender.connect(args.pair)
    if args.handshake_only:
        sender.close()
        print("==> handshake OK")
        return 0

    interval_us = 2000
    sender.send_reliable(MSG_SESSION_START, struct.pack(">IB", interval_us, 0))
    sender.send_reliable(MSG_RELEASE_ALL)
    sender.send_reliable(MSG_MODIFIER_SYNC, struct.pack(">H", 0))
    sender.send_reliable(MSG_TARGET_ACTIVE, b"\x01")
    print("==> the Mac now has the input")

    if args.stuck_modifier:
        sender.send_reliable(MSG_KEY, struct.pack(">HBB", 0xE1, 1, 0))
        sender.send_reliable(MSG_MODIFIER_SYNC, struct.pack(">H", 0x0002))
        print("==> left Shift is held; kill -9 this process to test modifier recovery")
        # Keep the heartbeat alive so the timeout can only be caused by this process dying.
        while True:
            sender.send_reliable(MSG_PING, struct.pack(">Q", sender.now_us()))
            drain_incoming(sender, [])
            time.sleep(0.3)

    stats = []
    sent = dropped = 0
    if args.circle > 0:
        period = 1.0 / args.rate
        steps = int(args.circle * args.rate)
        last_ping = 0.0
        # Two turns over the whole run, so both slow and fast motion is exercised.
        for step in range(steps):
            angle = 2 * math.pi * 2 * step / steps
            next_angle = 2 * math.pi * 2 * (step + 1) / steps
            dx = round(args.radius * (math.cos(next_angle) - math.cos(angle)))
            dy = round(args.radius * (math.sin(next_angle) - math.sin(angle)))
            if dx or dy:
                if sender.send_move(dx, dy, args.loss):
                    sent += 1
                else:
                    dropped += 1
            now = time.monotonic()
            if now - last_ping > 0.3:
                last_ping = now
                sender.send_reliable(MSG_PING, struct.pack(">Q", sender.now_us()))
            # Drained every tick: reading the PONG late would inflate the measured RTT by
            # however long we waited, which is exactly the bug this tool is meant to find.
            drain_incoming(sender, stats)
            time.sleep(period)

    if args.scroll:
        sender.send_reliable(MSG_SCROLL, struct.pack(">ii", 0, args.scroll * 120))
        time.sleep(0.2)
    if args.click:
        sender.send_reliable(MSG_MOUSE_BUTTON, bytes([0, 1]))
        time.sleep(0.05)
        sender.send_reliable(MSG_MOUSE_BUTTON, bytes([0, 0]))
        time.sleep(0.2)
    if args.type_text:
        usages = {**{chr(ord("a") + i): 0x04 + i for i in range(26)},
                  **{str(d): 0x1E + d - 1 for d in range(1, 10)},
                  "0": 0x27, " ": 0x2C, "\n": 0x28}
        for character in args.type_text.lower():
            usage = usages.get(character)
            if usage is None:
                continue
            sender.send_reliable(MSG_KEY, struct.pack(">HBB", usage, 1, 0))
            time.sleep(0.01)
            sender.send_reliable(MSG_KEY, struct.pack(">HBB", usage, 0, 0))
            time.sleep(0.02)

    # Wait for this PONG properly instead of sleeping first: a blind sleep would report its own
    # duration as the round trip time.
    before = len(stats)
    sender.send_reliable(MSG_PING, struct.pack(">Q", sender.now_us()))
    deadline = time.monotonic() + 0.5
    while len(stats) == before and time.monotonic() < deadline:
        drain_incoming(sender, stats)
        time.sleep(0.001)
    sender.close()

    print(f"==> sent {sent} movement packets, deliberately dropped {dropped}")
    if stats:
        rtts = [entry[0] for entry in stats]
        rtts.sort()
        print(f"    rtt min {rtts[0]:.2f} ms, median {rtts[len(rtts) // 2]:.2f} ms, "
              f"max {rtts[-1]:.2f} ms")
        last = stats[-1]
        mean = sum(rtts) / len(rtts)
        jitter = sum(abs(rtt - mean) for rtt in rtts) / len(rtts)
        print(f"    rtt mean {mean:.2f} ms, jitter {jitter:.2f} ms, samples {len(rtts)}")
        print(f"    receiver: applied {last[1]} events, udp received {last[2]}, missing {last[3]}")
    else:
        print("    no PONG received - the receiver never answered a heartbeat")
    return 0


if __name__ == "__main__":
    sys.exit(main())

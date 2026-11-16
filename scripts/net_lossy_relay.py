#!/usr/bin/env python3
"""A lossy relay between the guest's QEMU and the slirp QEMU (S9-SPEC 3.1, Q30).

Both QEMUs speak QEMU's socket-netdev framing (a 4-byte big-endian length, then one Ethernet frame). The relay listens for the
guest's QEMU (`-netdev socket,connect=`), connects to the slirp QEMU (`-netdev socket,listen=`) and forwards frames both ways,
dropping, duplicating, reordering and delaying them from a seeded generator, so a run can be repeated. Only IPv4 TCP frames are
disturbed: ARP, DHCP, ICMP and UDP pass untouched (the TCP stack is what is under test, and the link comes up cleanly).

    net_lossy_relay.py --listen PORT --slirp PORT --seed N [--loss P] [--dup P] [--reorder P] [--delay MS]

Probabilities are fractions (0.02 is two percent). `--selftest` checks the framing and the disturbances without a QEMU.
"""
import argparse, heapq, random, selectors, socket, struct, sys, time


def is_tcp(frame: bytes) -> bool:
    return len(frame) >= 34 and frame[12:14] == b"\x08\x00" and frame[23] == 6


class Disturb:
    """The decision for one frame: a list of (extra delay in seconds) deliveries; empty means dropped."""

    def __init__(self, seed, loss, dup, reorder, delay_ms):
        self.r = random.Random(seed)
        self.loss, self.dup, self.reorder, self.delay = loss, dup, reorder, delay_ms / 1000.0
        self.stats = {"seen": 0, "lost": 0, "duplicated": 0, "reordered": 0}

    def plan(self, frame):
        if not is_tcp(frame):
            return [0.0]
        self.stats["seen"] += 1
        if self.r.random() < self.loss:
            self.stats["lost"] += 1
            return []
        d = self.delay
        if self.r.random() < self.reorder:
            self.stats["reordered"] += 1
            d += self.delay * 2 + 0.002  # held back past the frames behind it
        out = [d]
        if self.r.random() < self.dup:
            self.stats["duplicated"] += 1
            out.append(d + 0.0005)
        return out


class Side:
    def __init__(self, sock, name):
        self.sock, self.name, self.buf = sock, name, b""
        self.out = []  # heap of (due, n, frame)

    def frames(self):
        data = self.sock.recv(65536)
        if not data:
            return None
        self.buf += data
        res = []
        while len(self.buf) >= 4:
            n = struct.unpack("!I", self.buf[:4])[0]
            if len(self.buf) < 4 + n:
                break
            res.append(self.buf[4:4 + n])
            self.buf = self.buf[4 + n:]
        return res


def relay(listen_port, slirp_port, dist, quiet=False):
    ls = socket.socket()
    ls.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    ls.bind(("127.0.0.1", listen_port))
    ls.listen(1)
    slirp = socket.create_connection(("127.0.0.1", slirp_port), timeout=10)
    slirp.settimeout(None)
    guest_sock, _ = ls.accept()
    ls.close()
    sides = {"guest": Side(guest_sock, "guest"), "slirp": Side(slirp, "slirp")}
    peer = {"guest": "slirp", "slirp": "guest"}
    sel = selectors.DefaultSelector()
    for k, s in sides.items():
        s.sock.setblocking(False)
        sel.register(s.sock, selectors.EVENT_READ, k)
    seq = 0
    alive = True
    while alive:
        now = time.monotonic()
        due = [s.out[0][0] for s in sides.values() if s.out]
        timeout = max(0.0, min(due) - now) if due else 1.0
        for key, _ in sel.select(timeout):
            side = sides[key.data]
            try:
                frames = side.frames()
            except (BlockingIOError, ConnectionError):
                frames = None
            if frames is None:
                alive = False
                break
            for f in frames:
                for d in dist.plan(f):
                    seq += 1
                    heapq.heappush(sides[peer[side.name]].out, (time.monotonic() + d, seq, f))
        now = time.monotonic()
        for s in sides.values():
            while s.out and s.out[0][0] <= now:
                _, _, f = heapq.heappop(s.out)
                try:
                    s.sock.setblocking(True)
                    s.sock.sendall(struct.pack("!I", len(f)) + f)
                    s.sock.setblocking(False)
                except OSError:
                    alive = False
    for s in sides.values():
        s.sock.close()
    if not quiet:
        print("lossy relay:", dist.stats, file=sys.stderr)


def selftest():
    d = Disturb(1, 0.1, 0.1, 0.1, 5)
    tcp = bytes(12) + b"\x08\x00" + bytes(9) + b"\x06" + bytes(30)
    arp = bytes(12) + b"\x08\x06" + bytes(30)
    assert d.plan(arp) == [0.0] and d.stats["seen"] == 0, "non-TCP frames are never disturbed"
    plans = [d.plan(tcp) for _ in range(20000)]
    lost = sum(1 for p in plans if not p)
    dup = sum(1 for p in plans if len(p) == 2)
    assert 1700 < lost < 2300, lost
    assert 1300 < dup < 1900, dup  # (ten percent of those not lost)
    assert d.stats["reordered"] > 1300
    a = [Disturb(7, 0.05, 0.05, 0.05, 5).plan(tcp) for _ in range(50)]
    b = [Disturb(7, 0.05, 0.05, 0.05, 5).plan(tcp) for _ in range(50)]
    assert a == b, "a seed repeats"
    # the framing across a socket pair
    x, y = socket.socketpair()
    f = Side(y, "t")
    x.sendall(struct.pack("!I", 5) + b"hello" + struct.pack("!I", 3) + b"ab")
    assert f.frames() == [b"hello"]
    x.sendall(b"c")
    assert f.frames() == [b"abc"]
    print("net_lossy_relay selftest passed")


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--listen", type=int)
    ap.add_argument("--slirp", type=int)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--loss", type=float, default=0.02)
    ap.add_argument("--dup", type=float, default=0.01)
    ap.add_argument("--reorder", type=float, default=0.05)
    ap.add_argument("--delay", type=float, default=5)
    ap.add_argument("--selftest", action="store_true")
    a = ap.parse_args()
    if a.selftest:
        selftest()
    else:
        relay(a.listen, a.slirp, Disturb(a.seed, a.loss, a.dup, a.reorder, a.delay))

#!/usr/bin/env python3
"""Probe QEMU's user-mode network (slirp) with a scripted fake guest, for NET-SPEC 3.2.

QEMU runs with no guest at all: slirp and a socket netdev share a hub, and this script is the "guest" on the
socket side (QEMU's socket netdev frames each Ethernet frame with a 4-byte big-endian length). It sends an ARP
request, a DHCP exchange, an ICMP echo and a host-to-guest UDP datagram through hostfwd, prints what slirp
answers, and writes everything it saw to a pcap.  Needs only the Python standard library and qemu-system-riscv64.
"""
import os, socket, struct, subprocess, sys, time, random, tempfile

QEMU = os.environ.get("QEMU", "qemu-system-riscv64")
GMAC = bytes.fromhex("525400123456")
BCAST = b"\xff" * 6

def csum(b):
    if len(b) % 2: b += b"\0"
    s = sum(struct.unpack("!%dH" % (len(b) // 2), b))
    while s >> 16: s = (s & 0xffff) + (s >> 16)
    return (~s) & 0xffff

def ip(src, dst, proto, payload, ident=1):
    h = struct.pack("!BBHHHBBH4s4s", 0x45, 0, 20 + len(payload), ident, 0, 64, proto, 0, src, dst)
    h = h[:10] + struct.pack("!H", csum(h)) + h[12:]
    return h + payload

def udp(src, dst, sp, dp, data):
    u = struct.pack("!HHHH", sp, dp, 8 + len(data), 0) + data
    pseudo = src + dst + struct.pack("!BBH", 0, 17, len(u))
    c = csum(pseudo + u) or 0xffff
    return u[:6] + struct.pack("!H", c) + u[8:]

def eth(dst, src, typ, payload):
    return dst + src + struct.pack("!H", typ) + payload

def dhcp(mtype, xid, extra=b"", ciaddr=b"\0" * 4):
    b = struct.pack("!BBBBIHH4s4s4s4s16s64s128s", 1, 1, 6, 0, xid, 0, 0x8000, ciaddr, b"\0" * 4, b"\0" * 4, b"\0" * 4,
                    GMAC + b"\0" * 10, b"", b"")
    b += b"\x63\x82\x53\x63" + bytes([53, 1, mtype]) + extra + bytes([55, 4, 1, 3, 6, 51, 255])
    return b

def parse_dhcp_options(p):
    opts = {}
    i = 240
    while i < len(p) and p[i] != 255:
        if p[i] == 0: i += 1; continue
        opts[p[i]] = p[i + 2:i + 2 + p[i + 1]]
        i += 2 + p[i + 1]
    return opts

class Guest:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=3)
        self.log = []
    def send(self, frame):
        self.log.append((time.time(), frame))
        self.s.sendall(struct.pack("!I", len(frame)) + frame)
    def recv(self, timeout=2.0):
        self.s.settimeout(timeout)
        try:
            n = struct.unpack("!I", self._read(4))[0]
            f = self._read(n)
        except (socket.timeout, TimeoutError):
            return None
        self.log.append((time.time(), f))
        return f
    def _read(self, n):
        b = b""
        while len(b) < n:
            c = self.s.recv(n - len(b))
            if not c: raise EOFError
            b += c
        return b
    def drain_until(self, pred, timeout=3.0):
        end = time.time() + timeout
        while time.time() < end:
            f = self.recv(max(0.05, end - time.time()))
            if f is None: return None
            if pred(f): return f
        return None

def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p

def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "slirp-probe.pcap"
    qport, hport = free_port(), free_port()
    cmd = [QEMU, "-machine", "none", "-nographic", "-nodefaults", "-monitor", "none", "-serial", "none",
           "-netdev", "user,id=u0,net=10.0.2.0/24,host=10.0.2.2,dhcpstart=10.0.2.15,hostfwd=udp:127.0.0.1:%d-:7777" % hport,
           "-netdev", "hubport,id=h0,hubid=0,netdev=u0",
           "-netdev", "socket,id=s0,listen=127.0.0.1:%d" % qport,
           "-netdev", "hubport,id=h1,hubid=0,netdev=s0"]
    q = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    try:
        for _ in range(50):
            try: g = Guest(qport); break
            except OSError: time.sleep(0.1)
        else:
            print("could not connect:", q.stdout.read(500)); return 1
        res = {}
        # 1. ARP who-has 10.0.2.2 and 10.0.2.3
        for target in ("10.0.2.2", "10.0.2.3", "10.0.2.4", "10.0.2.99"):
            arp = struct.pack("!HHBBH6s4s6s4s", 1, 0x0800, 6, 4, 1, GMAC, bytes([10, 0, 2, 15]), b"\0" * 6, socket.inet_aton(target))
            g.send(eth(BCAST, GMAC, 0x0806, arp))
            f = g.drain_until(lambda f: f[12:14] == b"\x08\x06" and f[20:22] == b"\x00\x02" and f[28:32] == socket.inet_aton(target), 1.0)
            res["arp " + target] = f[22:28].hex() if f else None
        # 2. DHCP
        xid = random.getrandbits(32)
        g.send(eth(BCAST, GMAC, 0x0800, ip(b"\0" * 4, b"\xff" * 4, 17, udp(b"\0" * 4, b"\xff" * 4, 68, 67, dhcp(1, xid)))))
        offer = g.drain_until(lambda f: f[12:14] == b"\x08\x00" and f[23] == 17 and f[36:38] == b"\x00\x44")
        if offer:
            p = offer[42:]
            o = parse_dhcp_options(p)
            res["dhcp offer yiaddr"] = socket.inet_ntoa(p[16:20])
            res["dhcp offer src mac/ip"] = offer[6:12].hex() + " " + socket.inet_ntoa(offer[26:30])
            res["dhcp offer dst"] = offer[0:6].hex() + " " + socket.inet_ntoa(offer[30:34])
            res["dhcp offer options"] = {k: v.hex() for k, v in o.items()}
            req = bytes([50, 4]) + p[16:20] + bytes([54, 4]) + o[54]
            g.send(eth(BCAST, GMAC, 0x0800, ip(b"\0" * 4, b"\xff" * 4, 17, udp(b"\0" * 4, b"\xff" * 4, 68, 67, dhcp(3, xid, req)))))
            ack = g.drain_until(lambda f: f[12:14] == b"\x08\x00" and f[23] == 17 and f[36:38] == b"\x00\x44" and parse_dhcp_options(f[42:]).get(53) == b"\x05")
            res["dhcp ack"] = bool(ack)
            if ack:
                oa = parse_dhcp_options(ack[42:])
                res["dhcp ack options"] = {k: v.hex() for k, v in oa.items()}
                lease = struct.unpack("!I", oa[51])[0]
                res["lease seconds"] = lease
        # 3. ICMP echo to the gateway and to DNS (10.0.2.3)
        for dst in ("10.0.2.2", "10.0.2.3"):
            echo = struct.pack("!BBHHH", 8, 0, 0, 0x1234, 1) + bytes(range(32))
            echo = echo[:2] + struct.pack("!H", csum(echo)) + echo[4:]
            g.send(eth(b"\0" * 6 if False else (bytes.fromhex(res["arp 10.0.2.2"]) if res.get("arp 10.0.2.2") else BCAST), GMAC, 0x0800,
                       ip(bytes([10, 0, 2, 15]), socket.inet_aton(dst), 1, echo)))
            rep = g.drain_until(lambda f: f[12:14] == b"\x08\x00" and f[23] == 1 and f[34] == 0, 1.5)
            res["echo reply from " + dst] = (socket.inet_ntoa(rep[26:30]) + " ttl=%d len=%d ident=%04x" % (rep[22], len(rep), struct.unpack("!H", rep[38:40])[0])) if rep else None
        # 4. host -> guest UDP through hostfwd, sizes
        h = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); h.bind(("127.0.0.1", 0)); hp = h.getsockname()[1]
        for size in (1, 100, 1472, 1473, 3000):
            payload = bytes(random.getrandbits(8) for _ in range(size))
            h.sendto(payload, ("127.0.0.1", hport))
            f = g.drain_until(lambda f: f[12:14] == b"\x08\x00" and f[23] == 17 and f[36:38] == b"\x1e\x61", 2.0)
            if f:
                ihl = (f[14] & 15) * 4
                res["hostfwd %d" % size] = dict(src=socket.inet_ntoa(f[26:30]), dst=socket.inet_ntoa(f[30:34]), sport=struct.unpack("!H", f[34:36])[0],
                                                dport=struct.unpack("!H", f[36:38])[0], frag=struct.unpack("!H", f[20:22])[0] & 0x3fff,
                                                payload_ok=f[14 + ihl + 8:14 + ihl + 8 + size] == payload, framelen=len(f))
                # echo it back to the host, as the guest's udpecho would
                back = eth(f[6:12], GMAC, 0x0800, ip(f[30:34], f[26:30], 17, udp(f[30:34], f[26:30], 7777, struct.unpack("!H", f[34:36])[0], payload), ident=7))
                g.send(back)
                h.settimeout(2.0)
                try:
                    data, addr = h.recvfrom(4096)
                    res["host got echo %d" % size] = (data == payload, addr[1] == hport)
                except socket.timeout:
                    res["host got echo %d" % size] = None
            else:
                res["hostfwd %d" % size] = None
        # 5. a frame the stack should never have to send but slirp may: ask the DNS address for a port-unreachable
        g.send(eth(bytes.fromhex(res["arp 10.0.2.2"]) if res.get("arp 10.0.2.2") else BCAST, GMAC, 0x0800,
                   ip(bytes([10, 0, 2, 15]), bytes([10, 0, 2, 2]), 17, udp(bytes([10, 0, 2, 15]), bytes([10, 0, 2, 2]), 5000, 9, b"x"))))
        unr = g.drain_until(lambda f: f[12:14] == b"\x08\x00" and f[23] == 1, 1.0)
        res["udp to gateway:9"] = ("icmp type %d code %d" % (unr[34], unr[35])) if unr else "no answer"
        for k, v in res.items():
            print("%-26s %s" % (k, v))
        # pcap
        with open(out, "wb") as fh:
            fh.write(struct.pack("<IHHiIII", 0xa1b2c3d4, 2, 4, 0, 0, 65535, 1))
            for t, fr in g.log:
                fh.write(struct.pack("<IIII", int(t), int((t % 1) * 1e6), len(fr), len(fr)) + fr)
        print("wrote", out, len(g.log), "frames")
        return 0
    finally:
        q.terminate()

if __name__ == "__main__":
    sys.exit(main())

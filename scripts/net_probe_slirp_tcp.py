#!/usr/bin/env python3
"""Probe QEMU's user-mode network (slirp) for TCP, with a scripted fake guest (S9-SPEC 3.2, step 0).

The same topology as `net_probe_slirp.py`: QEMU runs with no machine, slirp and a socket netdev share a hub, and this
script is the "guest" on the socket side (4-byte big-endian length-framed Ethernet). Here it speaks TCP: a minimal
hand-rolled endpoint, enough to open, send, acknowledge, withhold acknowledgements, reset and close. Host-side servers
and clients run in threads of this script. Every claim of S9-SPEC 3.2 is a row printed by `main`; nothing is copied
from the spec. Python standard library and `qemu-system-riscv64` only.

    python3 scripts/net_probe_slirp_tcp.py OUT.pcap          # the committed capture is made this way
    python3 scripts/net_probe_slirp_tcp.py OUT.pcap --long   # also the slow rows (a 685 KB unread stream, a 9 s SYN wait)

The pcap holds every frame of the quick rows (the bulk rows are logged too, kept small); `net_pcap_check.py` verifies
its checksums, TCP included.
"""
import hashlib, os, socket, struct, subprocess, sys, threading, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from net_probe_slirp import QEMU, GMAC, BCAST, Guest, csum, eth, ip, free_port  # noqa: E402

ME, GW = bytes([10, 0, 2, 15]), bytes([10, 0, 2, 2])
FIN, SYN, RST, PSH, ACK = 1, 2, 4, 8, 16


def tcp_seg(src, dst, sp, dp, seq, ack, flags, win, opts=b"", data=b""):
    off = (20 + len(opts)) // 4
    h = struct.pack("!HHIIBBHHH", sp, dp, seq & 0xffffffff, ack & 0xffffffff, off << 4, flags, win, 0, 0) + opts
    pseudo = src + dst + struct.pack("!BBH", 0, 6, len(h) + len(data))
    c = csum(pseudo + h + data)
    return h[:16] + struct.pack("!H", c) + h[18:] + data


class Seg:
    """A parsed TCP segment from a frame, or None."""
    def __init__(self, f):
        self.ok = False
        if len(f) < 54 or f[12:14] != b"\x08\x00" or f[23] != 6:
            return
        ihl = (f[14] & 15) * 4
        total = struct.unpack("!H", f[16:18])[0]
        t = f[14 + ihl:14 + total]
        if len(t) < 20:
            return
        self.ok = True
        self.src, self.dst = f[26:30], f[30:34]
        self.ttl, self.df = f[22], bool(struct.unpack("!H", f[20:22])[0] & 0x4000)
        self.frag = struct.unpack("!H", f[20:22])[0] & 0x3fff
        self.sp, self.dp, self.seq, self.ack = struct.unpack("!HHII", t[:12])
        off = (t[12] >> 4) * 4
        self.flags = t[13]
        self.win = struct.unpack("!H", t[14:16])[0]
        self.opts = self._opts(t[20:off])
        self.data = t[off:]
        self.framelen = len(f)
        self.t = None

    @staticmethod
    def _opts(b):
        o, i = {}, 0
        while i < len(b):
            k = b[i]
            if k == 0:
                break
            if k == 1:
                i += 1
                continue
            n = b[i + 1]
            o[k] = b[i + 2:i + n]
            i += n
        return o

    def f(self, bit):
        return bool(self.flags & bit)


class Tcp:
    """One connection of the fake guest. `seq` is our next sequence number, `rcv` the next one we expect."""

    def __init__(self, g, gwmac, sport, dport, dst=GW, src=ME):
        self.g, self.mac, self.sp, self.dp, self.dst, self.src = g, gwmac, sport, dport, dst, src
        self.seq, self.rcv, self.win = 1000 + sport, 0, 65535

    def send(self, flags, data=b"", opts=b"", win=None, seq=None, ack=None):
        seg = tcp_seg(self.src, self.dst, self.sp, self.dp, self.seq if seq is None else seq,
                      self.rcv if ack is None else ack, flags, self.win if win is None else win, opts, data)
        self.g.send(eth(self.mac, GMAC, 0x0800, ip(self.src, self.dst, 6, seg, ident=self.seq & 0xffff)))

    def mine(self, f):
        s = Seg(f)
        return s if s.ok and s.dp == self.sp and s.sp == self.dp else None

    def wait(self, pred, timeout=3.0):
        end = time.time() + timeout
        while time.time() < end:
            f = self.g.recv(max(0.02, end - time.time()))
            if f is None:
                return None
            s = self.mine(f)
            if s:
                s.t = time.time()
                if pred(s):
                    return s
        return None

    def connect(self, opts=b"\x02\x04\x05\xb4", win=65535, timeout=3.0):
        self.win = win
        self.send(SYN, opts=opts)
        sa = self.wait(lambda s: s.f(SYN) or s.f(RST), timeout)
        if sa is None or sa.f(RST):
            return sa
        self.seq += 1
        self.rcv = sa.seq + 1
        self.send(ACK)
        return sa

    def read(self, want, ack_every=True, timeout=5.0, drop=lambda n: False, stop_on_fin=True):
        """Receive up to `want` in-order bytes, acknowledging as we go. `drop(n)` makes us ignore the n-th data segment
        as if the network lost it. Returns (data, [segments seen], got_fin)."""
        got, segs, n, fin = b"", [], 0, False
        end = time.time() + timeout
        while len(got) < want and time.time() < end:
            s = self.wait(lambda s: True, max(0.05, end - time.time()))
            if s is None:
                break
            if s.data or s.f(FIN):
                n += 1
                segs.append(s)
                if drop(n) and s.data:
                    continue
                if s.seq == self.rcv and s.data:
                    got += s.data
                    self.rcv += len(s.data)
                    end = time.time() + timeout
                if s.f(FIN) and s.seq + len(s.data) == self.rcv:
                    self.rcv += 1
                    fin = True
                if ack_every:
                    self.send(ACK)
                if fin and stop_on_fin:
                    break
        return got, segs, fin


def serve_once(handler):
    """A host TCP server on a free port; `handler(conn)` runs in a thread per connection. Returns (port, stop)."""
    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", 0))
    srv.listen(8)
    stop = threading.Event()

    def loop():
        srv.settimeout(0.2)
        while not stop.is_set():
            try:
                c, _ = srv.accept()
            except OSError:
                continue
            threading.Thread(target=handler, args=(c,), daemon=True).start()
        srv.close()
    threading.Thread(target=loop, daemon=True).start()
    return srv.getsockname()[1], stop


def blob(n):
    return (hashlib.sha256(b"seed").digest() * (n // 32 + 1))[:n]


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    out = args[0] if args else "slirp-tcp-probe.pcap"
    long = "--long" in sys.argv
    qport, fport = free_port(), free_port()
    ssock = socket.socket()
    ssock.bind(("127.0.0.1", 0))
    tport = ssock.getsockname()[1]
    ssock.close()
    cmd = [QEMU, "-machine", "none", "-nographic", "-nodefaults", "-monitor", "none", "-serial", "none",
           "-netdev", "user,id=u0,net=10.0.2.0/24,host=10.0.2.2,dhcpstart=10.0.2.15,hostfwd=tcp:127.0.0.1:%d-:8080" % fport,
           "-netdev", "hubport,id=h0,hubid=0,netdev=u0",
           "-netdev", "socket,id=s0,listen=127.0.0.1:%d" % qport,
           "-netdev", "hubport,id=h1,hubid=0,netdev=s0"]
    q = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    res = []

    def row(k, v):
        res.append((k, v))
        print("%-44s %s" % (k, v), flush=True)

    try:
        for _ in range(50):
            try:
                g = Guest(qport)
                break
            except OSError:
                time.sleep(0.1)
        else:
            print("could not connect:", q.stdout.read(500))
            return 1
        print("QEMU:", subprocess.run([QEMU, "--version"], capture_output=True, text=True).stdout.splitlines()[0])
        arp = struct.pack("!HHBBH6s4s6s4s", 1, 0x0800, 6, 4, 1, GMAC, ME, b"\0" * 6, GW)
        g.send(eth(BCAST, GMAC, 0x0806, arp))
        r = g.drain_until(lambda f: f[12:14] == b"\x08\x06" and f[20:22] == b"\x00\x02", 2.0)
        gwmac = r[22:28]
        sports = iter(range(40000, 40100))

        def conn(dport, dst=GW):
            return Tcp(g, gwmac, next(sports), dport, dst)

        # ---- row 1: option negotiation ----
        stop_ev = []
        data = blob(60000)

        def bulk_server(c):
            c.recv(5)
            c.sendall(data)
            c.shutdown(socket.SHUT_WR)
            try:
                c.recv(1)
            except OSError:
                pass
            c.close()
        hport, stop = serve_once(bulk_server)
        stop_ev.append(stop)
        c = conn(hport)
        syn_opts = b"\x02\x04\x05\xb4\x01\x03\x03\x07\x04\x02\x08\x0a" + struct.pack("!II", 1, 0) + b"\x01"
        sa = c.connect(opts=syn_opts)
        row("SYN-ACK to a SYN with MSS+WS+SACK+TS", "none" if sa is None else
            "options %s window %d ttl %d df %s" % ({k: v.hex() for k, v in sa.opts.items()}, sa.win, sa.ttl, sa.df))
        # ---- row 2: bulk host to guest, acknowledged at once ----
        c.send(PSH | ACK, b"go!!!")
        c.seq += 5
        got, segs, fin = c.read(len(data), timeout=5.0)
        sizes = sorted({len(s.data) for s in segs if s.data})
        row("bulk %d bytes host to guest" % len(data), "%d segments, sizes %s, largest frame %d, fragments %d, hash %s, FIN %s" % (
            len([s for s in segs if s.data]), sizes, max((s.framelen for s in segs), default=0),
            sum(1 for s in segs if s.frag), "ok" if got == data else "MISMATCH (%d bytes)" % len(got), fin))
        row("slirp ACKs guest data", "see the dup-ACK and window rows; delayed ACK: see the next row")
        c.send(FIN | ACK)
        c.seq += 1
        ack = c.wait(lambda s: s.f(ACK) and s.ack == c.seq, 2.0)
        row("FIN from the guest after the host's FIN", "ACKed" if ack else "no ACK")

        # ---- row 3: guest data to the host: when does slirp ACK it? ----
        recv_box = []

        def sink_server(c2):
            b = b""
            c2.settimeout(3)
            try:
                while True:
                    d = c2.recv(65536)
                    if not d:
                        break
                    b += d
            except OSError:
                pass
            recv_box.append(b)
            c2.close()
        sport_, stop = serve_once(sink_server)
        stop_ev.append(stop)
        c = conn(sport_)
        c.connect()
        t0 = time.time()
        gaps = []
        for i in range(10):
            c.send(PSH | ACK, bytes([i]) * 1000)
            c.seq += 1000
            a = c.wait(lambda s: s.f(ACK) and s.ack == c.seq, 1.0)
            gaps.append(None if a is None else round((a.t - t0) * 1000, 1))
            t0 = time.time()
        row("slirp ACK latency per 1000-byte segment (ms)", gaps)
        # three segments after a hole: duplicate ACKs
        base = c.seq
        c.send(PSH | ACK, b"A" * 100, seq=base + 100)
        c.send(PSH | ACK, b"B" * 100, seq=base + 200)
        c.send(PSH | ACK, b"C" * 100, seq=base + 300)
        time.sleep(0.3)
        acks = []
        while True:
            f = g.recv(0.2)
            if f is None:
                break
            s = c.mine(f)
            if s and s.f(ACK) and not s.data:
                acks.append((s.ack - base, "sack" if 5 in s.opts else "nosack"))
        row("three segments after a hole", "ACKs (relative) %s" % acks)
        c.send(PSH | ACK, b"H" * 100, seq=base)
        a = c.wait(lambda s: s.f(ACK) and s.ack != base, 1.0)
        row("filling the hole", "ACK advances to +%s" % (None if a is None else a.ack - base))
        # ---- row: guest RST on an established connection: what the host sees ----
        c.send(RST | ACK, seq=base + 400)
        time.sleep(0.5)
        row("guest RST on an established connection (host server)", "server read %d bytes then EOF/err" % (len(recv_box[0]) if recv_box else -1))

        # ---- row: closed host port ----
        closed = socket.socket()
        closed.bind(("127.0.0.1", 0))
        cp = closed.getsockname()[1]
        closed.close()
        c = conn(cp)
        r = c.connect()
        row("SYN to a closed host port", "none" if r is None else "RST flags %02x seq %d ack %d (ours %d)" % (r.flags, r.seq, r.ack, c.seq))
        # ---- row: stray ACK with data ----
        c = conn(cp + 1)
        c.rcv = 5000
        c.send(PSH | ACK, b"stray", seq=77777)
        r = c.wait(lambda s: s.f(RST), 1.5)
        row("stray ACK+data for no connection", "none" if r is None else "RST flags %02x seq %d ack %d" % (r.flags, r.seq, r.ack))
        # ---- row: small MSS and window, no ACKs: retransmission schedule ----
        def push_server(c2):
            c2.sendall(blob(20000))
            time.sleep(12)
            c2.close()
        pp, stop = serve_once(push_server)
        stop_ev.append(stop)
        c = conn(pp)
        sa = c.connect(opts=b"\x02\x04\x02\x18", win=4096)
        first = c.wait(lambda s: s.data, 2.0)
        times = []
        if first:
            t_first = first.t
            seen_first = []
            end = time.time() + 11
            sizes = [len(first.data)]
            uniq = {first.seq: len(first.data)}
            while time.time() < end:
                s = c.wait(lambda s: True, max(0.05, end - time.time()))
                if s is None:
                    break
                if s.data:
                    if s.seq == first.seq:
                        seen_first.append(round(s.t - t_first, 2))
                    sizes.append(len(s.data))
                    uniq[s.seq] = len(s.data)
            total = sum(uniq.values())
            row("MSS 536 window 4096, never ACKed", "segment sizes seen %s; distinct bytes sent before it stops %d; retransmits of the first at %s s" % (
                sorted(set(sizes)), total, seen_first))
        else:
            row("MSS 536 window 4096, never ACKed", "no data")
        c.send(RST)
        # ---- row: zero window from the guest ----
        zw = blob(8000)

        def zw_server(c2):
            c2.sendall(zw)
            time.sleep(10)
            c2.close()
        zp, stop = serve_once(zw_server)
        stop_ev.append(stop)
        c = conn(zp)
        c.connect(win=0)
        time.sleep(0.5)
        quiet = []
        end = time.time() + 5
        while time.time() < end:
            f = g.recv(0.2)
            s = c.mine(f) if f else None
            if s and s.data:
                quiet.append((round(time.time() - end + 5, 2), len(s.data)))
        row("guest announces window 0 from the start", "data segments seen at (s, bytes): %s" % quiet)
        c.win = 8000
        c.send(ACK)
        got, segs, _ = c.read(len(zw), timeout=3.0)
        row("... then opens the window", "%d bytes arrive after the update" % len(got))
        c.send(RST)

        # ---- row: an IP-fragmented TCP segment from the guest ----
        recv2 = []

        def sink2(c2):
            c2.settimeout(2)
            b = b""
            try:
                while True:
                    d = c2.recv(65536)
                    if not d:
                        break
                    b += d
            except OSError:
                pass
            recv2.append(len(b))
            c2.close()
        fp, stop = serve_once(sink2)
        stop_ev.append(stop)
        c = conn(fp)
        c.connect()
        payload = b"F" * 2900
        seg = tcp_seg(ME, GW, c.sp, c.dp, c.seq, c.rcv, PSH | ACK, 65535, b"", payload)
        # two fragments: the first carries 1480 bytes of the TCP segment (a multiple of 8), the second the rest
        for off, part, more in ((0, seg[:1480], 1), (1480, seg[1480:], 0)):
            h = struct.pack("!BBHHHBBH4s4s", 0x45, 0, 20 + len(part), 4242, (more << 13) | (off // 8), 64, 6, 0, ME, GW)
            h = h[:10] + struct.pack("!H", csum(h)) + h[12:]
            g.send(eth(gwmac, GMAC, 0x0800, h + part))
        a = c.wait(lambda s: s.f(ACK) and s.ack == c.seq + len(payload), 2.0)
        row("guest sends a 2920-byte TCP datagram as 2 fragments", "reassembled and ACKed" if a else "no ACK (dropped, or not reassembled)")
        c.send(RST, seq=c.seq + len(payload))

        # ---- lossy variant: the guest drops chosen data segments; slirp must retransmit ----
        lossy = blob(30000)

        def lossy_server(c2):
            c2.sendall(lossy)
            c2.shutdown(socket.SHUT_WR)
            time.sleep(1)
            c2.close()
        lp, stop = serve_once(lossy_server)
        stop_ev.append(stop)
        c = conn(lp)
        c.connect()
        t0 = time.time()
        got, segs, fin = c.read(len(lossy), timeout=12.0, drop=lambda n: n in (3, 4, 11))
        row("lossy: the guest drops segments 3, 4 and 11", "%d of %d bytes in %.1f s, hash %s; slirp resent %d segments" % (
            len(got), len(lossy), time.time() - t0, "ok" if got == lossy[:len(got)] else "MISMATCH",
            len([s for s in segs if s.data]) - len({s.seq for s in segs if s.data})))
        c.send(RST)

        # ---- host-initiated through hostfwd ----
        hostc = {}

        def host_connect():
            t = time.time()
            s = socket.create_connection(("127.0.0.1", fport), timeout=15)
            hostc["connected_after"] = time.time() - t
            hostc["sock"] = s
        th = threading.Thread(target=host_connect, daemon=True)
        t_start = time.time()
        th.start()
        syn = None
        end = time.time() + 3
        while time.time() < end and syn is None:
            f = g.recv(0.2)
            s = Seg(f) if f else None
            if s and s.ok and s.f(SYN) and not s.f(ACK):
                syn = s
        if syn:
            row("host connect through hostfwd: the SYN the guest sees", "from %s port %d, to port %d, options %s, window %d, mac %s" % (
                socket.inet_ntoa(syn.src), syn.sp, syn.dp, {k: v.hex() for k, v in syn.opts.items()}, syn.win, "to our MAC"))
            time.sleep(0.5)
            row("the host's connect() before the guest answered", "returned after %s s" % (round(hostc["connected_after"], 2) if "connected_after" in hostc else "not yet"))
            c = Tcp(g, gwmac, syn.dp, syn.sp, dst=syn.src)
            c.seq, c.rcv = 5000, syn.seq + 1
            # the guest answers with RST: what does the host see?
            c.send(RST | ACK, ack=syn.seq + 1, seq=0)
            hs = hostc.get("sock")
            seen = "no socket"
            if hs:
                hs.settimeout(2)
                try:
                    d = hs.recv(10)
                    seen = "EOF" if d == b"" else "data"
                except socket.timeout:
                    seen = "nothing within 2 s"
                except OSError as e:
                    seen = "error %s" % e
                hs.close()
            row("the guest answers that SYN with RST: the host reads", seen)
        else:
            row("host connect through hostfwd", "the guest saw no SYN")
        th.join(1)

        quick_log = list(g.log)          # the capture holds the quick rows only: the long ones would make it huge
        if long:
            # the unanswered inbound SYN: slirp's retry timing
            hostc.clear()
            th = threading.Thread(target=host_connect, daemon=True)
            th.start()
            times = []
            end = time.time() + 9
            t0 = None
            while time.time() < end:
                f = g.recv(0.2)
                s = Seg(f) if f else None
                if s and s.ok and s.f(SYN) and not s.f(ACK):
                    t0 = t0 or time.time()
                    times.append(round(time.time() - t0, 1))
            row("an unanswered inbound SYN: sent at (s)", times)
            # a 685 KB stream the host server does not read
            gate = threading.Event()

            def deaf(c2):
                gate.wait(20)
                c2.close()
            dp_, stop = serve_once(deaf)
            stop_ev.append(stop)
            c = conn(dp_)
            c.connect()
            sent = 0
            end = time.time() + 12
            win = None
            while time.time() < end and sent < 2_000_000:
                c.send(PSH | ACK, b"D" * 1400)
                c.seq += 1400
                sent += 1400
                a = c.wait(lambda s: s.f(ACK), 0.05)
                if a:
                    win = a.win
                    if a.win == 0:
                        break
            row("guest streams to a host server that does not read", "slirp advertised window %s after %d bytes sent" % (win, sent))
            gate.set()
            c.send(RST)

        for s in stop_ev:
            s.set()
        with open(out, "wb") as fh:
            fh.write(struct.pack("<IHHiIII", 0xa1b2c3d4, 2, 4, 0, 0, 65535, 1))
            for t, fr in quick_log:
                fh.write(struct.pack("<IIII", int(t), int((t % 1) * 1e6), len(fr), len(fr)) + fr)
        print("wrote %s: %d frames, %d bytes" % (out, len(quick_log), os.path.getsize(out)))
        return 0
    finally:
        q.terminate()


if __name__ == "__main__":
    sys.exit(main())

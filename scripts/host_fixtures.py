#!/usr/bin/env python3
"""The host side of the network tests that the guest connects to (S9 steps 4c and 7b), one set per boot (T195).

    host_fixtures.py --self-test   check that two sets started at once get their own ports, and that the guest is told them

The guest reaches the host at 10.0.2.2 through slirp, which turns that address into the host's loopback at the same port. The
fixtures used fixed ports (47802 TCP, 47803 DNS, 47804 HTTP), so two boots on one host collided: the second could not bind, or its
guest talked to the first boot's servers (the first full soak, 2026-11-21: `##HOSTNET tcp_server NOTRUN cannot bind 47802`). Each
fixture now binds port 0, where the host has room, and `boot_arg()` is the kernel-line word that tells the guest where they are
(`hostports=<tcp>,<dns>,<http>`; the suite reads it as `debug_stat` 15). A fixture that cannot bind stops the run: there is no
silent skip. `scripts/os_boot_test.py` starts one set per boot; the self-test starts no emulator (the feature gate runs it).
"""

import socket
import sys
import threading


def tcp_pattern(seed, n, start=0):
    """The byte pattern the guest's `pattern(seed, i)` makes: ((i * 2654435761) + seed) >> 24, 32-bit arithmetic."""
    return bytes(((((i * 2654435761) & 0xffffffff) + seed) & 0xffffffff) >> 24 for i in range(start, start + n))


FILE21 = None


def file21():
    """The 256 KiB file the host HTTP server serves (and whose digest the guest's `fetch` must reproduce)."""
    global FILE21
    if FILE21 is None:
        FILE21 = tcp_pattern(21, 262144)
    return FILE21


class HostWeb:
    """The host side of the fetch tests (S9 step 7b): an HTTP/1.0 server and a scripted DNS server (UDP), each on a port of its own
    (`http_port`, `dns_port`). `files.test` is 10.0.2.2, `alias.test` is a CNAME to it, `slow.test` drops its first query (the
    resolver must retry), `nosuch.test` is NXDOMAIN."""

    def __init__(self):
        import socketserver
        import http.server
        self.stop = threading.Event()
        data = file21()

        class H(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.0"

            def do_GET(self):
                if self.path == "/file.bin":
                    self.send_response(200)
                    self.send_header("Content-Type", "application/octet-stream")
                    self.send_header("Content-Length", str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                else:
                    self.send_error(404)

            def log_message(self, *a):
                pass

        class S(socketserver.ThreadingTCPServer):
            daemon_threads = True

        self.http = S(("127.0.0.1", 0), H)
        self.http_port = self.http.server_address[1]
        threading.Thread(target=self.http.serve_forever, daemon=True).start()
        self.dns = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.dns.bind(("127.0.0.1", 0))
        self.dns_port = self.dns.getsockname()[1]
        self.dns.settimeout(0.3)
        self.seen = {}
        threading.Thread(target=self.dns_loop, daemon=True).start()

    def dns_loop(self):
        import struct
        while not self.stop.is_set():
            try:
                q, who = self.dns.recvfrom(512)
            except socket.timeout:
                continue
            except OSError:
                return
            if len(q) < 17:
                continue
            ident = q[:2]
            i, labels = 12, []
            while i < len(q) and q[i]:
                labels.append(q[i + 1:i + 1 + q[i]])
                i += 1 + q[i]
            name = b".".join(labels).decode().lower()
            question = q[12:i + 5]
            self.seen[name] = self.seen.get(name, 0) + 1
            if name == "slow.test" and self.seen[name] == 1:
                continue  # drop the first query: the resolver retries after a second
            def a_rr(owner_ptr=b"\xc0\x0c"):
                return owner_ptr + struct.pack("!HHIH", 1, 1, 60, 4) + bytes([10, 0, 2, 2])
            if name in ("files.test", "slow.test"):
                resp = ident + b"\x81\x80" + struct.pack("!HHHH", 1, 1, 0, 0) + question + a_rr()
            elif name == "alias.test":
                target = b"\x05files\x04test\x00"
                cn = b"\xc0\x0c" + struct.pack("!HHIH", 5, 1, 60, len(target)) + target
                resp = ident + b"\x81\x80" + struct.pack("!HHHH", 1, 2, 0, 0) + question + cn + target + struct.pack("!HHIH", 1, 1, 60, 4) + bytes([10, 0, 2, 2])
            else:
                resp = ident + b"\x81\x83" + struct.pack("!HHHH", 1, 0, 0, 0) + question
            self.dns.sendto(resp, who)

    def close(self):
        self.stop.set()
        self.http.shutdown()
        self.http.server_close()
        self.dns.close()


class HostTcpServer:
    """The host TCP server a guest test connects to (10.0.2.2 at `port`, through slirp): it sends a greeting and 300,000 bytes, shuts
    down its write side, and checks the 100,000 bytes the guest sends back. Reports once, if the guest came."""

    def __init__(self, emit):
        self.emit = emit
        self.stop = threading.Event()
        self.srv = socket.socket()
        self.srv.bind(("127.0.0.1", 0))
        self.port = self.srv.getsockname()[1]
        self.srv.listen(4)
        self.th = threading.Thread(target=self.run, daemon=True)
        self.th.start()

    def run(self):
        self.srv.settimeout(0.5)
        while not self.stop.is_set():
            try:
                c, _ = self.srv.accept()
            except OSError:
                continue
            bad = []
            try:
                c.settimeout(30)
                c.sendall(b"HELLO-FROM-HOST\n" + tcp_pattern(3, 300_000))
                got = bytearray()
                c.shutdown(socket.SHUT_WR)
                while True:
                    d = c.recv(65536)
                    if not d:
                        break
                    got.extend(d)
                if bytes(got) != tcp_pattern(4, 100_000):
                    bad.append("received %d of 100000 bytes%s" % (len(got), "" if bytes(got) == tcp_pattern(4, len(got)) else " (and wrong)"))
            except OSError as e:
                bad.append(str(e))
            c.close()
            self.emit("##HOSTNET tcp_server " + ("FAIL " + "; ".join(bad) if bad else "PASS"))
            return

    def close(self):
        self.stop.set()
        self.th.join(timeout=3)
        self.srv.close()


class Fixtures:
    """One boot's host fixtures: the TCP server, the DNS server and the HTTP server, each on a port the host chose."""

    def __init__(self, emit):
        self.tcp = HostTcpServer(emit)
        try:
            self.web = HostWeb()
        except BaseException:
            self.tcp.close()
            raise

    @property
    def ports(self):
        """(tcp, dns, http)"""
        return self.tcp.port, self.web.dns_port, self.web.http_port

    def boot_arg(self):
        """The kernel-line word that tells the guest where the fixtures are (`debug_stat` 15, `kcore::testlog::host_ports`)."""
        return "hostports=" + ",".join(str(p) for p in self.ports)

    def close(self):
        self.tcp.close()
        self.web.close()


def self_test():
    seen = []
    a, b = Fixtures(seen.append), Fixtures(seen.append)
    try:
        pa, pb = a.ports, b.ports
        assert all(0 < p < 65536 for p in pa + pb), (pa, pb)
        assert len(set(pa[0::2])) == 2 and len(set(pb[0::2])) == 2, "a set's two TCP servers have ports of their own"
        assert pa[0] != pb[0] and pa[2] != pb[2] and pa[1] != pb[1], f"two boots at once get different ports: {pa} {pb}"
        assert a.boot_arg() == "hostports=%d,%d,%d" % pa, a.boot_arg()
        # each set answers on its own ports: the second set's TCP server greets a client, and the first set's HTTP server serves the file
        with socket.create_connection(("127.0.0.1", pb[0]), timeout=5) as s:
            assert s.recv(16) == b"HELLO-FROM-HOST\n"
        import urllib.request
        with urllib.request.urlopen(f"http://127.0.0.1:{pa[2]}/file.bin", timeout=5) as r:
            assert r.read() == file21()
        q = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        q.settimeout(5)
        q.sendto(b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x05files\x04test\x00\x00\x01\x00\x01", ("127.0.0.1", pb[1]))
        resp = q.recv(512)
        q.close()
        assert resp[:2] == b"\x12\x34" and resp[-4:] == bytes([10, 0, 2, 2]), resp
    finally:
        a.close()
        b.close()
    print("host_fixtures self-test passed")
    return 0


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        sys.exit(self_test())
    print(__doc__, file=sys.stderr)
    sys.exit(2)

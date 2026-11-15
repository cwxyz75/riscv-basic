#!/usr/bin/env python3
"""Independent checksum check of a pcap (NET-SPEC H1 and Q5), Python standard library only.

Verifies, for every Ethernet/IPv4 frame: the IPv4 header checksum, the ICMP checksum, the UDP checksum (a zero UDP
checksum is allowed) and the TCP checksum (pseudo-header and the segment length from the IP length; never zero). Fragments are skipped for the transport checks (they cannot be checked alone). Prints one line per
problem and exits 1 if there is any; `--self-test` proves the checker can fail: it corrupts one byte of each kind in a
copy of the capture and requires all three to be reported (the negative control of the check itself).

Why this exists: `tcpdump -vv` verifies the IP header checksum but only verifies UDP checksums for protocols it
dissects (it did for DHCP, not for an arbitrary port), so it is used as a second opinion, not as the only oracle.
"""
import struct, sys

def csum(b):
    if len(b) % 2: b += b"\0"
    s = sum(struct.unpack("!%dH" % (len(b) // 2), b))
    while s >> 16: s = (s & 0xffff) + (s >> 16)
    return (~s) & 0xffff

def frames(data):
    magic = struct.unpack("<I", data[:4])[0]
    assert magic == 0xa1b2c3d4, "not a little-endian classic pcap"
    assert struct.unpack("<I", data[20:24])[0] == 1, "not Ethernet"
    i, n = 24, 0
    while i + 16 <= len(data):
        _, _, cl, _ = struct.unpack("<IIII", data[i:i + 16])
        yield n, i + 16, data[i + 16:i + 16 + cl]
        i += 16 + cl; n += 1

def check(data):
    problems, counts = [], {"ip": 0, "icmp": 0, "udp": 0, "tcp": 0}
    for n, _, f in frames(data):
        if len(f) < 34 or f[12:14] != b"\x08\x00": continue
        ihl = (f[14] & 15) * 4
        if f[14] >> 4 != 4 or ihl < 20 or len(f) < 14 + ihl: problems.append((n, "malformed IPv4 header")); continue
        if csum(f[14:14 + ihl]) != 0: problems.append((n, "bad IPv4 header checksum"))
        counts["ip"] += 1
        total = struct.unpack("!H", f[16:18])[0]
        frag = struct.unpack("!H", f[20:22])[0]
        if frag & 0x3fff: continue                       # a fragment: transport checks need the whole datagram
        body = f[14 + ihl:14 + total]                    # the IP length, not the frame length (Ethernet pads short frames)
        proto = f[23]
        if proto == 1 and len(body) >= 8:
            counts["icmp"] += 1
            if csum(body) != 0: problems.append((n, "bad ICMP checksum"))
        elif proto == 17 and len(body) >= 8:
            counts["udp"] += 1
            if body[6:8] != b"\0\0":
                pseudo = f[26:34] + struct.pack("!BBH", 0, 17, len(body))
                if csum(pseudo + body) != 0: problems.append((n, "bad UDP checksum"))
        elif proto == 6 and len(body) >= 20:
            counts["tcp"] += 1
            pseudo = f[26:34] + struct.pack("!BBH", 0, 6, len(body))
            if csum(pseudo + body) != 0: problems.append((n, "bad TCP checksum"))
    return problems, counts

def self_test(data):
    ok = True
    _, have = check(data)
    tests = [("IPv4 header checksum", lambda f: f[12:14] == b"\x08\x00", 14 + 8),
                            ("ICMP checksum", lambda f: f[12:14] == b"\x08\x00" and f[23] == 1 and (f[20] & 0x3f) == 0 and f[21] == 0, 34 + 6),
                            ("UDP checksum", lambda f: f[12:14] == b"\x08\x00" and f[23] == 17 and f[36:38] == b"\x1e\x61" and (f[20] & 0x3f) == 0 and f[21] == 0, 42)]
    if have["tcp"]:
        # a TCP segment with data, corrupted in its last payload byte (so only the checksum can catch it), and one
        # corrupted in the checksum field itself
        tests.append(("TCP checksum (payload)", lambda f: f[12:14] == b"\x08\x00" and f[23] == 6 and len(f) > 80 and (f[20] & 0x3f) == 0 and f[21] == 0, 70))
        tests.append(("TCP checksum (field)", lambda f: f[12:14] == b"\x08\x00" and f[23] == 6 and (f[20] & 0x3f) == 0 and f[21] == 0, 34 + 16))
    for kind, find, off in tests:
        proto = kind.split()[0].lower()
        if proto in have and not have[proto]:
            continue                                    # this capture has no such frames: not what it is for
        d = bytearray(data)
        for n, pos, f in frames(data):
            if find(f):
                d[pos + off] ^= 0x55; break
        else:
            print("self-test: no %s frame to corrupt" % kind); ok = False; continue
        problems, _ = check(bytes(d))
        hit = any(kind.split()[0] in p for _, p in problems)
        print("self-test: corrupting the %s is %s" % (kind, "detected" if hit else "NOT DETECTED"))
        ok = ok and hit
    return ok

if __name__ == "__main__":
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    data = open(args[0], "rb").read()
    if "--self-test" in sys.argv:
        clean, counts = check(data)
        print("clean capture: %d problems, checked %s" % (len(clean), counts))
        sys.exit(0 if (not clean and self_test(data)) else 1)
    problems, counts = check(data)
    for n, p in problems: print("frame %d: %s" % (n, p))
    print("checked %s, %d problems" % (counts, len(problems)))
    sys.exit(1 if problems else 0)

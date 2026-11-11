#!/usr/bin/env python3
"""Make and inspect disk images in the cartfs format (DESIGN.md, "The disk").

An independent implementation of the layout, written from the spec, so the
kernel's disk code (the Rust `cartfs` crate) is checked against something that
does not share its code.

    disk.py new IMAGE [SECTORS]      write a freshly formatted image (default 4096 sectors)
    disk.py ls IMAGE                 list the files: NAME KIND SIZE
    disk.py cat IMAGE NAME           print a file's text
    disk.py put IMAGE KIND NAME TEXT put a file (KIND basic or logo; TEXT is the contents)

As a module: `new_image`, `read_files`, `read_file`, `put_file`.
"""
import struct
import sys

SECTOR = 512
FIRST_DIR, DIR_SECTORS, ENTRIES = 2, 4, 64
FIRST_DATA = FIRST_DIR + DIR_SECTORS
PAYLOAD = SECTOR - 2
KINDS = {1: "BASIC", 2: "LOGO"}


def new_image(path: str, sectors: int = 4096) -> None:
    img = bytearray(sectors * SECTOR)
    img[0:8] = b"RVDISK01"
    struct.pack_into("<II", img, 8, sectors, ENTRIES)
    used = list(range(FIRST_DATA)) + list(range(sectors, 4096))
    for s in used:
        img[SECTOR + s // 8] |= 1 << (s % 8)
    with open(path, "wb") as f:
        f.write(img)


def _entries(img: bytes):
    for i in range(ENTRIES):
        at = FIRST_DIR * SECTOR + i * 32
        e = img[at : at + 32]
        if e[16] == 0:
            continue
        name = e[:16].split(b"\0")[0].decode()
        first, length = struct.unpack_from("<HI", e, 18)
        yield i, name, e[16], first, length


def _chain(img: bytes, first: int, length: int) -> bytes:
    out, s, seen = b"", first, set()
    while True:
        assert FIRST_DATA <= s < len(img) // SECTOR and s not in seen, f"bad chain at {s}"
        seen.add(s)
        base = s * SECTOR
        nxt = struct.unpack_from("<H", img, base)[0]
        out += img[base + 2 : base + SECTOR]
        if nxt == 0:
            break
        s = nxt
    assert len(seen) == max(1, -(-length // PAYLOAD)), "chain length does not match size"
    return out[:length]


def read_files(path: str) -> dict:
    """{NAME: (KIND, text)} for every file; checks the chains and the free map."""
    img = open(path, "rb").read()
    assert img[:8] == b"RVDISK01", "not a disk image"
    total = struct.unpack_from("<I", img, 8)[0]
    files, used = {}, set(range(FIRST_DATA))
    for _, name, kind, first, length in _entries(img):
        data = _chain(img, first, length)
        s = first
        while s:
            assert s not in used, f"sector {s} used twice"
            used.add(s)
            s = struct.unpack_from("<H", img, s * SECTOR)[0]
        files[name] = (KINDS[kind], data.decode())
    for s in range(total):
        bit = img[SECTOR + s // 8] >> (s % 8) & 1
        assert bit == (s in used), f"free map disagrees with the files at sector {s}"
    return files


def read_file(path: str, name: str) -> str:
    return read_files(path)[name][1]


def put_file(path: str, kind: str, name: str, text: str) -> None:
    """Add a new file (the name must not exist), first-fit like the kernel."""
    img = bytearray(open(path, "rb").read())
    total = struct.unpack_from("<I", img, 8)[0]
    data = text.encode()
    need = max(1, -(-len(data) // PAYLOAD))
    free = [s for s in range(FIRST_DATA, total) if not img[SECTOR + s // 8] >> (s % 8) & 1][:need]
    assert len(free) == need, "disk full"
    slot = next(i for i in range(ENTRIES) if img[FIRST_DIR * SECTOR + i * 32 + 16] == 0)
    for n, s in enumerate(free):
        chunk = data[n * PAYLOAD : (n + 1) * PAYLOAD]
        nxt = free[n + 1] if n + 1 < need else 0
        img[s * SECTOR : (s + 1) * SECTOR] = struct.pack("<H", nxt) + chunk.ljust(PAYLOAD, b"\0")
        img[SECTOR + s // 8] |= 1 << (s % 8)
    e = name.upper().encode().ljust(16, b"\0") + bytes([{"basic": 1, "logo": 2}[kind.lower()], 0])
    e += struct.pack("<HI", free[0], len(data)) + bytes(8)
    at = FIRST_DIR * SECTOR + slot * 32
    img[at : at + 32] = e
    open(path, "wb").write(img)


def main() -> int:
    a = sys.argv[1:]
    if len(a) >= 2 and a[0] == "new":
        new_image(a[1], int(a[2]) if len(a) > 2 else 4096)
    elif len(a) == 2 and a[0] == "ls":
        for name, (kind, text) in sorted(read_files(a[1]).items()):
            print(f"{name:<16}  {kind:<5}  {len(text):>6}")
    elif len(a) == 3 and a[0] == "cat":
        sys.stdout.write(read_file(a[1], a[2].upper()))
    elif len(a) == 5 and a[0] == "put":
        put_file(a[1], a[2], a[3], a[4])
    else:
        print(__doc__, file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Measure a release kernel (T54): image size, host time to the shell prompt, and the guest's own clock
for a few Lua workloads (the guest clock runs on QEMU's instruction counter with `-icount`, so the
numbers do not depend on how busy the host is).

Usage: bench_release.py KERNEL_ELF
"""
import os
import subprocess
import sys
import time

WORK = {
    "fib": "local function f(n) if n < 2 then return n end return f(n-1) + f(n-2) end f(28)",
    "sort": "local t = {} for i = 1, 200000 do t[i] = (i * 7919) % 100003 end table.sort(t)",
    "string": "local s = 0 for i = 1, 200000 do s = s + #(tostring(i) .. string.rep('x', i % 7)) end",
    "syscalls": "io.stdout:setvbuf('no') for i = 1, 30000 do io.write(' ') end io.write('\\n')",
    "alloc": "local x = {} for i = 1, 100000 do x[i] = {i, tostring(i)} end",
}


def main():
    kernel = sys.argv[1]
    size = os.path.getsize(kernel)
    cmd = ["qemu-system-riscv64", "-machine", "virt", "-bios", "default", "-m", "256M", "-display", "none",
           "-serial", "stdio", "-monitor", "none", "-icount", "shift=0,sleep=off", "-kernel", kernel]
    if os.environ.get("OS_CPU"):
        cmd[1:1] = ["-cpu", os.environ["OS_CPU"]]
    t0 = time.time()
    p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)

    def read_until(marker, limit=300):
        buf = b""
        deadline = time.time() + limit
        while marker not in buf:
            if time.time() > deadline:
                raise SystemExit(f"timed out waiting for {marker!r}; got {buf[-200:]!r}")
            c = p.stdout.read(1)
            if not c:
                raise SystemExit(f"QEMU ended; got {buf[-200:]!r}")
            buf += c
        return buf

    read_until(b"riscv-os S4 ready")
    read_until(b"$ ")
    boot = time.time() - t0
    out = {"image bytes": size, "host s to prompt": round(boot, 1)}
    for name, code in WORK.items():
        line = f"lua -e \"local c = os.clock() {code} print('BENCH', os.clock() - c)\"\n"
        p.stdin.write(line.encode())
        p.stdin.flush()
        buf = read_until(b"$ ")
        for l in buf.split(b"\n"):
            if b"BENCH" in l and b"print" not in l:
                out[f"lua {name} (guest s)"] = round(float(l.split()[-1]), 3)
    p.kill()
    for k, v in out.items():
        print(f"{k}: {v}")


if __name__ == "__main__":
    main()

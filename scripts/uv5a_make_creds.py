#!/usr/bin/env python3
"""Writes the seed credential store of the UV5a test image (tests/uv5a/seed/etc/credentials) with Python's hashlib.scrypt, so
the hashes the OS checks were made by an implementation that is not ours. Test cost parameters (N=16, r=8, p=1); the
passwords are fixed test values and the file is good for nothing else."""
import hashlib, os, sys

USERS = {"alice": ("alice-pass", bytes([0xA1] * 16)), "bob": ("bob-pass", bytes([0xB2] * 16))}
out = []
for name, (pw, salt) in USERS.items():
    h = hashlib.scrypt(pw.encode(), salt=salt, n=16, r=8, p=1, dklen=32)
    out.append(f"{name} scrypt$4$8$1${salt.hex()}${h.hex()}\n")
path = os.path.join(os.path.dirname(__file__), "..", "tests", "uv5a", "seed", "etc", "credentials")
open(path, "w").write("".join(out))
print("wrote", os.path.normpath(path))

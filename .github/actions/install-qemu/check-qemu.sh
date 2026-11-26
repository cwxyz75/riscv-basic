#!/usr/bin/env bash
# CI's QEMU check (TEST-TOOLS.md, "CI's QEMU"): the qemu-system-riscv64 on the PATH must be the one this action installed, at the
# version build-qemu.sh pins. Fails loudly otherwise, so a cache miss or a distribution package can never run the boots on another QEMU.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
want=$(sed -n 's/^QEMU_VERSION=//p' "$here/build-qemu.sh")
bin="$HOME/qemu/bin/qemu-system-riscv64"
found=$(command -v qemu-system-riscv64 || true)
if [ "$found" != "$bin" ]; then
  echo "::error::qemu-system-riscv64 is '${found:-not on the PATH}', not CI's QEMU $want at $bin"
  exit 1
fi
got=$(qemu-system-riscv64 --version | head -1)
echo "$got"
if [ "$got" != "QEMU emulator version $want" ]; then
  echo "::error::CI's QEMU must be $want; found '$got'"
  exit 1
fi
# what the boots need of the machine: the AIA, the platform IOMMU, the profile CPU, the user network backend
qemu-system-riscv64 -machine virt,help | grep -E "^  (aia|iommu-sys)="
qemu-system-riscv64 -cpu help | grep -w rva23s64 >/dev/null || { echo "::error::this QEMU has no rva23s64 CPU"; exit 1; }
qemu-system-riscv64 -machine none -netdev help | grep -x user >/dev/null || { echo "::error::this QEMU has no user network backend (slirp)"; exit 1; }

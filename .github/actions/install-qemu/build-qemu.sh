#!/usr/bin/env bash
# CI's QEMU (TEST-TOOLS.md, "CI's QEMU"): build the release named below from its tarball into the prefix given as $1.
# The tarball is checked twice before anything in it runs: its sha256 (pinned here; the same value as Homebrew's formula for
# this release) and its signature by the QEMU release key (the public key is the file beside this one, and its fingerprint is
# pinned here). The cache key hashes this file, so a change to the version or to the configuration below is a new cache entry.
# The build needs: ninja-build pkg-config python3-venv libglib2.0-dev libpixman-1-dev libslirp-dev libfdt-dev zlib1g-dev gnupg.
set -euo pipefail

QEMU_VERSION=11.1.3
QEMU_SHA256=5a8289c7940dc77d3873c81a6c29ba3afe1446fc9d36e77fa1658cdbecb84b22
QEMU_KEY_FPR=CEACC9E15534EBABB82D3FA03353C9CEF108B584 # Michael Roth, the QEMU release key

prefix=${1:?usage: build-qemu.sh <prefix>}
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
cd "$work"

url=https://download.qemu.org/qemu-$QEMU_VERSION.tar.xz
curl -sSfL --retry 3 --retry-all-errors -o qemu.tar.xz "$url"
curl -sSfL --retry 3 --retry-all-errors -o qemu.tar.xz.sig "$url.sig"
echo "$QEMU_SHA256  qemu.tar.xz" | sha256sum -c -
export GNUPGHOME="$work/gnupg"
mkdir -m 700 "$GNUPGHOME"
gpg --batch --quiet --import "$here/qemu-release-key.asc"
gpg --batch --status-fd 1 --verify qemu.tar.xz.sig qemu.tar.xz >verify.txt 2>&1 || { cat verify.txt; exit 1; }
# VALIDSIG <signing key fpr> ... <primary key fpr>: the signature is good and was made by the pinned key
if ! grep -Eq "^\[GNUPG:\] VALIDSIG .*\b$QEMU_KEY_FPR\$" verify.txt; then
  cat verify.txt
  echo "::error::qemu-$QEMU_VERSION.tar.xz is not signed by $QEMU_KEY_FPR"
  exit 1
fi
echo "qemu-$QEMU_VERSION.tar.xz: sha256 and signature (key $QEMU_KEY_FPR) good"

tar -xJf qemu.tar.xz
cd "qemu-$QEMU_VERSION"
# One target, and only what the boots use: TCG (with -icount and record/replay), the virt machine with AIA and the IOMMU, the virtio
# devices, the slirp network backend (`-netdev user`, hostfwd), pixman (the home computer's ramfb and `screendump`), the system libfdt
# (`-machine dumpdtb`, `-dtb`), and the bundled firmware blobs (`-bios default` is the OpenSBI that ships with this release, as on a
# developer's machine). No GUI, no VNC, no docs, no tools, no guest agent, no Rust.
./configure \
  --prefix="$prefix" \
  --target-list=riscv64-softmmu \
  --without-default-features \
  --enable-tcg \
  --enable-slirp \
  --enable-pixman \
  --enable-fdt=system \
  --disable-docs \
  --disable-debug-info \
  --disable-werror \
  --enable-strip
make -j"$(nproc)"
make install
"$prefix/bin/qemu-system-riscv64" --version | head -1
rm -rf "$work"

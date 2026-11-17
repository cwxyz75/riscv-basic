#!/bin/bash
# Fetch third-party sources that are NOT vendored into third_party/fetched/<name>/ (ignored by git).
#
# Reads third_party/fetch-manifest.txt: one source per line, "name url file sha256" (a sha256 of "-" means
# "not recorded yet": the script prints the computed one so it can be pinned). Downloads are never committed.
# Each archive is verified against the manifest, then extracted into <name>/src (archives are only
# unpacked here; nothing from them is built or run by this script). FETCH_ONLY="busybox lua" limits it to those names (CI
# fetches only what its tests build).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
manifest="$root/third_party/fetch-manifest.txt"
dest="$root/third_party/fetched"

[ -f "$manifest" ] || { echo "fetch: no manifest at $manifest" >&2; exit 1; }
mkdir -p "$dest"

while read -r name url file sha; do
  case "$name" in ''|'#'*) continue ;; esac
  if [ -n "${FETCH_ONLY:-}" ] && ! printf ' %s ' "$FETCH_ONLY" | grep -q " $name "; then continue; fi
  dir="$dest/$name"
  mkdir -p "$dir"
  f="$dir/$file"
  if [ ! -f "$f" ]; then
    echo "fetch: $name <- $url"
    curl -fsSL --retry 2 --max-time 300 -o "$f.part" "$url"
    mv "$f.part" "$f"
  fi
  got="$(shasum -a 256 "$f" | awk '{print $1}')"
  if [ "$sha" = "-" ]; then
    echo "fetch: $name $file sha256 $got (not pinned in the manifest yet)"
  elif [ "$sha" != "$got" ]; then
    echo "fetch: CHECKSUM MISMATCH for $name $file: expected $sha, got $got" >&2
    rm -f "$f"
    exit 1
  else
    echo "fetch: $name $file ok"
  fi
  if [ ! -d "$dir/src" ]; then
    mkdir -p "$dir/src"
    case "$file" in
      *.tar.gz|*.tar.bz2|*.tar.xz) tar -xf "$f" -C "$dir/src" --strip-components=1 ;;
      *.zip) unzip -q "$f" -d "$dir/src" ;;
      *) cp "$f" "$dir/src/$file" ;;
    esac
  fi
done < "$manifest"

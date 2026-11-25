#!/usr/bin/env bash
# Puts the BCPack format's constant tables in place (they are not stored in
# this repository, see docs/asset-policy.md) and checks them.
set -euo pipefail
cd "$(dirname "$0")/.."
OUT=crates/victory-formats/src/bcpack-tables.bin
SHA=1f1d4760419202bb5672f0c428ce51e456b5d08fbaec8f371d043fd94e01c36b
URL=${BCPACK_TABLES_URL:-https://raw.githubusercontent.com/Marvisak/VIctory/main/crates/victory-formats/src/bcpack-tables.bin}
check() { [ -f "$OUT" ] && [ "$( (sha256sum "$OUT" 2>/dev/null || shasum -a 256 "$OUT") | cut -d' ' -f1)" = "$SHA" ]; }
if check; then echo "BCPack tables: present"; exit 0; fi
curl -fsSL -o "$OUT.part" "$URL"
mv "$OUT.part" "$OUT"
check || { rm -f "$OUT"; echo "BCPack tables: checksum mismatch from $URL" >&2; exit 1; }
echo "BCPack tables: downloaded and checked"

#!/usr/bin/env bash
# Runs the runtime under Wine on Linux.
#   linux/run.sh /path/to/your/dump/<title>.exe
#
# Setup (once):
#   - a Proton build's Wine, e.g. GE-Proton (VICTORY_PROTON=<its folder>,
#     default ~/.steam/root/compatibilitytools.d/GE-Proton10-34)
#   - the prefix (VICTORY_PREFIX, default ~/.local/share/victory-linux/prefix)
#     is made on the first run, with that Proton's vkd3d-proton (d3d12,
#     d3d12core) and DXVK (dxgi) in it
#   - the executable from linux/build.sh (VICTORY_EXE to use another one)
set -uo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
DATA="${VICTORY_HOME:-$HOME/.local/share/victory-linux}"
P="${VICTORY_PROTON:-$HOME/.steam/root/compatibilitytools.d/GE-Proton10-34}/files"
EXE="${VICTORY_EXE:-$HERE/target/x86_64-pc-windows-gnu/release/victory.exe}"
[ -x "$P/bin/wine" ] || { echo "no Wine at $P/bin/wine (set VICTORY_PROTON)" >&2; exit 1; }
[ -f "$EXE" ] || { echo "no $EXE (run linux/build.sh, or set VICTORY_EXE)" >&2; exit 1; }
[ -n "${1:-}" ] || { echo "usage: linux/run.sh /path/to/<title>.exe" >&2; exit 1; }
mkdir -p "$DATA"/{cache/vkd3d,data,logs}
export PATH="$P/bin:$PATH" WINESERVER="$P/bin/wineserver" WINELOADER="$P/bin/wine" WINEDLLPATH="$P/lib/wine"
export WINEPREFIX="${VICTORY_PREFIX:-$DATA/prefix}"
export WINEDEBUG="${WINEDEBUG:-err+all,fixme-all}"
export WINEDLLOVERRIDES="${WINEDLLOVERRIDES:-d3d12,d3d12core,dxgi,d3d11,d3d10core,d3dcompiler_47=n}"
# Waits in the kernel (futex_waitv) rather than a round trip to wineserver each.
export WINEFSYNC="${WINEFSYNC-1}"
if [ ! -f "$WINEPREFIX/system.reg" ]; then
  echo "making the prefix $WINEPREFIX"
  "$P/bin/wine" wineboot -u >/dev/null 2>&1
  "$P/bin/wineserver" -w
fi
S32="$WINEPREFIX/drive_c/windows/system32"
for f in vkd3d-proton/x86_64-windows/d3d12.dll vkd3d-proton/x86_64-windows/d3d12core.dll dxvk/x86_64-windows/dxgi.dll; do
  [ -f "$P/lib/wine/$f" ] && cmp -s "$P/lib/wine/$f" "$S32/$(basename "$f")" || cp -f "$P/lib/wine/$f" "$S32/" 2>/dev/null
done
w() { "$P/bin/wine" winepath -w "$1" 2>/dev/null; }
export VICTORY_CACHE_DIR="$(w "$DATA/cache")" VICTORY_DATA_DIR="$(w "$DATA/data")"
# vkd3d-proton otherwise keeps its pipeline cache in the working directory:
# launched from elsewhere, a run starts cold (and a slow compile during the
# world's load can get a pipeline left out).
export VKD3D_SHADER_CACHE_PATH="${VKD3D_SHADER_CACHE_PATH:-$(w "$DATA/cache/vkd3d")}"
# Defaults that make the world run under Wine (each can be overridden):
# the title's memory is copied (read in place, the intro stays black),
export VICTORY_COPY_MEMORY="${VICTORY_COPY_MEMORY-1}"
# no sparse copies (their tile binds made the card reset on RADV),
export VICTORY_SPARSE="${VICTORY_SPARSE-0}"
# room for card buffers (at 2 GiB they filled up and characters lost their skin),
export VICTORY_CARD_BUFFERS_MB="${VICTORY_CARD_BUFFERS_MB-4096}"
export VICTORY_HAIR="${VICTORY_HAIR-1}" VICTORY_FPS="${VICTORY_FPS-0}"
export VICTORY_ASYNC_SUBMIT="${VICTORY_ASYNC_SUBMIT-1}"
LOG="$DATA/logs/$(date +%Y%m%d-%H%M%S).log"
echo "log: $LOG"
"$P/bin/wine" "$EXE" run "$(w "$1")" --execute 2> >(tee "$LOG" >&2)

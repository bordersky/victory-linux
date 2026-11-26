#!/usr/bin/env bash
# Runs the runtime under Wine on Linux.
#   linux/run.sh /path/to/your/dump/<title>.exe          the world as play.ps1 runs it
#   linux/run.sh /path/to/your/dump/<title>.exe --rt     with the ray-tracing structure builder
#
# Setup (once):
#   - a Proton build's Wine, e.g. GE-Proton (VICTORY_PROTON=<its folder>,
#     default ~/.steam/root/compatibilitytools.d/GE-Proton10-34)
#   - a wineserver that keeps mapped views in a tree (VICTORY_WINESERVER=<path>):
#     stock Wine spends seconds a frame there (Wine bug 60868, CachyOS/wine-cachyos#50);
#     build that server from the same Wine version's source, see README.md
#   - vkd3d-proton master for the ray-traced passes (VICTORY_VKD3D=<folder holding
#     d3d12.dll and d3d12core.dll>): released ones' dxil-spirv overflows its stack on
#     some of the title's compute shaders and those pipelines never arrive
#   - Microsoft's d3dcompiler_47.dll in the prefix (winetricks d3dcompiler_47): without
#     it the layer's own shaders do not compile and the cinematics are black
#   - the prefix (VICTORY_PREFIX, default ~/.local/share/victory-linux/prefix) is
#     made on the first run, with that Proton's vkd3d-proton and DXVK (dxgi) in it
#   - the executable from linux/build.sh (VICTORY_EXE to use another one)
set -uo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
DATA="${VICTORY_HOME:-$HOME/.local/share/victory-linux}"
P="${VICTORY_PROTON:-$HOME/.steam/root/compatibilitytools.d/GE-Proton10-34}/files"
EXE="${VICTORY_EXE:-$HERE/target/x86_64-pc-windows-gnu/diag/victory.exe}"
RT=0; ARGS=()
for a in "$@"; do [ "$a" = --rt ] && RT=1 || ARGS+=("$a"); done
set -- "${ARGS[@]}"
[ -x "$P/bin/wine" ] || { echo "no Wine at $P/bin/wine (set VICTORY_PROTON)" >&2; exit 1; }
[ -f "$EXE" ] || { echo "no $EXE (run linux/build.sh, or set VICTORY_EXE)" >&2; exit 1; }
[ -n "${1:-}" ] || { echo "usage: linux/run.sh /path/to/<title>.exe [--rt]" >&2; exit 1; }
mkdir -p "$DATA"/{cache/vkd3d,data,logs,saves-backup}
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
[ -f "$S32/d3dcompiler_47.dll" ] && [ "$(stat -c %s "$S32/d3dcompiler_47.dll")" -gt 3000000 ] \
  || echo "warning: no Microsoft d3dcompiler_47.dll in the prefix (winetricks d3dcompiler_47): black cinematics" >&2
for f in vkd3d-proton/x86_64-windows/d3d12.dll vkd3d-proton/x86_64-windows/d3d12core.dll dxvk/x86_64-windows/dxgi.dll; do
  src="$P/lib/wine/$f"
  [ -n "${VICTORY_VKD3D:-}" ] && [ "${f%%/*}" = vkd3d-proton ] && src="$VICTORY_VKD3D/$(basename "$f")"
  [ -f "$src" ] && { cmp -s "$src" "$S32/$(basename "$f")" || cp -f "$src" "$S32/"; }
done
[ -z "${VICTORY_VKD3D:-}" ] && [ "$RT" = 1 ] && echo "warning: --rt without VICTORY_VKD3D (vkd3d-proton master): ray-tracing pipelines may never arrive" >&2
# The patched wineserver, started first: the title's processes join the server that owns the prefix.
if [ -n "${VICTORY_WINESERVER:-}" ]; then
  pgrep -f "$VICTORY_WINESERVER" >/dev/null || { "$P/bin/wineserver" -k 2>/dev/null; "$VICTORY_WINESERVER" -p10; }
else
  echo "warning: stock wineserver (set VICTORY_WINESERVER): expect seconds a frame" >&2
fi
w() { "$P/bin/wine" winepath -w "$1" 2>/dev/null; }
export VICTORY_CACHE_DIR="$(w "$DATA/cache")" VICTORY_DATA_DIR="$(w "$DATA/data")"
# vkd3d-proton otherwise keeps its pipeline cache in the working directory:
# launched from elsewhere, a run starts cold.
export VKD3D_SHADER_CACHE_PATH="${VKD3D_SHADER_CACHE_PATH:-$(w "$DATA/cache/vkd3d")}"
# scripts/play.ps1's settings (each can be overridden from the environment).
d() { export "$1=${!1-$2}"; }
d VICTORY_TEXTURE_NO_WAIT 1; d VICTORY_WIDE_BACK 1; d VICTORY_CARD_REHASH_RUNS 1; d VICTORY_JOIN_COPIES 1
d VICTORY_INDIRECT_WINDOW 1; d VICTORY_RELAY_COPY 1; d VICTORY_INDIRECT_BATCH 2; d VICTORY_MAP_THREADS 1
d VICTORY_TITLE_PRIORITIES 0; d VICTORY_CARD_AWAKE 1; d VICTORY_SHOW_AFTER 1; d VICTORY_AMBIENT 0.8
d VICTORY_D3D12_DRED light; d VICTORY_THIN 1; d VICTORY_JOIN_PARTS 8; d VICTORY_UPLOAD_AHEAD 1
d VICTORY_RELAY 2; d VICTORY_CARD_STEPS 2
# Under vkd3d-proton relay stage 2 needs the fetch check: pieces of card copies fetched while a
# later flight rewrote them (one or two a frame) otherwise reach title memory, and skinned meshes
# (faces) are lost. The land echo breaks head skin under Wine (avatarmanz/VIctory-7900XTX#59).
d VICTORY_FETCH_CHECK 1; d VICTORY_LAND_ECHO 0
# Title buffers of 1 MiB or more copied in the card's memory (up to 4 GiB): the card finds what
# shaders wrote instead of the CPU comparing it (~107 -> ~28 MB a frame on the CPU).
d VICTORY_CARD_BUFFERS_MIN_MB 1; d VICTORY_CARD_BUFFERS_MB 4096
# Renderer patch 1 (VICTORY_PORT_CONSTANTS=1) gives a black screen under vkd3d-proton: off.
# Patch 2 on, with its fallback asked every frame and 128 pages a frame looked at: under Wine its
# memcpy hook marks none of the ~250 view records a frame the fallback finds, and with the
# defaults (every 8 frames, 16 pages) they stayed wrong for frames (stretched textures).
d VICTORY_PORT_DESCRIPTORS 1; d VICTORY_PORT_DESCRIPTORS_EVERY 1; d VICTORY_PORT_DESCRIPTORS_CHECK 128
# The title pins its threads as if SMT siblings were numbered side by side (as Windows does);
# Linux numbers them n and n+cores. Wine is shown them side by side (avatarmanz#129, zocram4cc).
if [ -z "${WINE_CPU_TOPOLOGY+x}" ]; then
  topo=$(for c in /sys/devices/system/cpu/cpu[0-9]*/topology/thread_siblings_list; do cat "$c"; done | sort -t, -k1n -u | tr '\n' ',' | sed 's/,$//')
  case "$topo" in *-*) ;; ?*) export WINE_CPU_TOPOLOGY="$(nproc):$topo" ;; esac
fi
[ "$RT" = 1 ] && export VICTORY_BVH_BUILDER=1
# The title's saves, copied at each start (the last 5 kept): a run killed while the title
# writes its checkpoint can leave one that loads characters without faces.
if [ -n "$(ls -A "$DATA/data" 2>/dev/null)" ]; then
  cp -a "$DATA/data" "$DATA/saves-backup/$(date +%Y%m%d-%H%M%S)"
  ls -1d "$DATA/saves-backup/"*/ 2>/dev/null | head -n -5 | xargs -r rm -rf
fi
LOG="$DATA/logs/$(date +%Y%m%d-%H%M%S).log"
echo "log: $LOG"
"$P/bin/wine" "$EXE" run "$(w "$1")" --execute 2> >(tee "$LOG" >&2)

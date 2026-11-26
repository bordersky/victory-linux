# victory-linux

The [VIctory](https://github.com/Marvisak/VIctory) runtime for Xbox Series
titles, built and run on **Linux under Wine**, with what it takes to run there
and the measurements behind each choice.

It now starts from [avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX)
(itself a fork of [fxceless/victory-fork](https://github.com/fxceless/victory-fork)),
the fastest tree measured here: `b0cfe2b` first, then `9c23e8d`, now its main at
`9576c45`, with fxceless/victory-fork#927 (facial animation) and pull requests
#166, #151 and #114 of that repository on top. Until 2026-11-26 it was built on
[Belobug's AMD fork](https://github.com/Belobug/VIctory-Belobug-AMD); that
state is in the history (`51a3912`). See [CREDITS.md](CREDITS.md) for where
each change comes from.

**This repository contains no game files, code or assets, and no keys.** You
need your own dump of a title you own. This project is not affiliated with or
endorsed by any game's developer or publisher, nor by Microsoft.

## State (one machine)

Arch Linux, AMD Radeon RX 7900 XT 20 GB (Mesa RADV), Ryzen 9 5900X, 32 GB RAM,
GE-Proton10-34's Wine with a patched wineserver (below), vkd3d-proton master.
Shader cache warm. The prologue's indoor scenes unless stated:

| Tree | Draws a frame | Frame | FPS |
|---|---|---|---|
| Belobug-based (this repository's previous base) | ~6,000-7,000 | ~290-330 ms | ~3.1-3.4 |
| this base | ~3,900 | 87-88 ms | ~11.4 |
| this base | ~6,500-7,000 | 100-110 ms | 9-10 |
| this base, ray tracing with the structure builder (`--rt`) | ~6,800-7,000 | 148-152 ms | ~6.7 |
| **current tree, `run.sh` defaults** (relay stage 2 with the fetch check) | ~6,800 | 65-67 ms | ~15 |
| current tree, a lighter view of the same prison | ~4,300 | ~58 ms | ~17 |
| previous tree (`9c23e8d`), outdoors (driving) | ~9,000-10,600 | 80-110 ms | 9-12 |
| **current tree, `run.sh` defaults**, from a checkpoint | ~5,900-6,000 | 67-69 ms | ~15 |
| same, with renderer patch 2 (off: textures stretch) | ~2,700-4,000 (moving) | 55-63 ms; 50-55 ms preparing | 16-18 |
| current tree, `--rt`, Series S profile (`--console series-s`) | ~5,000-6,500 | 80-120 ms (up to ~190 where the card is the limit) | 5-12.6 |
| current tree, `--rt` | ~5,000-7,000 | 155-220 ms | 4.5-6.4 |

The rows marked current are from avatarmanz's main at `9576c45` with this
repository's changes (`linux/build.sh release`, `run.sh`'s settings), resuming
the same checkpoint in the prologue; the 15 fps row and the patch row were not
taken at exactly the same spot (the player moved). The rows above them are
from `9c23e8d`, in a new game. On the first row's spot, relay stage 1 gives ~82 ms at ~6,650 draws in
the same build: with stage 2 the submitting thread no longer waits for the card
(~27-31 ms a frame before). The frame is CPU-bound here: the card is about
55-60 % busy, and it waits ~25-30 ms a frame for the thread that prepares
submissions.

With `--rt` the title's own ray-traced light runs: bounce light, sun and lamp
shadows; without it rooms that the sun does not reach are lit by a constant
ambient instead.

Characters keep their faces and skin, also when resuming from a checkpoint
(#166), and their faces animate in play without tearing (#927, #114 and the raw
typed loads). Open:
- **Two lockers** draw black: their chipped paint and numbers are there, the
  base colour is not. Not the card-buffer copies (black with them off too).
- **With `--rt`**, the inmates' outfits look glossy, as if their reflections
  were wrong, and the frame is 155-220 ms.
- **Outdoors** (on `9c23e8d`), some faces and textures blinked or stretched for
  a moment, with the card's memory nearly full; not checked on this tree yet.
- **Two crashes** in the title's code reading address 0
  (`+0x1884fb`, `+0x4e9b578`) with relay stage 2 on `9c23e8d`, one of them
  while driving; not seen on this tree yet.

## What this project adds

The forks this builds on are made and tested on Windows. Everything below was
found by running the world under Wine on an AMD card and measuring there, and is
the work of this project (as [bordersky](https://github.com/bordersky)) unless
credited otherwise.

| Under Wine / Linux | What was found | Where |
|---|---|---|
| ~13,000 `VirtualQuery` a frame, contending on Wine's one `virtual_mutex` | Wine answers no `MemoryRegionInformationEx`, so no hole is ever known; a `VirtualQuery` fallback (Kayotic1337) | carried here, [avatarmanz#59](https://github.com/avatarmanz/VIctory-7900XTX/issues/59); here ~10 a frame |
| The ray-tracing builder needs a global shortcut (`VICTORY_FIRSTLANE_OWN=1`) | no program of the title's shader archive needs it (all 26,084 translate without it); taken by default in the builder's programs only | here, [avatarmanz#101](https://github.com/avatarmanz/VIctory-7900XTX/issues/101#issuecomment-7183022660) |
| Ray-tracing pipelines never arrive | the dxil-spirv in released vkd3d-proton recurses without bound on some large compute shaders and the pipeline thread dies; fixed in vkd3d-proton master | `VICTORY_VKD3D`, [avatarmanz#111](https://github.com/avatarmanz/VIctory-7900XTX/issues/111#issuecomment-7183122720) |
| Main characters without faces (eyes, teeth and hair only) with relay stage 2 | pieces of card copies fetched on the copy queue while a later flight rewrote them (one or two a frame) reach title memory; the fork's `VICTORY_FETCH_CHECK=1` (off by default) fetches them again. The fork's own test `a_result_caught_in_between_is_put_right` fails under Wine without it and passes with it | `run.sh` defaults below, [avatarmanz#138](https://github.com/avatarmanz/VIctory-7900XTX/issues/138) |
| Main characters without faces after resuming from a checkpoint | the head blend-shape map's compute pipeline was not made yet when the title dispatched it once; a wait for it (Beqren) fixes it. Unbounded, the wait held a frame past the title's ~30 s watchdog on a cold shader cache: a crash while loading (`+0x23cf683`) after every translator change; #166's bounded wait loads from a cold cache without it | carried here, [avatarmanz#166](https://github.com/avatarmanz/VIctory-7900XTX/pull/166) |
| Every session started with no pipelines kept | vkd3d-proton refuses an empty pipeline library given as a non-null empty slice; #151 asks with a null blob. Here: 1,700 pipelines kept after the first run | carried here, [avatarmanz#151](https://github.com/avatarmanz/VIctory-7900XTX/pull/151) |
| Faces torn while characters talk (mouths far too wide) | 16-bit integer typed loads after a group barrier read stale values on RADV; Beqren's raw-view read, taken only where the view's word says a 16-bit integer view of the loaded size. Without that check (a first port here) a poster's texture went wrong | carried here, Beqren's #129 and [avatarmanz#114](https://github.com/avatarmanz/VIctory-7900XTX/pull/114) |
| Renderer patch 1 (`VICTORY_PORT_CONSTANTS=1`) | a black screen from the start screen on under vkd3d-proton ([avatarmanz#163](https://github.com/avatarmanz/VIctory-7900XTX/issues/163)) | off |
| Renderer patch 2 (`VICTORY_PORT_DESCRIPTORS=1`) | ~5 ms a frame less preparing. Its hook on the title's `memcpy`/`memmove` sees ~5,200 copies a frame into the view heap and marks none of their records; its fallback finds ~190-285 view records a frame (written by Wine's `ucrtbase` `memcpy` for ~40 % of them, and by two places in the title's code). Off here for now: the stretching first blamed on it turned out to be geometry spikes from relay stage 2 (below); not re-tested since | off, open |
| Ray tracing in the Series X profile | the card is the limit (83-89 % busy, the submitting thread waits ~110 ms a frame for it); the Series S profile's lower resolution gives 80-120 ms against 140-195 | `--console series-s` |
| Head skin wrong under Wine | the land echo (Kayotic1337, #59) | `run.sh` defaults below |
| No facial animation in play | fxceless/victory-fork#927 (tippythetaidum) merged with avatarmanz's partial fix (VIctory `4243a96a`): views in tables take #927's path, views picked from the heap keep `integer_view` | carried here, [#927 comment](https://github.com/fxceless/victory-fork/pull/927#issuecomment-7185216568) |
| Seconds a frame with the GPU idle | stock wineserver keeps mapped views in a list (Wine bug 60868) | patched wineserver, below |

Earlier, on the Belobug base: the pipeline library refused by vkd3d-proton
([Belobug#124](https://github.com/Belobug/VIctory-Belobug-AMD/pull/124)),
sparse copies resetting the card on RADV
([Belobug#69](https://github.com/Belobug/VIctory-Belobug-AMD/pull/69)), the
card-buffer budget and skin ([Belobug#105](https://github.com/Belobug/VIctory-Belobug-AMD/issues/105)),
and measurements posted on Belobug #123 and #119.

## Build (on Linux)

Needs `rustup`, `mingw-w64-gcc` and `curl`.

```sh
linux/build.sh            # target/x86_64-pc-windows-gnu/diag/victory.exe (as the fork builds it)
linux/build.sh release    # without the diagnostics switches
```

The BCPack format's constant tables are not stored here; the build downloads
them from upstream VIctory and checks their SHA-256 (see
[docs/asset-policy.md](docs/asset-policy.md)).

## Run

```sh
linux/run.sh /path/to/your/dump/<title>.exe          # as play.ps1 runs it
linux/run.sh /path/to/your/dump/<title>.exe --rt     # with the ray-tracing structure builder
```

Running under gamescope works well, e.g.
`gamescope -W 3840 -H 2160 -w 1920 -h 1080 -S integer -f -- linux/run.sh ...`.
Caches, logs and saves go to `~/.local/share/victory-linux`; the first run makes
a prefix there with the Proton's vkd3d-proton and DXVK.

Once, before that:

1. **A Proton build's Wine**: GE-Proton is what is tested
   (`VICTORY_PROTON=/path/to/GE-ProtonX-Y`, default
   `~/.steam/root/compatibilitytools.d/GE-Proton10-34`).
2. **Microsoft's `d3dcompiler_47.dll`** in the prefix
   (`WINEPREFIX=~/.local/share/victory-linux/prefix winetricks d3dcompiler_47`):
   without it the cinematics are black.
3. **A wineserver that keeps mapped views in a tree**: build `server/wineserver`
   from the Wine source of the same version as your Proton with the change of
   [CachyOS/wine-cachyos#50](https://github.com/CachyOS/wine-cachyos/pull/50)
   (Wine bug 60868), and point `VICTORY_WINESERVER` at it. Only the server
   changes; `run.sh` starts it before the title.
4. **For `--rt`, vkd3d-proton master** (`71cae09` or later, with dxil-spirv
   `b540c8a`), built from its repository, and `VICTORY_VKD3D` pointing at the
   folder holding its `d3d12.dll` and `d3d12core.dll`.

What `run.sh` sets besides `scripts/play.ps1`'s settings, and why (each can be
overridden from the environment):

| Setting | Why |
|---|---|
| `VICTORY_RELAY=1 VICTORY_CARD_STEPS=0` | with `play.ps1`'s stage 2 and card steps 2 (and the fetch check), parts of the geometry spike for a frame now and then: torn vertex data the check does not catch. Stage 1: none, ~84-89 ms a frame against ~70 |
| `VICTORY_FETCH_CHECK=1` | with relay stage 2 (`VICTORY_RELAY=2`), without it skinned meshes (faces) are lost under vkd3d-proton: torn fetches, above |
| `VICTORY_LAND_ECHO=0` | breaks head skin under Wine |
| `VICTORY_CARD_BUFFERS_MIN_MB=1 VICTORY_CARD_BUFFERS_MB=4096` | title buffers of 1 MiB or more get a copy in the card's memory (up to 4 GiB), where the card finds what shaders wrote: ~107 -> ~28 MB a frame compared on the CPU, ~10 % a frame (without any such copies: 0.3-1.3 fps) |
| `WINE_CPU_TOPOLOGY` (from `/sys`) | the title pins its threads as if SMT siblings were numbered side by side, as Windows numbers them; Linux numbers them `n` and `n+cores` (zocram4cc on avatarmanz#129); here about the same frame with ~15 % more draws |
| `VICTORY_BVH_BUILDER=1` (`--rt`) | the title's ray-tracing structure builder |
| `VKD3D_SHADER_CACHE_PATH` | otherwise vkd3d-proton keeps its cache in the working directory and runs start cold |
| `WINEFSYNC=1` | waits in the kernel instead of round trips to wineserver |

**Memory:** with `--rt` the process keeps ~15 GB resident and the card uses
~8-9 GB of system memory besides; 32 GB is enough with little else open.
**Saves:** `run.sh` copies the save folder at each start (the last five are
kept in `saves-backup/`).

**After a build that changes translations**, the first run translates again
and the driver makes pipelines: loading is slower once. With #166's bounded
wait this no longer crashes.

## Licence

GPL-3.0-or-later, as upstream ([LICENSE](LICENSE)). The BCPack tables are not
covered by it and are not part of this repository.

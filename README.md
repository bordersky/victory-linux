# victory-linux

The [VIctory](https://github.com/Marvisak/VIctory) runtime for Xbox Series
titles, built and run on **Linux under Wine**, with what it takes to run there
and the measurements behind each choice.

It now starts from [avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX)
at `b0cfe2b` (itself a fork of [fxceless/victory-fork](https://github.com/fxceless/victory-fork)),
the fastest tree measured here. Until 2026-11-26 it was built on
[Belobug's AMD fork](https://github.com/Belobug/VIctory-Belobug-AMD); that
state is in the history (`51a3912`). See [CREDITS.md](CREDITS.md) for where
each change comes from.

**This repository contains no game files, code or assets, and no keys.** You
need your own dump of a title you own. This project is not affiliated with or
endorsed by any game's developer or publisher, nor by Microsoft.

## State (one machine)

Arch Linux, AMD Radeon RX 7900 XT 20 GB (Mesa RADV), Ryzen 9 5900X, 32 GB RAM,
GE-Proton10-34's Wine with a patched wineserver (below), vkd3d-proton master.
Same spot in the prologue (an indoor scene), shader cache warm:

| Tree | Draws a frame | Frame | FPS |
|---|---|---|---|
| Belobug-based (this repository's previous base) | ~6,000-7,000 | ~290-330 ms | ~3.1-3.4 |
| this base | ~3,900 | 87-88 ms | ~11.4 |
| this base | ~6,500-7,000 | 100-110 ms | 9-10 |
| this base, ray tracing with the structure builder (`--rt`) | ~6,800-7,000 | 148-152 ms | ~6.7 |

With `--rt` the title's own ray-traced light runs: bounce light, sun and lamp
shadows; without it rooms that the sun does not reach are lit by a constant
ambient instead. Characters keep their faces and skin with the settings below, in a session started from a new game (resuming from a checkpoint loses the main characters' faces: open).
Minor graphical glitches remain.

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
| Main characters without faces (eyes, teeth and hair only) | not a bad save: a session resumed from a checkpoint draws them so, any setting, builder on or off; a new game draws them | open, [avatarmanz#67](https://github.com/avatarmanz/VIctory-7900XTX/issues/67#issuecomment-7184111949) |
| Faces and skinned meshes wrong under vkd3d-proton | relay stage 2 and card steps (firefloc, #13), the land echo (#59) | `run.sh` defaults below |
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
| `VICTORY_RELAY=1` | stage 2 hands back skinned meshes too early under vkd3d-proton (faces gone) |
| `VICTORY_CARD_STEPS=0` | card steps hand back wrong skinned vertices (also on Windows) |
| `VICTORY_LAND_ECHO=0` | breaks head skin under Wine |
| `VICTORY_BVH_BUILDER=1` (`--rt`) | the title's ray-tracing structure builder |
| `VKD3D_SHADER_CACHE_PATH` | otherwise vkd3d-proton keeps its cache in the working directory and runs start cold |
| `WINEFSYNC=1` | waits in the kernel instead of round trips to wineserver |

**Memory:** with `--rt` the process keeps ~15 GB resident and the card uses
~8-9 GB of system memory besides; 32 GB is enough with little else open.
**Saves:** `run.sh` copies the save folder at each start (the last five are
kept in `saves-backup/`). Resuming from a checkpoint currently loses the main
characters' faces here; a new game keeps them.

## Licence

GPL-3.0-or-later, as upstream ([LICENSE](LICENSE)). The BCPack tables are not
covered by it and are not part of this repository.

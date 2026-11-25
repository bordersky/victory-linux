# victory-linux

The [VIctory](https://github.com/Marvisak/VIctory) runtime for Xbox Series
titles, built and run on **Linux under Wine**, with the fixes that make it work
there and the performance changes that measured best, gathered from several
forks.

It starts from [Belobug's AMD fork](https://github.com/Belobug/VIctory-Belobug-AMD)
at `01aadd1`. See [CREDITS.md](CREDITS.md) for where each change comes from.

**This repository contains no game files, code or assets, and no keys.** You
need your own dump of a title you own. This project is not affiliated with or
endorsed by any game's developer or publisher, nor by Microsoft.

## State (one machine)

Arch Linux, AMD Radeon RX 7900 XT 20 GB (Mesa RADV), Ryzen 9 5900X, 32 GB RAM,
GE-Proton10-34's Wine with vkd3d-proton and DXVK. Same save, same spot, a heavy
indoor scene, world frames:

| Build | Frame | FPS |
|---|---|---|
| Belobug `01aadd1` under Wine (with the switches below) | ~680 ms | ~1.5 |
| + thin LTO, fixed vkd3d-proton cache, pipeline library | ~624 ms | ~1.6 |
| + skip-unchanged copy-back merge (Belobug #123) | ~330 ms | ~3.1 |
| + faster bindless planning and indirect clamps (#119, #120) | ~290 ms | ~3.4 |

Not playable yet. Nothing here has been run on NVIDIA or Intel cards under
Linux.

Known problems: blocky artefacts in some screen-space reflections; a few
hundred draws a frame are still dropped (shaders that do not translate yet);
the first loading takes a while (pipelines are kept for the next runs).

## Build (on Linux)

Needs `rustup`, `mingw-w64-gcc` and `curl`.

```sh
linux/build.sh            # target/x86_64-pc-windows-gnu/release/victory.exe
linux/build.sh diag       # with per-frame copy statistics
```

The BCPack format's constant tables are not stored here; the build downloads
them from upstream VIctory and checks their SHA-256 (see
[docs/asset-policy.md](docs/asset-policy.md)).

## Run

Needs a Proton build's Wine; GE-Proton is what is tested
(`VICTORY_PROTON=/path/to/GE-ProtonX-Y`, default
`~/.steam/root/compatibilitytools.d/GE-Proton10-34`).

```sh
linux/run.sh /path/to/your/dump/<title>.exe
```

The first run makes a prefix (`~/.local/share/victory-linux/prefix`) with that
Proton's vkd3d-proton and DXVK. Caches and logs go to
`~/.local/share/victory-linux`. Running under gamescope works well, e.g.
`gamescope -W 3840 -H 2160 -w 1920 -h 1080 -S integer -f -- linux/run.sh ...`.

What `run.sh` sets, and why (each can be overridden from the environment):

| Setting | Why |
|---|---|
| `VICTORY_COPY_MEMORY=1` | read in place, the intro stays black under Wine |
| `VICTORY_SPARSE=0` | sparse copies' tile binds made the card reset on RADV |
| `VICTORY_CARD_BUFFERS_MB=4096` | at 2 GiB the budget filled up and characters lost their skin |
| `VKD3D_SHADER_CACHE_PATH` | otherwise vkd3d-proton keeps its cache in the working directory and runs start cold |
| `WINEFSYNC=1` | waits in the kernel instead of round trips to wineserver |

## Licence

GPL-3.0-or-later, as upstream ([LICENSE](LICENSE)). The BCPack tables are not
covered by it and are not part of this repository.

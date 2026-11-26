# Credits

This repository is a tree of other people's work with Linux fixes on top.

- **[Marvisak](https://github.com/Marvisak/VIctory) and the VIctory
  contributors** wrote the runtime: the loader, the runtime services, the
  Direct3D 12 back end and the shader recompiler.
- **[fxceless/victory-fork](https://github.com/fxceless/victory-fork)** and the
  people whose pull requests it carries (its own description is kept in
  [docs/README-fxceless.md](docs/README-fxceless.md)).
- **[avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX)**:
  this repository's base since 2026-11-26, at `b0cfe2b` (speed on AMD RDNA 3,
  frame record and replay, findings). Its commits name the commits and authors
  they were taken from; its credits include:
  - **[DexNom/VIctory](https://github.com/DexNom/VIctory)**: ray tracing (the
    emulated ray/node test, title memory by address, the structure builder's
    translation), cube gradients, sparse textures;
  - **[Belobug/VIctory-Belobug-AMD](https://github.com/Belobug/VIctory-Belobug-AMD)**:
    the ambient light idea, pipeline workers, and work by romainhedouin;
  - **[FQY55kai/ZJOU-VICTORY](https://github.com/FQY55kai/ZJOU-VICTORY)**;
  - **[myokex/victory-amd](https://github.com/myokex/victory-amd)**: the guard
    over every view of a page (with Spexivus, joroyt and tightfall), and
    Joshua Odom's reads into title memory;
  - **Geckomacabre** and **cxy0072019-sketch**: the texture read-back.

## Changes carried here

| Change | Author | Where |
|---|---|---|
| Holes found by `VirtualQuery` when Wine does not answer `MemoryRegionInformationEx` | Kayotic1337 | [avatarmanz#59](https://github.com/avatarmanz/VIctory-7900XTX/issues/59) |
| The first-lane shortcut by default in the structure builder's programs only | bordersky | [avatarmanz#101](https://github.com/avatarmanz/VIctory-7900XTX/issues/101) |
| Comments name no game | bordersky | here |
| Linux build and run scripts (`linux/`), the BCPack tables fetched rather than stored, saves copied at each start | bordersky | here |

## Settings and findings from others' reports

- `VICTORY_RELAY=1 VICTORY_CARD_STEPS=0` under vkd3d-proton: firefloc and the
  commenters of [avatarmanz#13](https://github.com/avatarmanz/VIctory-7900XTX/issues/13);
  `VICTORY_LAND_ECHO=0`: Kayotic1337, [avatarmanz#59](https://github.com/avatarmanz/VIctory-7900XTX/issues/59).
- The wineserver that keeps mapped views in a tree:
  [CachyOS/wine-cachyos#50](https://github.com/CachyOS/wine-cachyos/pull/50)
  (Wine bug 60868), first applied to VIctory in fxceless/victory-fork#184 and
  reported for this fork by Fran-co-F in
  [avatarmanz#111](https://github.com/avatarmanz/VIctory-7900XTX/issues/111).
- vkd3d-proton and dxil-spirv by Hans-Kristian Arntzen and contributors; the
  stack fix this needs is in dxil-spirv `b540c8a`.

## The previous base (until 2026-11-26)

Belobug's fork at `01aadd1` with Belobug pull requests #123 (altuux), #119
(iWebbIO), #120 (ESAT586), #87 (bgschf), #96 (Key-rs), #124 and #69 (ours);
see the history up to `51a3912`.

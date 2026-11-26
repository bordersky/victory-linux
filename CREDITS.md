# Credits

This repository is a tree of other people's work with Linux fixes on top.

- **[Marvisak](https://github.com/Marvisak/VIctory) and the VIctory
  contributors** wrote the runtime: the loader, the runtime services, the
  Direct3D 12 back end and the shader recompiler.
- **[fxceless/victory-fork](https://github.com/fxceless/victory-fork)** and the
  people whose pull requests it carries (its own description is kept in
  [docs/README-fxceless.md](docs/README-fxceless.md)).
- **[avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX)**:
  this repository's base since 2026-11-26, at `b0cfe2b`, its main at
  `9c23e8d`, now its main at `9576c45` (speed on AMD RDNA 3, the renderer
  patches, pull requests of many contributors merged there,
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
  - **Geckomacabre** and **cxy0072019-sketch**: the texture read-back;
  - **codec_d** ([cool_fox](https://github.com/codec-d)): the table plan, the
    residency budgets, the two-plane compose and more, merged there.

## Changes carried here

| Change | Author | Where |
|---|---|---|
| Facial animation: 16-bit buffer accesses keep their bits (fxceless/victory-fork#927, six commits cherry-picked; merged here with VIctory `4243a96a` already in the base) | tippythetaidum; the merge bordersky | [fxceless#927](https://github.com/fxceless/victory-fork/pull/927) |
| Holes found by `VirtualQuery` when Wine does not answer `MemoryRegionInformationEx` | Kayotic1337 | [avatarmanz#59](https://github.com/avatarmanz/VIctory-7900XTX/issues/59) |
| The first-lane shortcut by default in the structure builder's programs only | bordersky | [avatarmanz#101](https://github.com/avatarmanz/VIctory-7900XTX/issues/101) |
| A compute dispatch waits for its own pipeline under Wine, bounded per frame and per pipeline (heads without skin after loading a checkpoint) | Beqren | [avatarmanz#166](https://github.com/avatarmanz/VIctory-7900XTX/pull/166) |
| An empty pipeline library is created with a null blob, which vkd3d-proton accepts (every session started cold before) | AugustoZFJ (Marvisak/VIctory#347, also Filip722 on fxceless#885), gomeaw | [avatarmanz#151](https://github.com/avatarmanz/VIctory-7900XTX/pull/151) |
| 16-bit accesses ask for the kind of a view in a table too | avatarmanz, secure-silicon | [avatarmanz#114](https://github.com/avatarmanz/VIctory-7900XTX/pull/114) |
| 16-bit integer typed loads after a group barrier read the raw view, where the view's word says so (faces torn while talking on RADV) | Beqren (from his #129); the port onto this tree bordersky | [avatarmanz#129](https://github.com/avatarmanz/VIctory-7900XTX/pull/129) |
| `VICTORY_FAULT_PAGES`: how many pages shaders' faults may bring in, in all | bordersky | here |
| `WINE_CPU_TOPOLOGY` from the CPU's sibling pairs | zocram4cc (the finding) | [avatarmanz#129](https://github.com/avatarmanz/VIctory-7900XTX/pull/129#issuecomment-7186324761) |
| Comments name no game | bordersky | here |
| Relay stage 2 with the fetch check under vkd3d-proton (torn fetches found by the fork's device tests run under Wine) | bordersky | here |
| Linux build and run scripts (`linux/`), the BCPack tables fetched rather than stored, saves copied at each start | bordersky | here |

## Settings and findings from others' reports

- Faces lost under vkd3d-proton with relay stage 2: first reported by firefloc
  and the commenters of [avatarmanz#13](https://github.com/avatarmanz/VIctory-7900XTX/issues/13)
  (who worked around it with `VICTORY_RELAY=1 VICTORY_CARD_STEPS=0`); the fetch
  check that fixes it is avatarmanz's (`VICTORY_FETCH_CHECK`, `host/torn.rs`);
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

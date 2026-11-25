# Credits

- **[Marvisak](https://github.com/Marvisak/VIctory) and the VIctory
  contributors** wrote the runtime.
- **[Belobug](https://github.com/Belobug/VIctory-Belobug-AMD)** and the
  contributors to that fork made the world render on AMD cards; this
  repository starts from it at `01aadd1` (2026-11-25).

## Pull requests carried here (Belobug's fork)

| PR | Author | What |
|---|---|---|
| #123 | altuux | AVX2 fast path and skip-unchanged writes in the copy-back merge |
| #119 | iWebbIO | faster bindless planning, BCPack decoding and shader translation |
| #120 | ESAT586 | hoisted indirect argument clamps batched into SIMD dispatches |
| #87 | bgschf | comments name no game |
| #96 | Key-rs | thin LTO and one codegen unit for the release profile |
| #124 | sil-kong | the empty pipeline library made from a null blob (vkd3d-proton) |
| #69 | sil-kong | no sparse copies of title memory under Wine by default |

## Changes made here

- DirectStorage reads that the kernel refuses (`ERROR_NOACCESS`) retried
  through a buffer (a work-around, not a root cause).
- Linux build and run scripts (`linux/`), the BCPack tables fetched rather
  than stored.

## Ideas from elsewhere

- [DexNom/VIctory](https://github.com/DexNom/VIctory) and Marvisak's
  `cd94d35`: shaders marking the pages they write so copy-back compares only
  those (not ported yet).

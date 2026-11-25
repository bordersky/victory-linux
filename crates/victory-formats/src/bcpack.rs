//! BCPack: lossless packing of BC1-BC7 block streams, as the console's
//! DirectStorage hardware unpacks them. Decode only; BC2 is left out (no
//! title data uses it).
//!
//! A packed stream has no header. It is 12 or 16 interleaved rANS *lanes*;
//! each lane is a run of fragments (header byte, length, payload) coded with
//! one of 128 static models, and a fixed schedule decides which lane takes
//! the next input byte. The lanes' symbols form one record per block: two
//! back references (to one of the last 256 blocks, or to a per-format
//! dictionary that stands in for blocks -1 to -255) and the residuals of the
//! endpoints and indices against them. The models, the index rank tables,
//! the BC7 control codes and the dictionaries are constants of the format,
//! kept in `bcpack-tables.bin` beside this file ([`Tables::builtin`]).

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error(pub &'static str);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BCPack: {}", self.0)
    }
}

impl std::error::Error for Error {}

const MODELS: usize = 128;
const BUCKETS: usize = 9;
const HISTORY: usize = 256;
const DICTIONARY: usize = 255;
/// rANS: decode while the state is above this, read while below.
const LOW: u32 = 1 << 24;
/// Input bytes per schedule round.
const ROUND_BYTES: usize = 8;

/// Bytes of the table file (see [`Tables::parse`]).
pub const TABLES_SIZE: usize = MODELS * BUCKETS * 2
    + 256
    + 64
    + 256
    + 64
    + 64
    + 512 * 2
    + DICTIONARY * (6 + 16 + 8 + 32 + 2 + 16 + 4 + 32 + 25 + 33 + 50 + 66);

/// Endpoints and indices of BC1, BC3, BC4 and BC5: small unsigned values,
/// summed per element modulo the element's width.
#[derive(Clone, Copy, Default)]
struct Plain {
    ep: [u8; 8],
    ix: [u8; 32],
}

#[derive(Clone, Copy, Default)]
struct Bc6Ep {
    /// Endpoint 0 R G B, then endpoints 1-3, left-aligned and biased.
    e: [[u16; 3]; 4],
}

#[derive(Clone, Copy, Default)]
struct Bc6Ix {
    mode: u8,
    ix: [i16; 16],
}

#[derive(Clone, Copy, Default)]
struct Bc7Ep {
    ctrl: u16,
    /// Per subset: R0 R1 G0 G1 B0 B1 A0 A1, left-aligned.
    rows: [[u16; 8]; 3],
}

#[derive(Clone, Copy, Default)]
struct Bc7Ix {
    bits: [u8; 2],
    ix: [[i16; 16]; 2],
}

/// Compact per-bucket decode parameters (16 bytes, one cache line slice).
/// Bucket 0 has `start = 1`, `freq = freq[0] - 1`, `base = 0`, `recip = 0`;
/// bucket 1 has `recip = 0`; buckets `>= 2` carry `ceil(2^32 / freq)`.
/// That makes every bucket follow the same branchless decode formula.
#[derive(Clone, Copy, Default)]
#[repr(C, align(16))]
struct Bucket {
    start: u16,
    freq: u16,
    base: u16,
    _pad: u16,
    recip: u64,
}

/// One rANS model's lookup tables, kept contiguous so active models stay in L1.
#[derive(Clone, Copy)]
struct Model {
    /// Each bucket's first slot; 65536 past the model's last, and one more
    /// 65536 at the end so a bucket always has a next start.
    start: [u32; BUCKETS + 1],
    /// Branchless decode parameters per bucket.
    buckets: [Bucket; BUCKETS],
    /// Per 256-slot window: the bucket of the first slot, with bit 7 (`0x80`)
    /// set when a later bucket starts inside the same window.
    window: [u8; 256],
}

impl Default for Model {
    fn default() -> Self {
        Self {
            start: [0; BUCKETS + 1],
            buckets: [Bucket::default(); BUCKETS],
            window: [0; 256],
        }
    }
}

impl Model {
    #[inline(always)]
    fn bucket_of(&self, slot: u32) -> usize {
        let entry = self.window[(slot >> 8) as usize];
        if entry < 0x80 {
            usize::from(entry)
        } else {
            let mut b = usize::from(entry & 0x7f);
            while self.start[b + 1] <= slot {
                b += 1;
            }
            b
        }
    }

    #[inline(always)]
    fn decode_slot(&self, x: u32) -> (u8, u32) {
        let slot = x & 0xffff;
        let b = self.bucket_of(slot);
        let bkt = self.buckets[b];
        let offset = slot.wrapping_sub(u32::from(bkt.start));
        let q = ((u64::from(offset).wrapping_mul(bkt.recip)) >> 32) as u32;
        let f = u32::from(bkt.freq);
        let sym = (u32::from(bkt.base) + q) as u8;
        let rem = offset.wrapping_sub(q.wrapping_mul(f));
        (sym, (x >> 16).wrapping_mul(f).wrapping_add(rem))
    }

    #[cfg(test)]
    fn start(&self, b: usize) -> u32 {
        self.start[b]
    }

    #[cfg(test)]
    fn freq(&self, b: usize) -> u32 {
        if b == 0 {
            u32::from(self.buckets[0].freq) + 1
        } else {
            u32::from(self.buckets[b].freq)
        }
    }
}

/// The format's constant tables.
pub struct Tables {
    models: Box<[Model; MODELS]>,
    rank1: [u8; 256],
    rank3a: [u8; 64],
    rank3c: [u8; 256],
    rank4: [u8; 64],
    rank5: [u8; 64],
    ctrl7: [u16; 512],
    bc1: Vec<Plain>,
    bc3: Vec<Plain>,
    bc4: Vec<Plain>,
    bc5: Vec<Plain>,
    bc6: Vec<(Bc6Ep, Bc6Ix)>,
    bc7: Vec<(Bc7Ep, Bc7Ix)>,
}

/// Maximum idle capacity kept in a worker's reusable staged-output buffer.
const OUT_CAP: usize = 2 << 20;

/// Reusable per-thread output and history buffers. Decoding streams each block
/// through a stack record into `out` and copies to `dst` only after every lane
/// and block succeeds, so `dst` stays untouched on any decode error while
/// avoiding a separate 1.5-2x intermediate `records` buffer. History entries
/// are only read at `i - r` for `1 <= r <= i`, written earlier in the same call.
struct Workspace {
    out: Vec<u8>,
    plain: Box<[Plain; HISTORY]>,
    bc6: Box<[(Bc6Ep, Bc6Ix); HISTORY]>,
    bc7: Box<[(Bc7Ep, Bc7Ix); HISTORY]>,
}

impl Workspace {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            plain: Box::new([Plain::default(); HISTORY]),
            bc6: Box::new([(Bc6Ep::default(), Bc6Ix::default()); HISTORY]),
            bc7: Box::new([(Bc7Ep::default(), Bc7Ix::default()); HISTORY]),
        }
    }

    fn prepare_out(&mut self, len: usize) {
        if self.out.len() < len {
            self.out.resize(len, 0);
        }
    }

    fn trim(&mut self) {
        if self.out.capacity() > OUT_CAP {
            self.out = Vec::new();
        }
    }
}

thread_local! {
    static WORKSPACE: std::cell::RefCell<Option<Workspace>> = const { std::cell::RefCell::new(None) };
}

fn with_workspace<R>(f: impl FnOnce(&mut Workspace) -> R) -> R {
    WORKSPACE.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            let ws = guard.get_or_insert_with(Workspace::new);
            let out = f(ws);
            ws.trim();
            out
        } else {
            let mut ws = Workspace::new();
            f(&mut ws)
        }
    })
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn bytes<const N: usize>(&mut self) -> [u8; N] {
        let (head, rest) = self.0.split_at(N);
        self.0 = rest;
        head.try_into().unwrap_or([0; N])
    }
    fn u8(&mut self) -> u8 {
        self.bytes::<1>()[0]
    }
    fn u16(&mut self) -> u16 {
        u16::from_le_bytes(self.bytes())
    }
}

impl Tables {
    /// The tables VIctory ships with.
    pub fn builtin() -> Result<Tables, Error> {
        Tables::parse(include_bytes!("bcpack-tables.bin"))
    }

    /// Reads the table file: 128 x 9 u16 model frequencies (per bucket, the
    /// frequency of each of its symbols), the rank tables (BC1 256 B, BC3
    /// alpha 64 B and colour 256 B, BC4 64 B, BC5 64 B), 512 u16 BC7 control
    /// words, then 255 dictionary entries per format in format order: BC1
    /// 6 + 16 B, BC3 8 + 32 B, BC4 2 + 16 B, BC5 4 + 32 B, BC6H (u8 mode,
    /// 12 u16, u8 mode, 16 i16), BC7 (u16 control, 24 u16, 2 u8, 32 i16).
    /// Little-endian, no padding. Every model must tile 65536.
    pub fn parse(bytes: &[u8]) -> Result<Tables, Error> {
        if bytes.len() != TABLES_SIZE {
            return Err(Error("table file has the wrong size"));
        }
        let mut r = Reader(bytes);
        let mut models = Box::new([Model::default(); MODELS]);
        for m in 0..MODELS {
            let model = &mut models[m];
            let mut freq = [0u32; BUCKETS];
            for f in &mut freq {
                *f = u32::from(r.u16());
            }
            // Model 16 g + shape has g + 2 buckets; bucket b > 0 holds 2^(b-1) symbols.
            let used = m / 16 + 2;
            let mut at = 0u32;
            for b in 0..used {
                if freq[b] == 0 {
                    return Err(Error("a model has an empty bucket"));
                }
                model.start[b] = at;
                model.buckets[b] = if b == 0 {
                    Bucket {
                        start: 1,
                        freq: (freq[0] - 1) as u16,
                        base: 0,
                        _pad: 0,
                        recip: 0,
                    }
                } else {
                    Bucket {
                        start: at as u16,
                        freq: freq[b] as u16,
                        base: 1u16 << (b - 1),
                        _pad: 0,
                        recip: if b == 1 {
                            0
                        } else {
                            (1u64 << 32).div_ceil(u64::from(freq[b]))
                        },
                    }
                };
                at += freq[b] << b.saturating_sub(1);
            }
            if at != 1 << 16 {
                return Err(Error("a model does not tile 65536"));
            }
            model.start[used..].fill(1 << 16);
            for (window, entry) in model.window.iter_mut().enumerate() {
                let first = (window as u32) << 8;
                let last = first + 255;
                let b = model.start[1..].iter().filter(|&&s| s <= first).count();
                let crosses = model.start[b + 1] <= last;
                *entry = (b as u8) | if crosses { 0x80 } else { 0 };
            }
        }
        let rank1 = r.bytes();
        let rank3a = r.bytes();
        let rank3c = r.bytes();
        let rank4 = r.bytes();
        let rank5 = r.bytes();
        let mut ctrl7 = [0u16; 512];
        for c in &mut ctrl7 {
            *c = r.u16();
        }
        let mut plain = |ep: usize, ix: usize| {
            (0..DICTIONARY)
                .map(|_| {
                    let mut p = Plain::default();
                    for v in &mut p.ep[..ep] {
                        *v = r.u8();
                    }
                    for v in &mut p.ix[..ix] {
                        *v = r.u8();
                    }
                    p
                })
                .collect::<Vec<_>>()
        };
        let bc1 = plain(6, 16);
        let bc3 = plain(8, 32);
        let bc4 = plain(2, 16);
        let bc5 = plain(4, 32);
        let bc6 = (0..DICTIONARY)
            .map(|_| {
                // The endpoint entry's mode byte plays no part in decoding.
                r.u8();
                let mut ep = Bc6Ep::default();
                for v in ep.e.as_flattened_mut() {
                    *v = r.u16();
                }
                let mut ix = Bc6Ix {
                    mode: r.u8(),
                    ..Default::default()
                };
                for v in &mut ix.ix {
                    *v = r.u16() as i16;
                }
                (ep, ix)
            })
            .collect();
        let bc7 = (0..DICTIONARY)
            .map(|_| {
                let mut ep = Bc7Ep {
                    ctrl: r.u16(),
                    ..Default::default()
                };
                for v in ep.rows.as_flattened_mut() {
                    *v = r.u16();
                }
                let mut ix = Bc7Ix {
                    bits: [r.u8(), r.u8()],
                    ..Default::default()
                };
                for v in ix.ix.as_flattened_mut() {
                    *v = r.u16() as i16;
                }
                (ep, ix)
            })
            .collect();
        Ok(Tables {
            models,
            rank1,
            rank3a,
            rank3c,
            rank4,
            rank5,
            ctrl7,
            bc1,
            bc3,
            bc4,
            bc5,
            bc6,
            bc7,
        })
    }
}

/// Unpacks `src` (BC number `bc`, 1 and 3-7) into `dst`, whose length gives
/// the block count. The stream carries no size: bytes after the last one the
/// lanes need are ignored, a stream that runs out is an error.
pub fn decode(bc: u8, src: &[u8], dst: &mut [u8], tables: &Tables) -> Result<(), Error> {
    let block = match bc {
        1 | 4 => 8,
        3 | 5 | 6 | 7 => 16,
        _ => return Err(Error("unsupported BC format")),
    };
    if dst.is_empty() || !dst.len().is_multiple_of(block) {
        return Err(Error("destination is not a whole number of blocks"));
    }
    let blocks = dst.len() / block;
    let t = tables;
    with_workspace(|ws| {
        let staged_len = dst.len();
        ws.prepare_out(staged_len);
        let staged = &mut ws.out[..staged_len];
        let res = match bc {
            1 => {
                let (out, _) = staged.as_chunks_mut::<8>();
                let history = &mut *ws.plain;
                let zero = Plain::default();
                lanes_stream::<12, 1, 12>(
                    t,
                    [7, 7, 4, 5, 4, 4, 5, 4, 7, 7, 7, 7],
                    src,
                    blocks,
                    |i, s| {
                        plain_block::<6, 16, false>(
                            i,
                            s,
                            &mut out[i],
                            history,
                            &t.bc1,
                            &zero,
                            &BC1,
                            |s| bc1_residual(t, s),
                            bc1_pack,
                        );
                        Ok(())
                    },
                )
            }
            3 => {
                let (out, _) = staged.as_chunks_mut::<16>();
                let history = &mut *ws.plain;
                let zero = Plain::default();
                lanes_stream::<12, 2, 24>(
                    t,
                    [3, 3, 7, 5, 5, 5, 5, 4, 5, 4, 7, 7],
                    src,
                    blocks,
                    |i, s| {
                        plain_block::<8, 32, true>(
                            i,
                            s,
                            &mut out[i],
                            history,
                            &t.bc3,
                            &zero,
                            &BC3,
                            |s| bc3_residual(t, s),
                            bc3_pack,
                        );
                        Ok(())
                    },
                )
            }
            4 => {
                let (out, _) = staged.as_chunks_mut::<8>();
                let history = &mut *ws.plain;
                let zero = Plain::default();
                lanes_stream::<12, 1, 12>(
                    t,
                    [7, 7, 7, 7, 5, 5, 5, 5, 5, 5, 5, 5],
                    src,
                    blocks,
                    |i, s| {
                        plain_block::<2, 16, false>(
                            i,
                            s,
                            &mut out[i],
                            history,
                            &t.bc4,
                            &zero,
                            &BC4,
                            |s| bc4_residual(t, s),
                            bc4_pack,
                        );
                        Ok(())
                    },
                )
            }
            5 => {
                let (out, _) = staged.as_chunks_mut::<16>();
                let history = &mut *ws.plain;
                let zero = Plain::default();
                lanes_stream::<12, 2, 24>(
                    t,
                    [3, 3, 7, 7, 5, 5, 5, 5, 5, 5, 5, 5],
                    src,
                    blocks,
                    |i, s| {
                        plain_block::<4, 32, true>(
                            i,
                            s,
                            &mut out[i],
                            history,
                            &t.bc5,
                            &zero,
                            &BC5,
                            |s| bc5_residual(t, s),
                            bc5_pack,
                        );
                        Ok(())
                    },
                )
            }
            6 => {
                let (out, _) = staged.as_chunks_mut::<16>();
                let history = &mut *ws.bc6;
                let zero = (
                    Bc6Ep::default(),
                    Bc6Ix {
                        mode: 11,
                        ..Default::default()
                    },
                );
                lanes_stream::<16, 2, 32>(
                    t,
                    [3, 3, 7, 7, 7, 4, 4, 4, 5, 5, 5, 7, 7, 7, 7, 4],
                    src,
                    blocks,
                    |i, s| bc6_block(i, s, &mut out[i], history, &t.bc6, &zero),
                )
            }
            _ => {
                let (out, _) = staged.as_chunks_mut::<16>();
                let history = &mut *ws.bc7;
                let zero = (Bc7Ep::default(), Bc7Ix::default());
                lanes_stream::<16, 2, 32>(
                    t,
                    [3, 3, 7, 7, 7, 7, 7, 6, 6, 4, 5, 7, 7, 7, 7, 4],
                    src,
                    blocks,
                    |i, s| bc7_block(i, s, &mut out[i], history, t, &zero),
                )
            }
        };
        if res.is_ok() {
            dst.copy_from_slice(&ws.out[..staged_len]);
        }
        res
    })
}

// Lanes.

const NONE: u16 = 0xffff;

#[derive(Clone, Copy)]
struct Lane {
    x: u32,
    left: u32,
    /// Latched input byte (`0..=255`), or `NONE`.
    latch: u16,
    /// Decoded output symbol (`0..=255`), or `NONE`.
    out: u16,
    /// Symbol bits - 1 at the full alphabet.
    width: u8,
    /// 0 header, 1 length (low byte), 2 length high byte, 3 payload.
    phase: u8,
    model: u8,
    need: bool,
    stalled: bool,
    long: bool,
    reset: bool,
}

impl Default for Lane {
    fn default() -> Self {
        Self {
            x: 0,
            left: 0,
            latch: NONE,
            out: NONE,
            width: 0,
            phase: 0,
            model: 0,
            need: false,
            stalled: false,
            long: false,
            reset: false,
        }
    }
}

impl Lane {
    #[cold]
    #[inline(never)]
    fn step_control(&mut self) {
        if self.latch == NONE {
            return;
        }
        let b = self.latch as u8;
        self.latch = NONE;
        self.need = true;
        match self.phase {
            0 => {
                // The top two bits shrink the alphabet; width >= 3.
                let group = self.width - (b >> 6);
                self.model = (group * 16 + (b & 15)) & 0x7f;
                self.reset = b & 0x10 != 0;
                self.long = b & 0x20 != 0;
                self.left = 0;
                self.phase = 1;
            }
            1 if self.long => {
                self.left = b.into();
                self.phase = 2;
            }
            1 => {
                self.left = u32::from(b) + 1;
                self.phase = 3;
            }
            _ => {
                self.left = (u32::from(b) << 8 | self.left) + 257;
                self.phase = 3;
            }
        }
    }

    /// One step: take the latched byte (header, length or payload) and
    /// decode at most one symbol.
    #[inline]
    fn step(&mut self, t: &Tables) {
        if self.phase != 3 {
            self.step_control();
            return;
        }
        let mut x = self.x;
        if x < LOW && self.latch != NONE {
            self.left = self.left.wrapping_sub(1);
            x = x << 8 | u32::from(self.latch);
            self.latch = NONE;
        }
        self.need = self.left != 0 && self.latch == NONE;
        if x > LOW {
            let (sym, next_x) = t.models[usize::from(self.model & 0x7f)].decode_slot(x);
            self.x = next_x;
            self.out = u16::from(sym);
        } else if self.left != 0 {
            // At exactly 2^24 the lane neither reads nor decodes again; only a
            // lane that has delivered its symbols may end up there.
            self.x = x;
        } else {
            // The fragment ends; a byte read in this step does not reach the carried state.
            self.phase = 0;
            self.need = self.latch == NONE;
            if self.reset {
                self.x = 0;
            }
        }
    }
}

/// The bucket holding `slot` (below 65536): the last whose start is at or
/// below it.
#[cfg(test)]
#[inline]
fn bucket_of(t: &Tables, model: u8, slot: u32) -> usize {
    t.models[usize::from(model)].bucket_of(slot)
}

/// Streams `blocks` records of `N * PER` symbols through a stack record buffer
/// straight into `on_block`. In the steady state (all lanes in payload phase
/// with `left >= 2` and at least `ROUND_BYTES` bytes remaining in `src`), runs
/// a tight loop with cached per-lane `&Model` references and a direct fast path
/// for unstalled lanes (avoiding `lane.out`/`lane.need` round-trips).
fn lanes_stream<const N: usize, const PER: usize, const STRIDE: usize>(
    t: &Tables,
    widths: [u8; N],
    src: &[u8],
    blocks: usize,
    mut on_block: impl FnMut(usize, &[u8; STRIDE]) -> Result<(), Error>,
) -> Result<(), Error> {
    debug_assert_eq!(N * PER, STRIDE);
    let mut lanes = [Lane::default(); N];
    for j in 0..N {
        lanes[j].width = widths[j];
        lanes[j].need = true;
    }
    let full = (1u32 << N) - 1;
    let total_rows = blocks * PER;
    let (mut pos, mut done, mut idle) = (0usize, 0u32, 0u32);
    let mut block_idx = 0usize;
    let mut sub = 0usize;
    let mut src_pos = 0usize;
    let mut rec = [0u8; STRIDE];

    while pos < total_rows {
        let mut min_left = u32::MAX;
        let mut all_payload = true;
        for lane in &lanes {
            if lane.phase != 3 || lane.left < 2 {
                all_payload = false;
                break;
            }
            if lane.left < min_left {
                min_left = lane.left;
            }
        }
        if all_payload {
            let avail_rounds = (src.len() - src_pos) / ROUND_BYTES;
            let fast_rounds = (min_left - 1)
                .min(avail_rounds as u32)
                .min((total_rows - pos) as u32);
            if fast_rounds > 0 {
                // Models cannot change while all lanes stay in payload phase
                // with `left >= 2`, so resolve each lane's `&Model` once and
                // track per-lane flags in bitmasks to decouple independent rANS
                // steps from the 8-byte fetch schedule.
                let lane_models: [&Model; N] =
                    std::array::from_fn(|j| &t.models[usize::from(lanes[j].model & 0x7f)]);
                let mut x_arr = [0u32; N];
                let mut left_arr = [0u32; N];
                let mut latch_arr = [0u8; N];
                let mut out_arr = [0u8; N];
                let mut need_mask = 0u32;
                let mut has_latch_mask = 0u32;
                let mut has_out_mask = 0u32;
                let mut stalled_mask = 0u32;
                for (j, lane) in lanes.iter().enumerate() {
                    let bit = 1u32 << j;
                    x_arr[j] = lane.x;
                    left_arr[j] = lane.left;
                    if lane.latch != NONE {
                        latch_arr[j] = lane.latch as u8;
                        has_latch_mask |= bit;
                    }
                    if lane.out != NONE {
                        out_arr[j] = lane.out as u8;
                        has_out_mask |= bit;
                    }
                    if lane.need {
                        need_mask |= bit;
                    }
                    if lane.stalled {
                        stalled_mask |= bit;
                    }
                }
                let mut err: Option<Error> = None;
                for _ in 0..fast_rounds {
                    let chunk = &src[src_pos..src_pos + ROUND_BYTES];
                    // Pass 1: step all unstalled lanes independently (no `taken`
                    // dependency between lanes, exposing ILP across rANS decodes).
                    let active_orig = full & !stalled_mask;
                    if stalled_mask == 0 {
                        for j in 0..N {
                            let bit = 1u32 << j;
                            let mut x = x_arr[j];
                            if x < LOW && (has_latch_mask & bit) != 0 {
                                left_arr[j] -= 1;
                                x = (x << 8) | u32::from(latch_arr[j]);
                                has_latch_mask &= !bit;
                            }
                            if x > LOW {
                                let (sym, next_x) = lane_models[j].decode_slot(x);
                                x_arr[j] = next_x;
                                out_arr[j] = sym;
                                has_out_mask |= bit;
                            } else {
                                x_arr[j] = x;
                            }
                        }
                    } else {
                        let mut active = active_orig;
                        while active != 0 {
                            let j = active.trailing_zeros() as usize;
                            let bit = 1u32 << j;
                            active &= active - 1;
                            let mut x = x_arr[j];
                            if x < LOW && (has_latch_mask & bit) != 0 {
                                left_arr[j] -= 1;
                                x = (x << 8) | u32::from(latch_arr[j]);
                                has_latch_mask &= !bit;
                            }
                            if x > LOW {
                                let (sym, next_x) = lane_models[j].decode_slot(x);
                                x_arr[j] = next_x;
                                out_arr[j] = sym;
                                has_out_mask |= bit;
                            } else {
                                x_arr[j] = x;
                            }
                        }
                    }
                    // For every lane stepped in Pass 1 (`active_orig`), `left >= 1`
                    // holds so `need == !has_latch`; stalled lanes keep their `need`.
                    need_mask = (need_mask & stalled_mask) | (active_orig & !has_latch_mask);

                    // Pass 2: assign up to ROUND_BYTES bytes to lanes needing input
                    // in lane order; remaining wanting lanes stall.
                    let waiting_mask = has_out_mask & done;
                    let mut want = need_mask & !waiting_mask;
                    let mut taken = 0usize;
                    while want != 0 && taken < ROUND_BYTES {
                        let j = want.trailing_zeros() as usize;
                        let bit = 1u32 << j;
                        want &= want - 1;
                        latch_arr[j] = chunk[taken];
                        has_latch_mask |= bit;
                        taken += 1;
                    }
                    src_pos += taken;
                    stalled_mask = waiting_mask | want;

                    // Pass 3: emit symbols from all ready unstalled lanes.
                    let mut emit = has_out_mask & !stalled_mask;
                    let mut moved = emit != 0;
                    has_out_mask &= !emit;
                    done |= emit;
                    if emit == full {
                        for (j, &sym) in out_arr.iter().enumerate() {
                            rec[j * PER + sub] = sym;
                        }
                    } else {
                        while emit != 0 {
                            let j = emit.trailing_zeros() as usize;
                            emit &= emit - 1;
                            rec[j * PER + sub] = out_arr[j];
                        }
                    }

                    if done == full {
                        pos += 1;
                        done = 0;
                        moved = true;
                        if PER == 1 {
                            if let Err(e) = on_block(block_idx, &rec) {
                                err = Some(e);
                                break;
                            }
                            block_idx += 1;
                        } else if sub == 0 {
                            sub = 1;
                        } else {
                            if let Err(e) = on_block(block_idx, &rec) {
                                err = Some(e);
                                break;
                            }
                            block_idx += 1;
                            sub = 0;
                        }
                    }
                    idle = if taken > 0 || moved { 0 } else { idle + 1 };
                    if idle > 3 {
                        err = Some(Error("the stream ends early or is corrupt"));
                        break;
                    }
                }
                if let Some(e) = err {
                    return Err(e);
                }
                for (j, lane) in lanes.iter_mut().enumerate() {
                    let bit = 1u32 << j;
                    lane.x = x_arr[j];
                    lane.left = left_arr[j];
                    lane.latch = if has_latch_mask & bit != 0 {
                        u16::from(latch_arr[j])
                    } else {
                        NONE
                    };
                    lane.out = if has_out_mask & bit != 0 {
                        u16::from(out_arr[j])
                    } else {
                        NONE
                    };
                    lane.need = need_mask & bit != 0;
                    lane.stalled = stalled_mask & bit != 0;
                }
                continue;
            }
        }

        let (mut taken, mut moved) = (0usize, false);
        for (j, lane) in lanes.iter_mut().enumerate() {
            let bit = 1u32 << j;
            let before = taken;
            if !lane.stalled {
                lane.step(t);
            }
            let waiting = lane.out != NONE && done & bit != 0;
            if lane.need && !waiting && taken < ROUND_BYTES && src_pos < src.len() {
                lane.latch = u16::from(src[src_pos]);
                src_pos += 1;
                taken += 1;
            }
            lane.stalled = (lane.need && before >= ROUND_BYTES) || waiting;
            if !lane.stalled && lane.out != NONE {
                rec[j * PER + sub] = lane.out as u8;
                lane.out = NONE;
                done |= bit;
                moved = true;
            }
        }
        if done == full {
            pos += 1;
            done = 0;
            moved = true;
            if PER == 1 {
                on_block(block_idx, &rec)?;
                block_idx += 1;
            } else if sub == 0 {
                sub = 1;
            } else {
                on_block(block_idx, &rec)?;
                block_idx += 1;
                sub = 0;
            }
        }
        idle = if taken > 0 || moved { 0 } else { idle + 1 };
        if idle > 3 {
            return Err(Error("the stream ends early or is corrupt"));
        }
    }
    Ok(())
}

/// Decodes `per` symbols per block on every lane into `records`, taking input
/// from `fetch` in schedule order. Used by the synthetic test encoder.
#[cfg(test)]
fn lanes(
    t: &Tables,
    widths: &[u8],
    per: usize,
    blocks: usize,
    mut fetch: impl FnMut(usize) -> Option<u8>,
    records: &mut [u8],
) -> Result<(), Error> {
    let mut lanes = [Lane::default(); 16];
    let lanes = &mut lanes[..widths.len()];
    for (lane, &width) in lanes.iter_mut().zip(widths) {
        lane.width = width;
        lane.need = true;
    }
    let stride = widths.len() * per;
    let full = (1u32 << widths.len()) - 1;
    let (mut pos, mut done, mut idle) = (0, 0u32, 0);
    let mut at = 0;
    while pos < blocks * per {
        let (mut taken, mut moved) = (0, false);
        for (j, lane) in lanes.iter_mut().enumerate() {
            let bit = 1 << j;
            let before = taken;
            if !lane.stalled {
                lane.step(t);
            }
            let waiting = lane.out != NONE && done & bit != 0;
            if lane.need
                && !waiting
                && taken < ROUND_BYTES
                && let Some(b) = fetch(j)
            {
                lane.latch = u16::from(b);
                taken += 1;
            }
            lane.stalled = (lane.need && before >= ROUND_BYTES) || waiting;
            if !lane.stalled && lane.out != NONE {
                records[at + j * per] = lane.out as u8;
                lane.out = NONE;
                done |= bit;
                moved = true;
            }
        }
        if done == full {
            pos += 1;
            at += if pos.is_multiple_of(per) {
                stride - per + 1
            } else {
                1
            };
            done = 0;
            moved = true;
        }
        idle = if taken > 0 || moved { 0 } else { idle + 1 };
        if idle > 3 {
            return Err(Error("the stream ends early or is corrupt"));
        }
    }
    Ok(())
}

// Blocks.

/// Spreads 4 bits onto the even bit positions of a byte.
const SPREAD4: [u8; 16] = {
    let mut t = [0u8; 16];
    let mut v = 0usize;
    while v < 16 {
        t[v] = ((v & 1) | ((v & 2) << 1) | ((v & 4) << 2) | ((v & 8) << 3)) as u8;
        v += 1;
    }
    t
};

/// Spreads 8 bits onto the even bit positions of a `u16`.
const SPREAD8: [u16; 256] = {
    let mut t = [0u16; 256];
    let mut v = 0usize;
    while v < 256 {
        t[v] = (SPREAD4[v & 15] as u16) | ((SPREAD4[v >> 4] as u16) << 8);
        v += 1;
    }
    t
};

/// Precomputed `[even, odd]` zigzag-decoded 4-bit residuals for every byte.
const NIBBLES_EVEN_FIRST: [[i16; 2]; 256] = {
    const fn half(v: u8) -> u8 {
        (v & 1) | ((v >> 1) & 2) | ((v >> 2) & 4) | ((v >> 3) & 8)
    }
    const fn z(u: u8) -> i16 {
        (u >> 1) as i16 ^ -((u & 1) as i16)
    }
    let mut t = [[0i16; 2]; 256];
    let mut b = 0usize;
    while b < 256 {
        let byte = b as u8;
        t[b] = [z(half(byte)), z(half(byte >> 1))];
        b += 1;
    }
    t
};

/// The record's endpoint and index references.
#[inline(always)]
fn references<const PAIRED: bool>(s: &[u8]) -> (usize, usize) {
    let (d, ix) = if PAIRED {
        (interleave(s[0], s[1]), interleave(s[2], s[3]))
    } else {
        (s[0], s[1])
    };
    let delta = (d >> 1) ^ (d & 1).wrapping_neg();
    (usize::from(ix.wrapping_add(delta)), usize::from(ix))
}

/// Two 4-bit symbols, the first on the even bits.
#[inline(always)]
fn interleave(a: u8, b: u8) -> u8 {
    SPREAD4[usize::from(a & 15)] | (SPREAD4[usize::from(b & 15)] << 1)
}

/// Block `i`'s reference `r`: 0 none, else `r` blocks back, before the first
/// block into the dictionary.
#[inline(always)]
fn pick<'a, T>(i: usize, r: usize, history: &'a [T], dictionary: &'a [T], zero: &'a T) -> &'a T {
    match r {
        0 => zero,
        r if r <= i => &history[(i - r) & (HISTORY - 1)],
        r => &dictionary[r - i - 1],
    }
}

/// Zigzag residual (0, -1, 1, -2, ...) modulo 2^bits.
#[inline(always)]
fn zz(v: u8, bits: u32) -> u8 {
    let mask = ((1u32 << bits) - 1) as u8;
    ((v >> 1) ^ if v & 1 != 0 { mask } else { 0 }) & mask
}

/// Precomputed element bit-masks of the plain formats' endpoints and indices.
struct Masks {
    ep: [u8; 8],
    ix: [u8; 32],
}

const fn masks(ep_bits: [u8; 8], ix3: usize, ix2: usize) -> Masks {
    let mut ep = [0u8; 8];
    let mut k = 0;
    while k < 8 {
        ep[k] = if ep_bits[k] == 0 {
            0
        } else {
            ((1u16 << ep_bits[k]) - 1) as u8
        };
        k += 1;
    }
    let mut ix = [0u8; 32];
    let mut k = 0;
    while k < ix3 + ix2 {
        ix[k] = if k < ix3 { 7 } else { 3 };
        k += 1;
    }
    Masks { ep, ix }
}

const BC1: Masks = masks([5, 6, 5, 5, 6, 5, 0, 0], 0, 16);
const BC3: Masks = masks([5, 6, 5, 5, 6, 5, 8, 8], 16, 16);
const BC4: Masks = masks([8, 8, 0, 0, 0, 0, 0, 0], 16, 0);
const BC5: Masks = masks([8, 8, 8, 8, 0, 0, 0, 0], 32, 0);

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn plain_block<const EP: usize, const IX: usize, const PAIRED: bool>(
    i: usize,
    s: &[u8],
    block: &mut [u8],
    history: &mut [Plain; HISTORY],
    dictionary: &[Plain],
    zero: &Plain,
    m: &Masks,
    residual: impl FnOnce(&[u8]) -> Plain,
    pack: fn(&Plain, &mut [u8]),
) {
    let (ep_ref, ix_ref) = references::<PAIRED>(s);
    let ep_ref = &pick(i, ep_ref, history, dictionary, zero).ep;
    let ix_ref = &pick(i, ix_ref, history, dictionary, zero).ix;
    let mut v = residual(s);
    for ((v, &r), &mask) in v.ep[..EP].iter_mut().zip(&ep_ref[..EP]).zip(&m.ep[..EP]) {
        *v = v.wrapping_add(r) & mask;
    }
    for ((v, &r), &mask) in v.ix[..IX].iter_mut().zip(&ix_ref[..IX]).zip(&m.ix[..IX]) {
        *v = v.wrapping_add(r) & mask;
    }
    history[i & (HISTORY - 1)] = v;
    pack(&v, block);
}

/// 2-bit index residuals: one symbol per 2x2 quad.
fn quads(rank: &[u8; 256], s: &[u8], ix: &mut [u8]) {
    for (q, &sym) in s.iter().enumerate() {
        let v = rank[usize::from(sym)];
        let base = (q >> 1) * 8 + (q & 1) * 2;
        for (k, p) in [0, 1, 4, 5].into_iter().enumerate() {
            ix[base + p] = v >> (2 * k) & 3;
        }
    }
}

/// 3-bit index residuals: one symbol per two pixels in raster order.
fn pairs(rank: &[u8; 64], s: &[u8], ix: &mut [u8]) {
    for (k, &sym) in s.iter().enumerate() {
        let v = rank[usize::from(sym & 63)];
        ix[2 * k] = v & 7;
        ix[2 * k + 1] = v >> 4 & 7;
    }
}

fn bc1_residual(t: &Tables, s: &[u8]) -> Plain {
    let mut v = Plain::default();
    for (k, bits) in [5, 6, 5, 5, 6, 5].into_iter().enumerate() {
        v.ep[k] = zz(s[2 + k], bits);
    }
    quads(&t.rank1, &s[8..12], &mut v.ix);
    v
}

fn bc3_residual(t: &Tables, s: &[u8]) -> Plain {
    // Colour lanes carry each channel's two endpoints together.
    let mut v = Plain {
        ep: [
            zz(s[14], 5),
            zz(s[16], 6),
            zz(s[18], 5),
            zz(s[15], 5),
            zz(s[17], 6),
            zz(s[19], 5),
            zz(s[4], 8),
            zz(s[5], 8),
        ],
        ..Default::default()
    };
    pairs(&t.rank3a, &s[6..14], &mut v.ix[..16]);
    quads(&t.rank3c, &s[20..24], &mut v.ix[16..]);
    v
}

fn bc4_residual(t: &Tables, s: &[u8]) -> Plain {
    let mut v = Plain::default();
    v.ep[..2].copy_from_slice(&[zz(s[2], 8), zz(s[3], 8)]);
    pairs(&t.rank4, &s[4..12], &mut v.ix[..16]);
    v
}

fn bc5_residual(t: &Tables, s: &[u8]) -> Plain {
    let mut v = Plain::default();
    for k in 0..4 {
        v.ep[k] = zz(s[4 + k], 8);
    }
    pairs(&t.rank5, &s[8..24], &mut v.ix);
    v
}

/// Indices along the endpoint line -> BC1 and BC4 codes.
const BC1_CODE: [u32; 4] = [0, 2, 3, 1];
const BC4_CODE: [u64; 8] = [0, 2, 3, 4, 5, 6, 7, 1];

fn bc1_color(ep: &[u8], ix: &[u8], out: &mut [u8]) {
    let c0 = u16::from(ep[0]) << 11 | u16::from(ep[1]) << 5 | u16::from(ep[2]);
    let c1 = u16::from(ep[3]) << 11 | u16::from(ep[4]) << 5 | u16::from(ep[5]);
    let bits = (0..16).fold(0, |acc, p| {
        acc | BC1_CODE[usize::from(ix[p] & 3)] << (2 * p)
    });
    out[..2].copy_from_slice(&c0.to_le_bytes());
    out[2..4].copy_from_slice(&c1.to_le_bytes());
    out[4..8].copy_from_slice(&bits.to_le_bytes());
}

/// BC4 half: endpoints are kept 128 below their stored value.
fn bc4_alpha(e0: u8, e1: u8, ix: &[u8], out: &mut [u8]) {
    let bits = (0..16).fold(0u64, |acc, p| {
        acc | BC4_CODE[usize::from(ix[p] & 7)] << (16 + 3 * p)
    });
    let v = u64::from(e0 ^ 0x80) | u64::from(e1 ^ 0x80) << 8 | bits;
    out[..8].copy_from_slice(&v.to_le_bytes());
}

fn bc1_pack(v: &Plain, out: &mut [u8]) {
    bc1_color(&v.ep, &v.ix, out);
}

fn bc3_pack(v: &Plain, out: &mut [u8]) {
    bc4_alpha(v.ep[6], v.ep[7], &v.ix[..16], &mut out[..8]);
    bc1_color(&v.ep, &v.ix[16..], &mut out[8..]);
}

fn bc4_pack(v: &Plain, out: &mut [u8]) {
    bc4_alpha(v.ep[0], v.ep[1], &v.ix, out);
}

fn bc5_pack(v: &Plain, out: &mut [u8]) {
    bc4_alpha(v.ep[0], v.ep[1], &v.ix[..16], &mut out[..8]);
    bc4_alpha(v.ep[2], v.ep[3], &v.ix[16..], &mut out[8..]);
}

/// A byte's even and odd bits as two zigzag-coded 4-bit residuals.
#[inline(always)]
fn nibbles(b: u8, odd_first: bool) -> [i16; 2] {
    let [even, odd] = NIBBLES_EVEN_FIRST[usize::from(b)];
    if odd_first { [odd, even] } else { [even, odd] }
}

/// Adds a 16-pixel index set with pre-hoisted shifts (`sh = 16 - cur_bits`,
/// `rsh = 16 - ref_bits` when `ref_bits > 0`).
#[inline(always)]
fn add_index_shifted(cur: i16, sh: u8, refv: i16, rsh: Option<u8>) -> i16 {
    let up_cur = (cur as u16) << sh;
    let up_ref = match rsh {
        Some(rsh) => (refv as u16) << rsh,
        None => 0,
    };
    (up_cur.wrapping_add(up_ref) as i16) >> sh
}

/// Little-endian bit writer for one 128-bit block.
#[derive(Default)]
struct Bits {
    v: u128,
    at: u32,
}

impl Bits {
    fn put(&mut self, value: u64, n: u32) {
        if n > 0 && self.at < 128 {
            self.v |= (u128::from(value) & ((1u128 << n) - 1)) << self.at;
        }
        self.at += n;
    }
}

// BC7.

/// Public BC7 anchor indices: second subset of two; second and third of three.
const ANCHOR2: [u8; 64] = [
    15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 2, 8, 2, 2, 8, 8, 15, 2, 8,
    2, 2, 8, 8, 2, 2, 15, 15, 6, 8, 2, 8, 15, 15, 2, 8, 2, 2, 2, 15, 15, 6, 6, 2, 6, 8, 15, 15, 2,
    2, 15, 15, 15, 15, 15, 2, 2, 15,
];
const ANCHOR3A: [u8; 64] = [
    3, 3, 15, 15, 8, 3, 15, 15, 8, 8, 6, 6, 6, 5, 3, 3, 3, 3, 8, 15, 3, 3, 6, 10, 5, 8, 8, 6, 8, 5,
    15, 15, 8, 15, 3, 5, 6, 10, 8, 15, 15, 3, 15, 5, 15, 15, 15, 15, 3, 15, 5, 5, 5, 8, 5, 10, 5,
    10, 8, 13, 15, 12, 3, 3,
];
const ANCHOR3B: [u8; 64] = [
    15, 8, 8, 3, 15, 15, 3, 8, 15, 15, 15, 15, 15, 15, 15, 8, 15, 8, 15, 3, 15, 8, 15, 8, 3, 15, 6,
    10, 15, 15, 10, 8, 15, 3, 15, 10, 10, 8, 9, 10, 6, 15, 8, 15, 3, 6, 6, 8, 15, 3, 15, 15, 15,
    15, 15, 15, 15, 15, 15, 15, 3, 15, 15, 8,
];

/// Endpoint bits of R G B A per mode (public), before p-bits and rotation.
const BC7_BITS: [[u8; 4]; 8] = [
    [4, 4, 4, 0],
    [6, 6, 6, 0],
    [5, 5, 5, 0],
    [7, 7, 7, 0],
    [5, 5, 5, 6],
    [7, 7, 7, 8],
    [7, 7, 7, 7],
    [5, 5, 5, 5],
];
/// Index bits of the primary and secondary sets per mode.
const BC7_INDEX: [[u8; 2]; 8] = [
    [3, 0],
    [3, 0],
    [2, 0],
    [2, 0],
    [3, 2],
    [2, 2],
    [4, 0],
    [2, 0],
];

fn pbits(mode: u16) -> bool {
    matches!(mode, 0 | 1 | 3 | 6 | 7)
}

/// Channel precisions: the p-bit is red's lowest bit; rotation swaps alpha
/// with the rotated channel.
fn bc7_precision(ctrl: u16) -> [u8; 4] {
    let (mode, rot) = (ctrl & 7, usize::from(ctrl >> 3 & 3));
    let mut p = BC7_BITS[usize::from(mode)];
    if rot != 0 {
        p.swap(3, rot - 1);
    } else if pbits(mode) {
        p[0] += 1;
    }
    p
}

#[inline]
fn bc7_block(
    i: usize,
    s: &[u8; 32],
    block: &mut [u8],
    history: &mut [(Bc7Ep, Bc7Ix); HISTORY],
    t: &Tables,
    zero: &(Bc7Ep, Bc7Ix),
) -> Result<(), Error> {
    let (ep_ref, ix_ref) = references::<true>(s);
    let ctrl = t.ctrl7[usize::from(s[31] & 15) << 5 | usize::from(s[30] & 31)];
    if ctrl == 0 {
        return Err(Error("unused BC7 control code"));
    }
    let mode = usize::from(ctrl & 7);
    let subsets = usize::from((ctrl & 0xff) >> 6);
    let prec = bc7_precision(ctrl);
    let three = subsets == 3;
    let or0 = |v: u8| if three { 0 } else { v };
    let if3 = |v: u8| if three { v } else { 0 };
    let stored = [
        [s[4], s[5], s[6], s[7], s[8], s[9], or0(s[10]), or0(s[11])],
        [
            s[12],
            s[13],
            s[14],
            s[15],
            s[16],
            s[17],
            or0(s[18]),
            or0(s[19]),
        ],
        [
            s[20],
            s[21],
            if3(s[10]),
            if3(s[11]),
            if3(s[18]),
            if3(s[19]),
            0,
            0,
        ],
    ];
    let r = &pick(i, ep_ref, history, &t.bc7, zero).0;
    let mut ep = Bc7Ep {
        ctrl,
        ..Default::default()
    };
    // Rows the reference lacks build on its first row.
    let common = subsets.min(usize::from((r.ctrl & 0xff) >> 6));
    let columns = if prec[3] > 0 { 8 } else { 6 };
    for c in 0..columns {
        let p = prec[c / 2];
        if p == 0 {
            continue;
        }
        let sh = 16 - p;
        let mask = 0xffffu16 << sh;
        for (row, st) in stored.iter().enumerate().take(subsets) {
            let v = st[c];
            let residual = (u16::from(v >> 1) ^ (v & 1).wrapping_neg() as i8 as u16) << sh;
            let base = r.rows[if row < common { row } else { 0 }][c] & mask;
            ep.rows[row][c] = base.wrapping_add(residual);
        }
    }
    let bits = BC7_INDEX[mode];
    let rx = &pick(i, ix_ref, history, &t.bc7, zero).1;
    let mut ix = Bc7Ix {
        bits,
        ..Default::default()
    };
    for (set, offsets) in [[22, 24, 26, 28], [12, 14, 16, 20]].into_iter().enumerate() {
        let cur_bits = bits[set];
        if cur_bits == 0 {
            continue;
        }
        let sh = 16 - cur_bits;
        let rbits = rx.bits[set];
        let rsh = (rbits > 0 && rbits <= 16).then(|| 16 - rbits);
        for (k, o) in offsets.into_iter().enumerate() {
            let odd_first = k < 3;
            let p0 = nibbles(s[o], odd_first);
            let p1 = nibbles(s[o + 1], odd_first);
            let base_p = 4 * k;
            ix.ix[set][base_p] = add_index_shifted(p0[0], sh, rx.ix[set][base_p], rsh);
            ix.ix[set][base_p + 1] = add_index_shifted(p0[1], sh, rx.ix[set][base_p + 1], rsh);
            ix.ix[set][base_p + 2] = add_index_shifted(p1[0], sh, rx.ix[set][base_p + 2], rsh);
            ix.ix[set][base_p + 3] = add_index_shifted(p1[1], sh, rx.ix[set][base_p + 3], rsh);
        }
    }
    history[i & (HISTORY - 1)] = (ep, ix);
    bc7_pack(&ep, &ix, prec, block);
    Ok(())
}

#[inline]
fn bc7_pack(ep: &Bc7Ep, ix: &Bc7Ix, prec: [u8; 4], out: &mut [u8]) {
    let ctrl = ep.ctrl;
    let mode = ctrl & 7;
    let subsets = usize::from((ctrl & 0xff) >> 6);
    let part = usize::from(ctrl >> 8 & 63);
    let rot = usize::from(ctrl >> 3 & 3);
    let mut w = Bits::default();
    w.put(1 << mode, u32::from(mode) + 1);
    match mode {
        0 => w.put(part as u64, 4),
        1 | 2 | 3 | 7 => w.put(part as u64, 6),
        4 => {
            w.put(rot as u64, 2);
            w.put(u64::from(ctrl >> 5 & 1), 1);
        }
        5 => w.put(rot as u64, 2),
        _ => {}
    }
    let ends = 2 * subsets;
    let field = |row: usize, col: usize, p: u8| u64::from(ep.rows[row][col] >> (16 - p));
    let mut order = [0, 1, 2, 3];
    if rot != 0 {
        order.swap(3, rot - 1);
    }
    for ch in order {
        let p = prec[ch];
        if p == 0 {
            continue;
        }
        let drop = u32::from(ch == 0 && pbits(mode));
        for e in 0..ends {
            w.put(field(e / 2, 2 * ch + e % 2, p) >> drop, u32::from(p) - drop);
        }
    }
    match mode {
        1 => (0..subsets).for_each(|row| w.put(field(row, 0, prec[0]) & 1, 1)),
        m if pbits(m) => (0..ends).for_each(|e| w.put(field(e / 2, e % 2, prec[0]) & 1, 1)),
        _ => {}
    }
    let anchor_mask: u16 = match subsets {
        2 => 1u16 | (1u16 << ANCHOR2[part]),
        3 => 1u16 | (1u16 << ANCHOR3A[part]) | (1u16 << ANCHOR3B[part]),
        _ => 1u16,
    };
    let mut indices = |set: usize| {
        let bits = u32::from(ix.bits[set]);
        for (p, &v) in ix.ix[set].iter().enumerate() {
            let n = bits - u32::from((anchor_mask >> p) & 1);
            w.put(u64::from(v as u16), n);
        }
    };
    match mode {
        4 => {
            indices(1);
            indices(0);
        }
        5 => {
            indices(0);
            indices(1);
        }
        _ => indices(0),
    }
    out.copy_from_slice(&w.v.to_le_bytes());
}

// BC6H.

/// Bits of endpoint 0, then of the other endpoints' R, G, B, per mode 1-14.
const BC6_BITS: [[u8; 4]; 15] = [
    [0; 4],
    [10, 5, 5, 5],
    [7, 6, 6, 6],
    [11, 5, 4, 4],
    [11, 4, 5, 4],
    [11, 4, 4, 5],
    [9, 5, 5, 5],
    [8, 6, 5, 5],
    [8, 5, 6, 5],
    [8, 5, 5, 6],
    [6, 6, 6, 6],
    [10, 10, 10, 10],
    [11, 9, 9, 9],
    [12, 8, 8, 8],
    [16, 4, 4, 4],
];
/// Mode code and its width, modes 1-14.
const BC6_CODE: [(u64, u32); 15] = [
    (0, 0),
    (0, 2),
    (1, 2),
    (2, 5),
    (6, 5),
    (10, 5),
    (14, 5),
    (18, 5),
    (22, 5),
    (26, 5),
    (30, 5),
    (3, 5),
    (7, 5),
    (11, 5),
    (15, 5),
];

/// A run of the public BC6H layout: `len` bits of channel `ch` (0-2; 3 is
/// the partition) of endpoint `ep`, from bit `first` up.
#[derive(Clone, Copy)]
struct Run {
    ch: u8,
    ep: u8,
    first: u8,
    len: u8,
}

const fn r(ch: u8, ep: u8, first: u8, len: u8) -> Run {
    Run { ch, ep, first, len }
}

const R: u8 = 0;
const G: u8 = 1;
const B: u8 = 2;
const D: Run = r(3, 0, 0, 5);

/// The header after the mode bits, per mode 1-14. Modes 13 and 14 write the
/// high base bits in ascending order (what the console's packer does).
#[rustfmt::skip]
const BC6_LAYOUT: [&[Run]; 15] = [
    &[],
    &[
        r(G, 2, 4, 1), r(B, 2, 4, 1), r(B, 3, 4, 1), r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10),
        r(R, 1, 0, 5), r(G, 3, 4, 1), r(G, 2, 0, 4), r(G, 1, 0, 5), r(B, 3, 0, 1), r(G, 3, 0, 4),
        r(B, 1, 0, 5), r(B, 3, 1, 1), r(B, 2, 0, 4), r(R, 2, 0, 5), r(B, 3, 2, 1), r(R, 3, 0, 5),
        r(B, 3, 3, 1), D,
    ],
    &[
        r(G, 2, 5, 1), r(G, 3, 4, 1), r(G, 3, 5, 1), r(R, 0, 0, 7), r(B, 3, 0, 1), r(B, 3, 1, 1),
        r(B, 2, 4, 1), r(G, 0, 0, 7), r(B, 2, 5, 1), r(B, 3, 2, 1), r(G, 2, 4, 1), r(B, 0, 0, 7),
        r(B, 3, 3, 1), r(B, 3, 5, 1), r(B, 3, 4, 1), r(R, 1, 0, 6), r(G, 2, 0, 4), r(G, 1, 0, 6),
        r(G, 3, 0, 4), r(B, 1, 0, 6), r(B, 2, 0, 4), r(R, 2, 0, 6), r(R, 3, 0, 6), D,
    ],
    &[
        r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10), r(R, 1, 0, 5), r(R, 0, 10, 1), r(G, 2, 0, 4),
        r(G, 1, 0, 4), r(G, 0, 10, 1), r(B, 3, 0, 1), r(G, 3, 0, 4), r(B, 1, 0, 4), r(B, 0, 10, 1),
        r(B, 3, 1, 1), r(B, 2, 0, 4), r(R, 2, 0, 5), r(B, 3, 2, 1), r(R, 3, 0, 5), r(B, 3, 3, 1), D,
    ],
    &[
        r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10), r(R, 1, 0, 4), r(R, 0, 10, 1), r(G, 3, 4, 1),
        r(G, 2, 0, 4), r(G, 1, 0, 5), r(G, 0, 10, 1), r(G, 3, 0, 4), r(B, 1, 0, 4), r(B, 0, 10, 1),
        r(B, 3, 1, 1), r(B, 2, 0, 4), r(R, 2, 0, 4), r(B, 3, 0, 1), r(B, 3, 2, 1), r(R, 3, 0, 4),
        r(G, 2, 4, 1), r(B, 3, 3, 1), D,
    ],
    &[
        r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10), r(R, 1, 0, 4), r(R, 0, 10, 1), r(B, 2, 4, 1),
        r(G, 2, 0, 4), r(G, 1, 0, 4), r(G, 0, 10, 1), r(B, 3, 0, 1), r(G, 3, 0, 4), r(B, 1, 0, 5),
        r(B, 0, 10, 1), r(B, 2, 0, 4), r(R, 2, 0, 4), r(B, 3, 1, 1), r(B, 3, 2, 1), r(R, 3, 0, 4),
        r(B, 3, 4, 1), r(B, 3, 3, 1), D,
    ],
    &[
        r(R, 0, 0, 9), r(B, 2, 4, 1), r(G, 0, 0, 9), r(G, 2, 4, 1), r(B, 0, 0, 9), r(B, 3, 4, 1),
        r(R, 1, 0, 5), r(G, 3, 4, 1), r(G, 2, 0, 4), r(G, 1, 0, 5), r(B, 3, 0, 1), r(G, 3, 0, 4),
        r(B, 1, 0, 5), r(B, 3, 1, 1), r(B, 2, 0, 4), r(R, 2, 0, 5), r(B, 3, 2, 1), r(R, 3, 0, 5),
        r(B, 3, 3, 1), D,
    ],
    &[
        r(R, 0, 0, 8), r(G, 3, 4, 1), r(B, 2, 4, 1), r(G, 0, 0, 8), r(B, 3, 2, 1), r(G, 2, 4, 1),
        r(B, 0, 0, 8), r(B, 3, 3, 1), r(B, 3, 4, 1), r(R, 1, 0, 6), r(G, 2, 0, 4), r(G, 1, 0, 5),
        r(B, 3, 0, 1), r(G, 3, 0, 4), r(B, 1, 0, 5), r(B, 3, 1, 1), r(B, 2, 0, 4), r(R, 2, 0, 6),
        r(R, 3, 0, 6), D,
    ],
    &[
        r(R, 0, 0, 8), r(B, 3, 0, 1), r(B, 2, 4, 1), r(G, 0, 0, 8), r(G, 2, 5, 1), r(G, 2, 4, 1),
        r(B, 0, 0, 8), r(G, 3, 5, 1), r(B, 3, 4, 1), r(R, 1, 0, 5), r(G, 3, 4, 1), r(G, 2, 0, 4),
        r(G, 1, 0, 6), r(G, 3, 0, 4), r(B, 1, 0, 5), r(B, 3, 1, 1), r(B, 2, 0, 4), r(R, 2, 0, 5),
        r(B, 3, 2, 1), r(R, 3, 0, 5), r(B, 3, 3, 1), D,
    ],
    &[
        r(R, 0, 0, 8), r(B, 3, 1, 1), r(B, 2, 4, 1), r(G, 0, 0, 8), r(B, 2, 5, 1), r(G, 2, 4, 1),
        r(B, 0, 0, 8), r(B, 3, 5, 1), r(B, 3, 4, 1), r(R, 1, 0, 5), r(G, 3, 4, 1), r(G, 2, 0, 4),
        r(G, 1, 0, 5), r(B, 3, 0, 1), r(G, 3, 0, 4), r(B, 1, 0, 6), r(B, 2, 0, 4), r(R, 2, 0, 5),
        r(B, 3, 2, 1), r(R, 3, 0, 5), r(B, 3, 3, 1), D,
    ],
    &[
        r(R, 0, 0, 6), r(G, 3, 4, 1), r(B, 3, 0, 1), r(B, 3, 1, 1), r(B, 2, 4, 1), r(G, 0, 0, 6),
        r(G, 2, 5, 1), r(B, 2, 5, 1), r(B, 3, 2, 1), r(G, 2, 4, 1), r(B, 0, 0, 6), r(G, 3, 5, 1),
        r(B, 3, 3, 1), r(B, 3, 5, 1), r(B, 3, 4, 1), r(R, 1, 0, 6), r(G, 2, 0, 4), r(G, 1, 0, 6),
        r(G, 3, 0, 4), r(B, 1, 0, 6), r(B, 2, 0, 4), r(R, 2, 0, 6), r(R, 3, 0, 6), D,
    ],
    &[r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10), r(R, 1, 0, 10), r(G, 1, 0, 10), r(B, 1, 0, 10)],
    &[
        r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10), r(R, 1, 0, 9), r(R, 0, 10, 1), r(G, 1, 0, 9),
        r(G, 0, 10, 1), r(B, 1, 0, 9), r(B, 0, 10, 1),
    ],
    &[
        r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10), r(R, 1, 0, 8), r(R, 0, 10, 2), r(G, 1, 0, 8),
        r(G, 0, 10, 2), r(B, 1, 0, 8), r(B, 0, 10, 2),
    ],
    &[
        r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10), r(R, 1, 0, 4), r(R, 0, 10, 6), r(G, 1, 0, 4),
        r(G, 0, 10, 6), r(B, 1, 0, 4), r(B, 0, 10, 6),
    ],
];

fn bc6_index_bits(mode: u8) -> u8 {
    match mode {
        1..=10 => 3,
        11..=14 => 4,
        _ => 0,
    }
}

#[inline]
fn bc6_block(
    i: usize,
    s: &[u8; 32],
    block: &mut [u8],
    history: &mut [(Bc6Ep, Bc6Ix); HISTORY],
    dictionary: &[(Bc6Ep, Bc6Ix)],
    zero: &(Bc6Ep, Bc6Ix),
) -> Result<(), Error> {
    let z = |v: u16| (v >> 1) ^ (v & 1).wrapping_neg();
    let wide = |a: u8, b: u8| SPREAD8[usize::from(a)] | (SPREAD8[usize::from(b)] << 1);
    let (ep_ref, ix_ref) = references::<true>(s);
    let mode = s[30].wrapping_add(1);
    if !(1..=14).contains(&mode) {
        return Err(Error("BC6H mode out of range"));
    }
    let two = mode < 11;
    let shift = BC6_BITS[usize::from(mode)].map(|b| 16 - u32::from(b));
    let e0 = [wide(s[4], s[5]), wide(s[6], s[7]), wide(s[8], s[9])].map(z);
    let e1 = [wide(s[10], s[11]), wide(s[12], s[13]), wide(s[14], s[15])].map(z);
    let (e2, e3) = if two {
        (
            [s[16], s[18], s[20]].map(|v| z(v.into())),
            [s[17], s[19], s[21]].map(|v| z(v.into())),
        )
    } else {
        (e0, e1)
    };
    let r = &pick(i, ep_ref, history, dictionary, zero).0;
    let mut ep = Bc6Ep::default();
    // Endpoint 0 at the base precision, the others per channel; the
    // reference is cut by the mask after the sum.
    let mask = |sh: u32| 0xffffu16 << sh;
    for ((e, &r), res) in ep.e[0].iter_mut().zip(&r.e[0]).zip(e0) {
        *e = r.wrapping_add(res << shift[0]) & mask(shift[0]);
    }
    for (k, res) in [e1, e2, e3]
        .iter()
        .enumerate()
        .take(if two { 3 } else { 1 })
    {
        for c in 0..3 {
            ep.e[k + 1][c] =
                r.e[k + 1][c].wrapping_add(res[c] << shift[1 + c]) & mask(shift[1 + c]);
        }
    }
    if !two {
        for c in 0..3 {
            ep.e[2][c] = (ep.e[0][c] >> shift[0]) << shift[1];
        }
        ep.e[3] = ep.e[1];
    }
    let rx = &pick(i, ix_ref, history, dictionary, zero).1;
    let bits = bc6_index_bits(mode);
    let sh = 16 - bits;
    let rbits = bc6_index_bits(rx.mode);
    let rsh = (rbits > 0 && rbits <= 16).then(|| 16 - rbits);
    let mut ix = Bc6Ix {
        mode,
        ..Default::default()
    };
    for k in 0..8 {
        let pair = nibbles(s[22 + k], k < 6);
        let p = 2 * k;
        ix.ix[p] = add_index_shifted(pair[0], sh, rx.ix[p], rsh);
        ix.ix[p + 1] = add_index_shifted(pair[1], sh, rx.ix[p + 1], rsh);
    }
    history[i & (HISTORY - 1)] = (ep, ix);
    // Fields: the working value has its top bit flipped.
    let field = |k: usize, c: usize| {
        let sh = if k == 0 { shift[0] } else { shift[1 + c] };
        u64::from((ep.e[k][c] ^ 0x8000) >> sh)
    };
    let mut w = Bits::default();
    let (code, n) = BC6_CODE[usize::from(mode)];
    w.put(code, n);
    let part = s[31] & 31;
    for run in BC6_LAYOUT[usize::from(mode)] {
        if run.ch == 3 {
            w.put(part.into(), 5);
        } else {
            w.put(
                field(run.ep.into(), run.ch.into()) >> run.first,
                run.len.into(),
            );
        }
    }
    let anchor = if two { ANCHOR2[usize::from(part)] } else { 0 };
    for (p, &v) in ix.ix.iter().enumerate() {
        let short = p == 0 || p == usize::from(anchor);
        let v = (v as u16).wrapping_add(1 << (bits - 1));
        w.put(v.into(), u32::from(bits) - u32::from(short));
    }
    block.copy_from_slice(&w.v.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_shipped_tables_parse() {
        assert!(super::Tables::builtin().is_ok());
    }

    use super::*;

    /// Tables we made: uniform models, identity ranks, a BC7 code per mode,
    /// empty dictionaries.
    fn tables() -> Vec<u8> {
        let mut t = Vec::new();
        for m in 0..MODELS {
            let g = m / 16;
            for b in 0..BUCKETS {
                let f: u16 = if b < g + 2 { 1 << (15 - g) } else { 0 };
                t.extend_from_slice(&f.to_le_bytes());
            }
        }
        t.extend((0..=255u8).map(|v| v & 0x77)); // rank1 (unused by the tests beyond 2-bit fields)
        t.extend(0..64u8); // rank3a
        t.extend(0..=255u8); // rank3c
        t.extend((0..64u8).map(|v| (v & 7) | (v >> 3) << 4)); // rank4: low 3 bits, next 3 bits
        t.extend(0..64u8); // rank5
        for code in 0..512u16 {
            // Code c: mode 6 (one subset), the only one the tests use.
            let ctrl: u16 = if code == 0 { 0x46 } else { 0 };
            t.extend_from_slice(&ctrl.to_le_bytes());
        }
        t.resize(TABLES_SIZE, 0);
        t
    }

    /// A lane's symbols as one fragment: uniform model, rANS from 2^24, no
    /// reset, so the lane keeps 2^24 when it ends.
    fn fragment(width: u8, symbols: &[u8]) -> Vec<u8> {
        let f = 1u32 << (15 - width);
        let mut x = LOW;
        let mut bytes = Vec::new();
        for &s in symbols.iter().rev() {
            let (freq, start) = if s == 0 {
                (f - 1, 1)
            } else {
                (f, u32::from(s) * f)
            };
            while x >= (freq << 16) {
                bytes.push(x as u8);
                x >>= 8;
            }
            x = ((x / freq) << 16) + x % freq + start;
        }
        bytes.extend_from_slice(&x.to_le_bytes());
        bytes.reverse();
        let mut out = vec![0];
        if bytes.len() > 256 {
            out[0] |= 0x20;
            let n = bytes.len() - 257;
            out.extend_from_slice(&[n as u8, (n >> 8) as u8]);
        } else {
            out.push((bytes.len() - 1) as u8);
        }
        out.extend(bytes);
        out
    }

    /// Interleaves per-lane fragments the way the decoder takes bytes. A lane
    /// that has finished still asks: a filler fragment (two payload bytes)
    /// brings its state to 2^24 with a byte latched, which parks it for good.
    fn stream(t: &Tables, widths: &[u8], per: usize, records: &[Vec<u8>]) -> Vec<u8> {
        let stride = widths.len() * per;
        let mut queues: Vec<std::collections::VecDeque<u8>> = (0..widths.len())
            .map(|j| {
                let syms: Vec<u8> = records
                    .iter()
                    .flat_map(|r| r[j * per..j * per + per].to_vec())
                    .collect();
                let mut bytes = fragment(widths[j], &syms);
                bytes.extend([0, 1, 0, 0]);
                bytes.into()
            })
            .collect();
        let mut out = Vec::new();
        let mut check = vec![0; records.len() * stride];
        let fetch = |j: usize| {
            let b = queues[j]
                .pop_front()
                .expect("a parked lane asked for input");
            out.push(b);
            Some(b)
        };
        lanes(t, widths, per, records.len(), fetch, &mut check).unwrap();
        assert_eq!(check, records.concat());
        out
    }

    const BC4_LANES: [u8; 12] = [7, 7, 7, 7, 5, 5, 5, 5, 5, 5, 5, 5];

    fn zigzag(v: i32) -> u8 {
        if v >= 0 {
            (2 * v) as u8
        } else {
            (-2 * v - 1) as u8
        }
    }

    #[test]
    fn bc4_blocks_with_and_without_references() {
        let t = Tables::parse(&tables()).unwrap();
        // Block 0: endpoints 10, -3 (+128 stored), indices p -> p & 7.
        let mut b0 = vec![0, 0, zigzag(10), zigzag(-3)];
        b0.extend((0..8).map(|k| ((2 * k) & 7) | ((2 * k + 1) & 7) << 3));
        // Block 1 refers to block 0 for both: residual endpoints +1, +1; indices +1.
        let mut b1 = vec![0, 1, zigzag(1), zigzag(1)];
        b1.extend((0..8).map(|_| 1 | 1 << 3));
        let src = stream(&t, &BC4_LANES, 1, &[b0, b1]);
        let mut dst = [0u8; 16];
        decode(4, &src, &mut dst, &t).unwrap();
        let block = |e0: u8, e1: u8, ix: &dyn Fn(usize) -> usize| {
            let bits = (0..16).fold(0u64, |a, p| a | BC4_CODE[ix(p)] << (16 + 3 * p));
            (u64::from(e0) | u64::from(e1) << 8 | bits).to_le_bytes()
        };
        assert_eq!(dst[..8], block(138, 125, &|p| p & 7));
        assert_eq!(dst[8..], block(139, 126, &|p| (p + 1) & 7));
    }

    #[test]
    fn lanes_take_bytes_in_schedule_order_and_long_fragments() {
        let t = Tables::parse(&tables()).unwrap();
        // Many blocks: long fragments (over 256 payload bytes) on the wide lanes.
        let records: Vec<Vec<u8>> = (0..400u32)
            .map(|i| {
                let mut r = vec![0, 0, (i * 7) as u8, (i * 13) as u8];
                r.extend((0..8).map(|k| ((i + k) * 5 % 64) as u8));
                r
            })
            .collect();
        let src = stream(&t, &BC4_LANES, 1, &records);
        let mut got = vec![0u8; records.len() * 12];
        let mut next = src.iter().copied();
        lanes(&t, &BC4_LANES, 1, records.len(), |_| next.next(), &mut got).unwrap();
        assert_eq!(got, records.concat());
    }

    #[test]
    fn bc7_mode6_block() {
        let t = Tables::parse(&tables()).unwrap();
        // Control code 0 -> mode 6; endpoints R 8 bits (p-bit low), G B A 7 bits.
        let mut s = vec![0u8; 32];
        s[4] = zigzag(0x55); // R0 = 0x55: field 0x2a, p-bit 1
        s[5] = zigzag(-1); // R1 = 0xff (8 bits)
        s[6] = zigzag(3);
        s[10] = zigzag(0x7f >> 1);
        let src = stream(
            &t,
            &[3, 3, 7, 7, 7, 7, 7, 6, 6, 4, 5, 7, 7, 7, 7, 4],
            2,
            &[s],
        );
        let mut dst = [0u8; 16];
        decode(7, &src, &mut dst, &t).unwrap();
        let mut w = Bits::default();
        w.put(1 << 6, 7);
        for (v, n) in [
            (0x2a, 7),
            (0x7f, 7),
            (3, 7),
            (0, 7),
            (0, 7),
            (0, 7),
            (0x3f, 7),
            (0, 7),
        ] {
            w.put(v, n);
        }
        w.put(1, 1);
        w.put(1, 1);
        assert_eq!(dst, w.v.to_le_bytes());
    }

    #[test]
    fn malformed_input_is_an_error() {
        let t = Tables::parse(&tables()).unwrap();
        let records: Vec<Vec<u8>> = (0..64u8)
            .map(|i| vec![0, 0, i, i, 1, 2, 3, 4, 5, 6, 7, 8])
            .collect();
        let src = stream(&t, &BC4_LANES, 1, &records);
        let mut dst = vec![0u8; 64 * 8];
        decode(4, &src, &mut dst, &t).unwrap();
        // Truncated.
        assert!(decode(4, &src[..src.len() / 2], &mut dst, &t).is_err());
        // Headers promising more payload than the stream holds. (Other corrupt
        // headers decode to garbage: the format has no redundancy.)
        let headers = [[0x30; 8], [0xff; 8]].concat();
        assert!(decode(4, &headers, &mut dst[..8], &t).is_err());
        assert!(decode(4, &[], &mut dst, &t).is_err());
        // Destination not a whole number of blocks, empty, more blocks than coded.
        assert!(decode(4, &src, &mut dst[..12], &t).is_err());
        assert!(decode(4, &src, &mut [], &t).is_err());
        assert!(decode(4, &src, &mut vec![0u8; 65 * 8], &t).is_err());
        // Unsupported formats, bad tables.
        assert!(decode(2, &src, &mut dst, &t).is_err());
        assert!(Tables::parse(&[0; 16]).is_err());
        let mut no_tiling = tables();
        no_tiling[0] = 1;
        assert!(Tables::parse(&no_tiling).is_err());
    }

    #[test]
    fn noise_never_panics() {
        let t = Tables::parse(&tables()).unwrap();
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for round in 0..300 {
            let len = round * 7 % 900;
            let src: Vec<u8> = (0..len)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    seed as u8
                })
                .collect();
            for bc in [1, 3, 4, 5, 6, 7] {
                let mut dst = vec![0u8; 16 * (1 + round % 40)];
                let _ = decode(bc, &src, &mut dst, &t);
            }
        }
    }

    /// The window table finds the bucket a scan of the starts finds, for
    /// every slot of every model, in the shipped tables and in ours (whose
    /// buckets are wide) and in tables whose buckets are one slot wide.
    #[test]
    fn bucket_lookup_matches_a_scan_of_the_starts() {
        let mut narrow = tables();
        for m in 0..MODELS {
            // Buckets 1.. of one slot each where they fit; bucket 0 takes the rest.
            let used = m / 16 + 2;
            let rest = (1u32 << 16) - (1..used).map(|b| 1u32 << (b - 1)).sum::<u32>();
            for b in 0..used {
                let f = if b == 0 { rest } else { 1 };
                narrow[(m * BUCKETS + b) * 2..][..2].copy_from_slice(&(f as u16).to_le_bytes());
            }
        }
        for t in [
            Tables::builtin().unwrap(),
            Tables::parse(&tables()).unwrap(),
            Tables::parse(&narrow).unwrap(),
        ] {
            for model in 0..MODELS as u8 {
                let start = &t.models[usize::from(model)].start;
                for slot in 0..1u32 << 16 {
                    let scan = start[1..].iter().filter(|&&s| s <= slot).count();
                    assert_eq!(
                        bucket_of(&t, model, slot),
                        scan,
                        "model {model} slot {slot}"
                    );
                }
            }
        }
    }

    /// One rANS decode as first written: scan for the bucket, divide for the
    /// symbol, rebuild its frequency and start from the symbol.
    fn reference_decode(t: &Tables, model: u8, x: u32) -> (u8, u32) {
        let m = &t.models[usize::from(model)];
        let slot = x & 0xffff;
        let b = m.start[1..BUCKETS].iter().filter(|&&s| s <= slot).count();
        let sym = match b {
            0 => 0,
            1 => 1,
            _ => (1u32 << (b - 1)) + (slot - m.start(b)) / m.freq(b),
        } as u8;
        let (f, c) = if sym == 0 {
            (m.freq(0) - 1, 1)
        } else {
            let k = (8 - sym.leading_zeros()) as usize;
            let f = m.freq(k);
            (
                f,
                ((u32::from(sym) - (1 << (k - 1))) * f + m.start(k)) & 0xffff,
            )
        };
        (
            sym,
            (x >> 16).wrapping_mul(f).wrapping_sub(c).wrapping_add(slot),
        )
    }

    /// A payload step decodes the symbol and state the first version did, for
    /// every model and slot of the shipped tables and ours, at assorted states.
    #[test]
    fn payload_step_matches_the_reference_decode() {
        for t in [
            Tables::builtin().unwrap(),
            Tables::parse(&tables()).unwrap(),
        ] {
            for model in 0..MODELS as u8 {
                for high in [0x100u32, 0x1234, 0xffff] {
                    for slot in 0..1u32 << 16 {
                        let x = high << 16 | slot;
                        if x <= LOW {
                            continue;
                        }
                        let mut lane = Lane {
                            phase: 3,
                            model,
                            x,
                            left: 1,
                            ..Default::default()
                        };
                        lane.step(&t);
                        let (sym, next) = reference_decode(&t, model, x);
                        assert_eq!(
                            (lane.out, lane.x),
                            (u16::from(sym), next),
                            "model {model} x {x:#x}"
                        );
                    }
                }
            }
        }
    }

    /// Pseudo-random records that fit each lane's alphabet; BC7 keeps control
    /// code 0 (mode 6), the only one the synthetic tables define.
    fn random_records(widths: &[u8], per: usize, blocks: usize, bc7: bool) -> Vec<Vec<u8>> {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        (0..blocks)
            .map(|_| {
                let mut r: Vec<u8> = (0..widths.len() * per)
                    .map(|k| {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        let alphabet = 1u32 << (widths[k / per] + 1);
                        (seed as u32 % alphabet) as u8
                    })
                    .collect();
                if bc7 {
                    r[30] = 0;
                    r[31] = 0;
                }
                r
            })
            .collect()
    }

    /// Supported BCPack formats: `(bc, lane_widths, symbols_per_block_per_lane, block_bytes)`.
    const FORMAT_CASES: [(u8, &[u8], usize, usize); 6] = [
        (1, &[7, 7, 4, 5, 4, 4, 5, 4, 7, 7, 7, 7], 1, 8),
        (3, &[3, 3, 7, 5, 5, 5, 5, 4, 5, 4, 7, 7], 2, 16),
        (4, &BC4_LANES, 1, 8),
        (5, &[3, 3, 7, 7, 5, 5, 5, 5, 5, 5, 5, 5], 2, 16),
        (6, &[3, 3, 7, 7, 7, 4, 4, 4, 5, 5, 5, 7, 7, 7, 7, 4], 2, 16),
        (7, &[3, 3, 7, 7, 7, 7, 7, 6, 6, 4, 5, 7, 7, 7, 7, 4], 2, 16),
    ];

    /// The fast paths decode exactly what one lane step at a time does, on
    /// every format's lane layout, with short and long fragments.
    #[test]
    fn every_layout_round_trips_through_the_lanes() {
        let t = Tables::parse(&tables()).unwrap();
        for (_, widths, per, _) in FORMAT_CASES {
            for blocks in [1, 2, 17, 300, 1000] {
                let records = random_records(widths, per, blocks, false);
                let src = stream(&t, widths, per, &records);
                let mut got = vec![0u8; records.concat().len()];
                let mut next = src.iter().copied();
                lanes(&t, widths, per, blocks, |_| next.next(), &mut got).unwrap();
                assert_eq!(got, records.concat(), "{widths:?} x{per}, {blocks} blocks");
            }
        }
    }

    /// Encodes `symbols` as one or more fragments using `t`'s model
    /// `(width, shape)` (full alphabet, no reset).
    fn fragments_with_tables(t: &Tables, width: u8, shape: u8, symbols: &[u8]) -> Vec<u8> {
        let m = &t.models[usize::from(width) * 16 + usize::from(shape & 15)];
        let mut out = Vec::new();
        let mut chunks = symbols.chunks(4096).peekable();
        while let Some(chunk) = chunks.next() {
            let mut x = LOW;
            let mut bytes = Vec::new();
            for &s in chunk.iter().rev() {
                let (freq, start) = if s == 0 {
                    (m.freq(0) - 1, 1)
                } else {
                    let b = (8 - s.leading_zeros()) as usize;
                    let f = m.freq(b);
                    (f, m.start(b) + (u32::from(s) - (1 << (b - 1))) * f)
                };
                while x >= (freq << 16) {
                    bytes.push(x as u8);
                    x >>= 8;
                }
                x = ((x / freq) << 16) + x % freq + start;
            }
            bytes.extend_from_slice(&x.to_le_bytes());
            bytes.reverse();
            // Reset x = 0 between fragments, but leave x = LOW after the last
            // fragment so the trailing filler parks the lane at LOW.
            let mut hdr = shape & 15;
            if chunks.peek().is_some() {
                hdr |= 0x10;
            }
            if bytes.len() > 256 {
                hdr |= 0x20;
                let n = bytes.len() - 257;
                out.push(hdr);
                out.extend_from_slice(&[n as u8, (n >> 8) as u8]);
            } else {
                out.push(hdr);
                out.push((bytes.len() - 1) as u8);
            }
            out.extend(bytes);
        }
        out
    }

    /// Interleaves per-lane fragments encoded against `t` (any valid tables,
    /// including `Tables::builtin()`).
    fn stream_with_tables(
        t: &Tables,
        widths: &[u8],
        per: usize,
        records: &[Vec<u8>],
        shape: u8,
    ) -> Vec<u8> {
        let stride = widths.len() * per;
        let mut queues: Vec<std::collections::VecDeque<u8>> = (0..widths.len())
            .map(|j| {
                let syms: Vec<u8> = records
                    .iter()
                    .flat_map(|r| r[j * per..j * per + per].iter().copied())
                    .collect();
                let mut bytes = fragments_with_tables(t, widths[j], shape, &syms);
                bytes.extend([shape & 15, 1, 0, 0]);
                bytes.into()
            })
            .collect();
        let mut out = Vec::new();
        let mut check = vec![0; records.len() * stride];
        let fetch = |j: usize| {
            let b = queues[j]
                .pop_front()
                .expect("a parked lane asked for input");
            out.push(b);
            Some(b)
        };
        lanes(t, widths, per, records.len(), fetch, &mut check).unwrap();
        assert_eq!(check, records.concat());
        out
    }

    /// Synthetic records across all 6 formats valid for `Tables::builtin()`:
    /// biased toward small residuals and recent back-references (realistic
    /// texture entropy) while covering valid BC6H modes and BC7 control codes.
    fn realistic_records(t: &Tables, bc: u8, widths: &[u8], per: usize, blocks: usize) -> Vec<Vec<u8>> {
        let valid_bc7: Vec<(u8, u8)> = (0..512u16)
            .filter(|&c| t.ctrl7[usize::from(c)] != 0)
            .map(|c| ((c & 31) as u8, ((c >> 5) & 15) as u8))
            .collect();
        let mut seed = 0x517c_c1b7_2722_0a95u64 ^ u64::from(bc);
        let mut next_u32 = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as u32
        };
        (0..blocks)
            .map(|_| {
                let mut r: Vec<u8> = (0..widths.len() * per)
                    .map(|k| {
                        let alphabet = 1u32 << (widths[k / per] + 1);
                        // Skewed toward smaller symbols (half small, half full range).
                        let raw = next_u32();
                        if raw & 1 == 0 {
                            (raw >> 8) as u8 & ((alphabet.min(8) - 1) as u8)
                        } else {
                            (raw % alphabet) as u8
                        }
                    })
                    .collect();
                match bc {
                    6 => {
                        r[30] = (next_u32() % 14) as u8;
                        r[31] = (next_u32() % 32) as u8;
                    }
                    7 => {
                        let (lo, hi) = valid_bc7[next_u32() as usize % valid_bc7.len()];
                        r[30] = lo;
                        r[31] = hi;
                    }
                    _ => {}
                }
                r
            })
            .collect()
    }

    /// FNV-1a 64-bit checksum of decoded output, to verify bit-exactness across
    /// benchmark runs without storing game data.
    fn fnv1a(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    }

    /// Verifies compile-time bit-spread and nibble LUTs against direct loops.
    #[test]
    fn luts_match_bitwise_definitions() {
        let spread4 = |v: u8| (0..4).fold(0u8, |acc, k| acc | ((v >> k) & 1) << (2 * k));
        let spread8 = |v: u8| (0..8).fold(0u16, |acc, k| acc | u16::from((v >> k) & 1) << (2 * k));
        let half = |v: u8| (0..4).fold(0u8, |acc, k| acc | ((v >> (2 * k)) & 1) << k);
        let z = |u: u8| i16::from(u >> 1) ^ -i16::from(u & 1);
        for v in 0..=255u8 {
            assert_eq!(SPREAD4[usize::from(v & 15)], spread4(v));
            assert_eq!(SPREAD8[usize::from(v)], spread8(v));
            assert_eq!(
                NIBBLES_EVEN_FIRST[usize::from(v)],
                [z(half(v)), z(half(v >> 1))]
            );
            assert_eq!(
                nibbles(v, true),
                [z(half(v >> 1)), z(half(v))]
            );
        }
    }

    /// Reusing the thread-local history workspace across formats and sizes
    /// never leaks stale history into a later decode (including small streams
    /// and dictionary/zero references right after a large decode).
    #[test]
    fn workspace_reuse_across_all_formats_leaves_no_stale_history() {
        let t = Tables::builtin().unwrap();
        for (bc, widths, per, block) in FORMAT_CASES {
            // Dirty the thread-local workspace with > HISTORY blocks first.
            let big_records = realistic_records(&t, bc, widths, per, 300);
            let big_src = stream_with_tables(&t, widths, per, &big_records, 3);
            let mut big_dst = vec![0u8; 300 * block];
            decode(bc, &big_src, &mut big_dst, &t).unwrap();

            // Decode small streams (1, 2, 17 blocks) on the same dirty thread
            // and compare byte-for-byte with a fresh thread's workspace.
            for blocks in [1, 2, 17] {
                let small_records = realistic_records(&t, bc, widths, per, blocks);
                let small_src = stream_with_tables(&t, widths, per, &small_records, 3);
                let mut reused = vec![0u8; blocks * block];
                decode(bc, &small_src, &mut reused, &t).unwrap();

                let src_clone = small_src.clone();
                let fresh = std::thread::spawn(move || {
                    let t2 = Tables::builtin().unwrap();
                    let mut out = vec![0u8; blocks * block];
                    decode(bc, &src_clone, &mut out, &t2).unwrap();
                    out
                })
                .join()
                .unwrap();
                assert_eq!(reused, fresh, "BC{bc} {blocks} blocks");
            }
        }
    }

    /// Oversized staged-output workspace allocations are trimmed after decode
    /// so idle worker threads do not hold unbounded memory.
    #[test]
    fn workspace_trims_oversized_staged_output() {
        let mut ws = Workspace::new();
        ws.prepare_out(OUT_CAP);
        assert!(ws.out.capacity() >= OUT_CAP);
        ws.trim();
        assert!(ws.out.capacity() >= OUT_CAP, "within cap stays retained");

        ws.prepare_out(OUT_CAP + 16);
        assert!(ws.out.capacity() > OUT_CAP);
        ws.trim();
        assert_eq!(ws.out.capacity(), 0, "oversized buffer is released");
    }

    /// A truncated multi-block stream that fails in the entropy phase after
    /// decoding early blocks must leave the caller's destination buffer
    /// untouched (preserving in-place DirectStorage destinations on error).
    #[test]
    fn truncated_stream_after_valid_blocks_leaves_dst_untouched() {
        let t = Tables::builtin().unwrap();
        let blocks = 64usize;
        for (bc, widths, per, block) in FORMAT_CASES {
            let records = realistic_records(&t, bc, widths, per, blocks);
            let src = stream_with_tables(&t, widths, per, &records, 3);
            // Cut halfway through: early blocks decode cleanly, later lanes run dry.
            let truncated = &src[..src.len() / 2];
            let sentinel = 0xa5u8;
            let mut dst = vec![sentinel; blocks * block];
            assert!(decode(bc, truncated, &mut dst, &t).is_err());
            assert!(
                dst.iter().all(|&b| b == sentinel),
                "BC{bc}: truncated entropy stream modified destination bytes"
            );
        }
    }

    /// Repeatable multi-sample benchmark across all 6 BCPack formats using the
    /// shipped `Tables::builtin()` on deterministic synthetic streams:
    /// `cargo test --release -p victory-formats bcpack_all_formats_bench -- --ignored --nocapture --test-threads=1`.
    #[test]
    #[ignore = "benchmark"]
    fn bcpack_all_formats_bench() {
        let t = Tables::builtin().unwrap();
        let blocks = 1 << 14;
        let samples = 11usize;
        let runs_per_sample = 16u32;
        for (bc, widths, per, block) in FORMAT_CASES {
            let records = realistic_records(&t, bc, widths, per, blocks);
            let src = stream_with_tables(&t, widths, per, &records, 3);
            let mut dst = vec![0u8; blocks * block];
            decode(bc, &src, &mut dst, &t).unwrap();
            let digest = fnv1a(&dst);
            // Warm up.
            for _ in 0..4 {
                decode(bc, std::hint::black_box(&src), &mut dst, &t).unwrap();
            }
            let mut ns_per_block = Vec::with_capacity(samples);
            for _ in 0..samples {
                let started = std::time::Instant::now();
                for _ in 0..runs_per_sample {
                    decode(bc, std::hint::black_box(&src), &mut dst, &t).unwrap();
                    std::hint::black_box(&dst);
                }
                let secs = started.elapsed().as_secs_f64() / f64::from(runs_per_sample);
                ns_per_block.push(secs * 1e9 / blocks as f64);
            }
            ns_per_block.sort_by(f64::total_cmp);
            let med = ns_per_block[samples / 2];
            let min = ns_per_block[0];
            let max = ns_per_block[samples - 1];
            let med_mb_s = (block as f64) * 1e3 / med;
            println!(
                "BC{bc} (builtin, {blocks} blocks, {samples}x{runs_per_sample}): median {med_mb_s:.1} MB/s ({med:.1} ns/blk, min {min:.1}, max {max:.1}) digest={digest:#018x}"
            );
        }
    }

    /// Decode throughput on synthetic streams (no game data):
    /// `cargo test --release -p victory-formats bcpack_throughput -- --ignored --nocapture`.
    #[test]
    #[ignore = "benchmark"]
    fn bcpack_throughput() {
        let t = Tables::parse(&tables()).unwrap();
        let cases: [(u8, &[u8], usize, usize); 3] = [
            (1, &[7, 7, 4, 5, 4, 4, 5, 4, 7, 7, 7, 7], 1, 8),
            (4, &BC4_LANES, 1, 8),
            (7, &[3, 3, 7, 7, 7, 7, 7, 6, 6, 4, 5, 7, 7, 7, 7, 4], 2, 16),
        ];
        for (bc, widths, per, block) in cases {
            // One fragment per lane holds at most 65792 bytes.
            let blocks = 1 << 14;
            let records = random_records(widths, per, blocks, bc == 7);
            let src = stream(&t, widths, per, &records);
            let mut dst = vec![0u8; blocks * block];
            decode(bc, &src, &mut dst, &t).unwrap();
            let runs = 40;
            let started = std::time::Instant::now();
            for _ in 0..runs {
                decode(bc, std::hint::black_box(&src), &mut dst, &t).unwrap();
            }
            let secs = started.elapsed().as_secs_f64() / f64::from(runs);
            println!(
                "BC{bc}: {blocks} blocks, {:.1} MB out/s, {:.1} ns/block",
                (blocks * block) as f64 / secs / 1e6,
                secs * 1e9 / blocks as f64
            );
        }
    }

    /// Compares with reference outputs made outside the repository:
    /// `VICTORY_BCPACK_TABLES` (the table file) and `VICTORY_BCPACK_CASES`, a
    /// directory of `<name>.bc<n>` packed chunks with `<name>.out` beside them.
    #[test]
    #[ignore = "needs the format tables and reference chunks"]
    fn reference_chunks() {
        let tables = std::env::var_os("VICTORY_BCPACK_TABLES").expect("VICTORY_BCPACK_TABLES");
        let cases = std::env::var_os("VICTORY_BCPACK_CASES").expect("VICTORY_BCPACK_CASES");
        let t = Tables::parse(&std::fs::read(tables).unwrap()).unwrap();
        let mut n = 0;
        for entry in std::fs::read_dir(cases).unwrap() {
            let path = entry.unwrap().path();
            let Some(bc) = path
                .extension()
                .and_then(|e| e.to_str()?.strip_prefix("bc")?.parse().ok())
            else {
                continue;
            };
            let want = std::fs::read(path.with_extension("out")).unwrap();
            let mut got = vec![0u8; want.len()];
            decode(bc, &std::fs::read(&path).unwrap(), &mut got, &t).unwrap();
            assert!(got == want, "{}", path.display());
            n += 1;
        }
        assert!(n > 0);
    }
}

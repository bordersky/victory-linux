//! Indirect execution's arguments, bounded before the GPU reads them. The
//! title's own compute passes write most of them; a pass we drop or
//! translate wrongly leaves whatever the memory held, and a draw of four
//! billion vertices or a dispatch past the host's limits runs until the
//! host resets the display driver. So active records are copied and clamped
//! on the GPU into a device-local scratch buffer, just before execution, by a
//! compute shader of ours (HLSL compiled by the system's compiler; a 5.1
//! shader with its root signature inside), leaving the title's argument and
//! count buffers untouched.
//!
//! When a count buffer is given, threads at or past `min(max, count)` return
//! immediately without charging the list's budget or reading uninitialized
//! tail slots. Each list has a budget (2^27 vertex instances or threads,
//! groups counted as 64 threads, mesh groups as 128): active records past it
//! run empty, saturating without 32-bit wraparound so a list of thousands of
//! garbage records cannot run for seconds either.
//!
//! shortcut: the bounds (2^24 vertices, 2^26 vertex instances, 65,535
//! groups per dimension and 2^20 per dispatch, one-dimensional mesh
//! dispatches, the budget) cut legitimate work beyond them; raise them if
//! the title needs more.

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{Interface, PCSTR, s};

use crate::host::{HostList, host, transition};

const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=6, b0), SRV(t0), SRV(t1), UAV(u0), UAV(u1)"
cbuffer Record : register(b0) {
    uint kind;
    uint stride;
    uint records;
    uint args_offset;
    uint has_count;
    uint count_offset;
};
ByteAddressBuffer src_args : register(t0);
ByteAddressBuffer count_buf : register(t1);
RWByteAddressBuffer dst_args : register(u0);
// The list's work so far (vertex instances, threads): past the budget,
// records run empty.
RWByteAddressBuffer budget : register(u1);
static const uint BUDGET = 1u << 27;
// Groups of one dispatch; more run empty.
static const uint GROUPS = 1u << 20;
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint eff_count = records;
    if (has_count != 0) {
        eff_count = min(eff_count, count_buf.Load(count_offset));
    }
    if (id.x >= eff_count) return;
    uint src_at = args_offset + id.x * stride;
    uint dst_at = id.x * stride;
    uint cost;
    uint2 draw_counts = uint2(0, 0);
    uint3 groups = uint3(0, 0, 0);
    if (kind <= 1) {
        // Draws: vertex or index count, then instances.
        uint count = min(src_args.Load(src_at), 1u << 24);
        uint instances = min(src_args.Load(src_at + 4), (1u << 26) / max(count, 1u));
        cost = count * instances;
        draw_counts = uint2(count, instances);
    } else if (kind == 2) {
        uint3 g = min(src_args.Load3(src_at), uint3(65535u, 65535u, 65535u));
        // x * y fits 32 bits; the whole count may not.
        if (g.z != 0 && g.x * g.y > GROUPS / g.z) {
            g = uint3(0, 0, 0);
        }
        cost = g.x * g.y * g.z * 64u;
        groups = g;
    } else {
        // Mesh dispatches: the console's are one-dimensional.
        uint3 g = src_args.Load3(src_at);
        g = uint3(min(g.x, 65535u), min(g.y, 1u), min(g.z, 1u));
        cost = g.x * g.y * g.z * 128u;
        groups = g;
    }
    bool over = false;
    if (cost > 0) {
        uint before;
        budget.InterlockedOr(0, 0, before);
        // Bounded retries: an unbounded loop of atomics is a GPU hang if
        // contention never settles; a record that gives up runs empty.
        bool settled = false;
        [allow_uav_condition]
        for (uint tries = 0; tries < 256 && before < BUDGET; tries++) {
            uint next = (BUDGET - before <= cost) ? BUDGET : (before + cost);
            uint prev;
            budget.InterlockedCompareExchange(0, before, next, prev);
            if (prev == before) { settled = true; break; }
            before = prev;
        }
        over = !settled || before >= BUDGET || BUDGET - before < cost;
    }
    if (kind <= 1) {
        if (over) {
            draw_counts = uint2(0, 0);
        }
        dst_args.Store2(dst_at, draw_counts);
        for (uint off = 8; off < stride; off += 4) {
            dst_args.Store(dst_at + off, src_args.Load(src_at + off));
        }
    } else {
        if (over) {
            groups = uint3(0, 0, 0);
        }
        dst_args.Store3(dst_at, groups);
        for (uint off = 12; off < stride; off += 4) {
            dst_args.Store(dst_at + off, src_args.Load(src_at + off));
        }
    }
}
"#;

const BATCH_SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=4, b0), SRV(t0), SRV(t1), SRV(t2), UAV(u0), UAV(u1)"
cbuffer Batch : register(b0) { uint first_job; uint job_count; uint unused0; uint unused1; };
// Packed 32-byte job descriptors:
//   j0 = uint4(kind, stride, records, args_offset)
//   j1 = uint4(has_count, count_offset, dst_offset, 0)
ByteAddressBuffer jobs : register(t0);
ByteAddressBuffer src_args : register(t1);
ByteAddressBuffer count_buf : register(t2);
RWByteAddressBuffer dst_args : register(u0);
RWByteAddressBuffer budget : register(u1);
static const uint BUDGET = 1u << 27;
static const uint GROUPS = 1u << 20;
static const uint PAST = 0xffffffffu;
groupshared uint costs[64];
groupshared uint single_run;

uint compute_clamped(uint kind, uint src_at, out uint2 draw_counts, out uint3 groups) {
    draw_counts = uint2(0, 0);
    groups = uint3(0, 0, 0);
    if (kind <= 1) {
        uint count = min(src_args.Load(src_at), 1u << 24);
        uint instances = min(src_args.Load(src_at + 4), (1u << 26) / max(count, 1u));
        draw_counts = uint2(count, instances);
        return count * instances;
    } else if (kind == 2) {
        uint3 g = min(src_args.Load3(src_at), uint3(65535u, 65535u, 65535u));
        if (g.z != 0 && g.x * g.y > GROUPS / g.z) {
            g = uint3(0, 0, 0);
        }
        groups = g;
        return g.x * g.y * g.z * 64u;
    } else {
        uint3 g = src_args.Load3(src_at);
        g = uint3(min(g.x, 65535u), min(g.y, 1u), min(g.z, 1u));
        groups = g;
        return g.x * g.y * g.z * 128u;
    }
}

void write_record(uint kind, uint stride, uint src_at, uint dst_at, bool over, uint2 draw_counts, uint3 groups) {
    if (kind <= 1) {
        if (over) {
            draw_counts = uint2(0, 0);
        }
        dst_args.Store2(dst_at, draw_counts);
        for (uint off = 8; off < stride; off += 4) {
            dst_args.Store(dst_at + off, src_args.Load(src_at + off));
        }
    } else {
        if (over) {
            groups = uint3(0, 0, 0);
        }
        dst_args.Store3(dst_at, groups);
        for (uint off = 12; off < stride; off += 4) {
            dst_args.Store(dst_at + off, src_args.Load(src_at + off));
        }
    }
}

[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint lane : SV_GroupIndex) {
    uint left = 0;
    if (lane == 0) {
        left = BUDGET - min(budget.Load(0), BUDGET);
    }
    uint j = first_job;
    uint end = first_job + job_count;
    while (j < end) {
        // Fast path: clamp up to 64 consecutive single-record jobs in one wave.
        bool has_job = (j + lane < end);
        uint4 j0 = has_job ? jobs.Load4((j + lane) * 32) : uint4(0, 0, 0, 0);
        uint4 j1 = has_job ? jobs.Load4((j + lane) * 32 + 16) : uint4(0, 0, 0, 0);
        costs[lane] = j0.z;
        GroupMemoryBarrierWithGroupSync();
        if (lane == 0) {
            uint run = 0;
            while (run < 64 && costs[run] == 1) {
                run++;
            }
            single_run = run;
        }
        GroupMemoryBarrierWithGroupSync();
        uint run = single_run;
        if (run > 0) {
            bool active = false;
            uint cost = 0;
            uint2 draw_counts = uint2(0, 0);
            uint3 groups = uint3(0, 0, 0);
            if (lane < run) {
                active = (j1.x == 0) || (count_buf.Load(j1.y) >= 1u);
                if (active) {
                    cost = compute_clamped(j0.x, j0.w, draw_counts, groups);
                }
            }
            costs[lane] = cost;
            GroupMemoryBarrierWithGroupSync();
            if (lane == 0) {
                for (uint i = 0; i < run; i++) {
                    if (costs[i] > left) {
                        costs[i] = PAST;
                        left = 0;
                    } else {
                        left -= costs[i];
                    }
                }
            }
            GroupMemoryBarrierWithGroupSync();
            if (active) {
                write_record(j0.x, j0.y, j0.w, j1.z, costs[lane] == PAST, draw_counts, groups);
            }
            j += run;
        } else {
            uint4 m0 = jobs.Load4(j * 32);
            uint4 m1 = jobs.Load4(j * 32 + 16);
            uint eff_count = m0.z;
            if (m1.x != 0) {
                eff_count = min(eff_count, count_buf.Load(m1.y));
            }
            uint tiles = eff_count / 64 + min(eff_count % 64, 1u);
            for (uint tile = 0; tile < tiles; tile++) {
                uint record = tile * 64 + lane;
                uint src_at = m0.w + record * m0.y;
                uint dst_at = m1.z + record * m0.y;
                uint cost = 0;
                uint2 draw_counts = uint2(0, 0);
                uint3 groups = uint3(0, 0, 0);
                if (record < eff_count) {
                    cost = compute_clamped(m0.x, src_at, draw_counts, groups);
                }
                costs[lane] = cost;
                GroupMemoryBarrierWithGroupSync();
                if (lane == 0) {
                    for (uint i = 0; i < 64; i++) {
                        if (costs[i] > left) {
                            costs[i] = PAST;
                            left = 0;
                        } else {
                            left -= costs[i];
                        }
                    }
                }
                GroupMemoryBarrierWithGroupSync();
                if (record < eff_count) {
                    write_record(m0.x, m0.y, src_at, dst_at, costs[lane] == PAST, draw_counts, groups);
                }
            }
            j += 1;
        }
    }
    if (lane == 0) {
        budget.Store(0, BUDGET - left);
    }
}
"#;

struct Clamp {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Clamp {}
// SAFETY: as above.
unsafe impl Sync for Clamp {}

fn clamp() -> Option<&'static Clamp> {
    static CLAMP: std::sync::OnceLock<Option<Clamp>> = std::sync::OnceLock::new();
    CLAMP
        .get_or_init(|| match make() {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(error = %e, "no argument clamp: indirect execution dropped");
                None
            }
        })
        .as_ref()
}

fn batch_clamp() -> Option<&'static Clamp> {
    static BATCH: std::sync::OnceLock<Option<Clamp>> = std::sync::OnceLock::new();
    BATCH
        .get_or_init(|| {
            let device = &host().ok_or_else(windows::core::Error::empty).ok()?.device;
            match compile_for(device, BATCH_SOURCE, s!("batch_clamp")) {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!(error = %e, "no batched argument clamp: falling back to per-draw dispatches");
                    None
                }
            }
        })
        .as_ref()
}

fn make() -> windows::core::Result<Clamp> {
    let device = &host().ok_or_else(windows::core::Error::empty)?.device;
    make_for(device)
}

fn make_for(device: &ID3D12Device) -> windows::core::Result<Clamp> {
    compile_for(device, SOURCE, s!("clamp"))
}

fn compile_for(device: &ID3D12Device, source: &str, name: PCSTR) -> windows::core::Result<Clamp> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            name,
            None,
            None,
            s!("main"),
            s!("cs_5_1"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe {
                std::slice::from_raw_parts(
                    errors.GetBufferPointer().cast::<u8>(),
                    errors.GetBufferSize(),
                )
            };
            tracing::warn!("argument clamp: {}", String::from_utf8_lossy(text));
        }
        return Err(e);
    }
    let code = code.ok_or_else(windows::core::Error::empty)?;
    // SAFETY: the blob's own bytes; the root signature is inside the shader.
    unsafe {
        let bytes =
            std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
        let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: bytes.as_ptr().cast(),
                BytecodeLength: bytes.len(),
            },
            ..Default::default()
        })?;
        Ok(Clamp { root, pso })
    }
}

/// Minimum record byte stride for a supported PC indirect argument `kind`.
fn min_stride(kind: u32) -> Option<u32> {
    match kind {
        0 => Some(16),
        1 => Some(20),
        2 | 10 => Some(12),
        _ => None,
    }
}

/// Validates record alignment and returns the scratch byte size `stride * records`.
fn validate_layout(
    (kind, stride, records): (u32, u32, u32),
    src_offset: u64,
    count_offset: Option<u64>,
) -> Option<u64> {
    let min = min_stride(kind)?;
    if stride < min || !stride.is_multiple_of(4) || !src_offset.is_multiple_of(4) {
        return None;
    }
    if count_offset.is_some_and(|off| !off.is_multiple_of(4)) {
        return None;
    }
    let bytes = u64::from(stride).checked_mul(u64::from(records))?;
    (bytes <= u64::from(u32::MAX)).then_some(bytes)
}

/// Records, in `list`, the non-destructive clamp of up to `records` records of
/// `kind` (PC argument numbering) `stride` bytes apart from `src` (and bounded
/// by the 32-bit count in `count` when given) into a device-local scratch
/// buffer, leaving that scratch buffer in `INDIRECT_ARGUMENT` at offset `0`.
/// Replaces the list's compute root signature and pipeline (callers bind
/// theirs after). `None` if the clamp or alignment is not available.
pub(crate) fn bound(
    list: &HostList,
    src: (&ID3D12Resource, u64),
    count: Option<(&ID3D12Resource, u64)>,
    spec: (u32, u32, u32),
) -> Option<ID3D12Resource> {
    let bytes = validate_layout(spec, src.1, count.map(|(_, off)| off))?;
    let (c, budget, dst) = (clamp()?, list.budget()?, list.indirect_scratch(bytes)?);
    record_clamp(c, &list.list, &dst, budget, src, count, spec).then_some(dst)
}

fn record_clamp(
    c: &Clamp,
    l: &ID3D12GraphicsCommandList,
    dst: &ID3D12Resource,
    budget: u64,
    (src, src_offset): (&ID3D12Resource, u64),
    count: Option<(&ID3D12Resource, u64)>,
    (kind, stride, records): (u32, u32, u32),
) -> bool {
    let Some(bytes) = validate_layout(
        (kind, stride, records),
        src_offset,
        count.map(|(_, off)| off),
    ) else {
        return false;
    };
    // SAFETY: live D3D12 buffer resources.
    let (src_base, dst_gpu) = unsafe { (src.GetGPUVirtualAddress(), dst.GetGPUVirtualAddress()) };
    if !src_base.is_multiple_of(4) || !dst_gpu.is_multiple_of(4) || !budget.is_multiple_of(4) {
        return false;
    }
    let (src_gpu, args_offset) = match src_offset
        .checked_add(bytes)
        .filter(|&end| end <= u64::from(u32::MAX))
    {
        Some(_) => (src_base, src_offset as u32),
        None => {
            let Some(rebased) = src_base.checked_add(src_offset) else {
                return false;
            };
            (rebased, 0)
        }
    };
    let (count_gpu, has_count, count_offset) = match count {
        Some((buf, off)) => {
            // SAFETY: a live D3D12 buffer resource.
            let base = unsafe { buf.GetGPUVirtualAddress() };
            if !base.is_multiple_of(4) {
                return false;
            }
            match off.checked_add(4).filter(|&end| end <= u64::from(u32::MAX)) {
                Some(_) => (base, 1u32, off as u32),
                None => {
                    let Some(rebased) = base.checked_add(off) else {
                        return false;
                    };
                    (rebased, 1u32, 0u32)
                }
            }
        }
        None => (src_gpu, 0u32, 0u32),
    };
    let constants = [kind, stride, records, args_offset, has_count, count_offset];
    // SAFETY: an open list; our root signature and pipeline; live source,
    // count, scratch destination, and budget buffers. `src` and `count` stay
    // in `COMMON` / `GENERIC_READ` via implicit buffer promotion while `dst`
    // transitions `COMMON -> UNORDERED_ACCESS -> INDIRECT_ARGUMENT`.
    unsafe {
        // Earlier shader writes (the title's argument and count passes) first.
        l.ResourceBarrier(&[
            uav_barrier(),
            transition(
                dst,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            ),
        ]);
        if records > 0 {
            l.SetComputeRootSignature(&c.root);
            l.SetPipelineState(&c.pso);
            l.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
            l.SetComputeRootShaderResourceView(1, src_gpu);
            l.SetComputeRootShaderResourceView(2, count_gpu);
            l.SetComputeRootUnorderedAccessView(3, dst_gpu);
            l.SetComputeRootUnorderedAccessView(4, budget);
            l.Dispatch(records.div_ceil(64), 1, 1);
        }
        l.ResourceBarrier(&[
            uav_barrier(),
            transition(
                dst,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            ),
        ]);
    }
    true
}

/// How clamps of indirect draws are recorded: 0 each before its draw; 1 in
/// a list of their own before the list that draws, so that a run of draws
/// is not interleaved with our dispatches (the default); 2 as 1, with a
/// barrier for shader writes before each draw as the clamps had.
/// To find faults: 3 as 1 for mesh dispatches only, 4 as 1 for draws only,
/// 5 as 1 with a new list for the clamps always, 6 the lists cut as for 1
/// with each clamp before its draw all the same.
/// 7 as 1 with the clamps run one after another (a barrier between them):
/// the default. Without it whole mesh objects flickered (leaves, chairs,
/// clothes: seen by the user, 2026-11-25), as records past the list's budget
/// were not the same ones from frame to frame.
/// `VICTORY_HOIST_CLAMPS=<n>` sets it; diagnostics builds read it again
/// every half second from the file `VICTORY_HOIST_FILE` names, if any.
pub(crate) fn hoist_mode() -> u8 {
    use std::sync::atomic::{AtomicU8, Ordering};
    static MODE: AtomicU8 = AtomicU8::new(u8::MAX);
    let mode = MODE.load(Ordering::Relaxed);
    if mode == u8::MAX {
        let set = std::env::var("VICTORY_HOIST_CLAMPS").ok().and_then(|v| v.parse().ok()).unwrap_or(7);
        MODE.store(set, Ordering::Relaxed);
        if cfg!(feature = "diagnostics")
            && let Some(file) = std::env::var_os("VICTORY_HOIST_FILE")
        {
            let _ = std::thread::Builder::new().name("victory-hoist".into()).spawn(move || {
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    let read = std::fs::read_to_string(&file).ok().and_then(|t| t.trim().parse::<u8>().ok());
                    let cuts = CUTS.each_ref().map(|c| c.swap(0, Ordering::Relaxed));
                    if cuts != [0, 0] {
                        tracing::info!(cuts = cuts[0], queries = cuts[1], "lists cut for hoisted clamps");
                    }
                    if let Some(n) = read.filter(|&n| n < 8 && n != MODE.load(Ordering::Relaxed)) {
                        MODE.store(n, Ordering::Relaxed);
                        tracing::info!(mode = n, "clamps of indirect draws: mode changed");
                    }
                }
            });
        }
        return set;
    }
    mode
}

/// Cuts made for hoisted clamps, and those not made for queries that run.
pub(crate) static CUTS: [std::sync::atomic::AtomicU64; 2] =
    [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];

/// Whether clamps of indirect draws go to a list before the one that draws.
pub(crate) fn hoisting() -> bool {
    hoist_mode() != 0
}

/// One queued hoisted clamp job (its 32-byte payload lives in [`JobUpload`]).
struct BatchJob {
    src_gpu: u64,
    /// `None` when `has_count == 0` (the shader never loads `count_buf`, so
    /// uncounted jobs coalesce into adjacent counted runs sharing `src_gpu`
    /// and `dst_gpu`).
    count_gpu: Option<u64>,
    dst_gpu: u64,
}

/// Persistent upload buffer holding packed 32-byte job descriptors for a
/// [`HostList`]'s hoisted clamp list, reused across resets.
struct JobUpload {
    _buffer: ID3D12Resource,
    mapped: usize,
    gpu: u64,
    /// Capacity in jobs (32 bytes each).
    capacity: usize,
}

// SAFETY: `mapped` is a persistent upload mapping written only under the
// list's `ClampBatch` lock while the list is open for recording.
unsafe impl Send for JobUpload {}

/// Queued hoisted indirect clamps recorded on a clamps [`HostList`] and
/// flushed as batched dispatches in [`HostList::clamp_end`], preserving exact
/// draw-order budget charging without per-draw dispatches or global UAV
/// barriers.
#[derive(Default)]
pub(crate) struct ClampBatch {
    jobs: Vec<BatchJob>,
    /// Index of the first job not yet flushed to the command list.
    flushed: usize,
    /// True if an unbatched dispatch (e.g. a non-7 diagnostic mode) ran on
    /// this list before the current pending batch.
    had_unbatched: bool,
    upload: Option<JobUpload>,
    /// Source/count buffers referenced by queued jobs, kept alive until reset.
    kept: Vec<ID3D12Resource>,
}

// SAFETY: D3D12 resources are free-threaded; mutated under the list's mutex.
unsafe impl Send for ClampBatch {}

impl ClampBatch {
    pub(crate) fn reset(&mut self) {
        self.jobs.clear();
        self.flushed = 0;
        self.had_unbatched = false;
        self.kept.clear();
    }

    fn keep_once(&mut self, resource: &ID3D12Resource) {
        let raw = resource.as_raw();
        if !self.kept.iter().any(|r| r.as_raw() == raw) {
            self.kept.push(resource.clone());
        }
    }

    fn ensure_upload(&mut self, needed_jobs: usize) -> bool {
        if self.upload.as_ref().is_some_and(|u| u.capacity >= needed_jobs) {
            return true;
        }
        let Some(host) = host() else {
            return false;
        };
        let capacity = needed_jobs.next_power_of_two().max(256);
        let Some(bytes) = (capacity as u64).checked_mul(32) else {
            return false;
        };
        let Some((buffer, data)) = host.upload_buffer(bytes) else {
            return false;
        };
        crate::host::name(&buffer, || format!("hoisted clamp jobs ({capacity})"));
        // SAFETY: a live upload buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        if let Some(old) = &self.upload
            && !self.jobs.is_empty()
        {
            // SAFETY: both mappings are valid for `self.jobs.len() * 32` bytes and disjoint.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    old.mapped as *const u8,
                    data,
                    self.jobs.len() * 32,
                );
            }
            // Keep the old upload buffer alive until reset in case earlier
            // dispatches in this list already referenced its GPU address.
            if self.flushed > 0 {
                self.kept.push(old._buffer.clone());
            }
        }
        self.upload = Some(JobUpload {
            _buffer: buffer,
            mapped: data as usize,
            gpu,
            capacity,
        });
        true
    }

    fn push(
        &mut self,
        src_buf: &ID3D12Resource,
        count_buf: Option<&ID3D12Resource>,
        src_gpu: u64,
        count_gpu: Option<u64>,
        dst_gpu: u64,
        words: [u32; 8],
    ) -> bool {
        let slot = self.jobs.len();
        if u32::try_from(slot).is_err() || !self.ensure_upload(slot + 1) {
            return false;
        }
        self.keep_once(src_buf);
        if let Some(c) = count_buf {
            self.keep_once(c);
        }
        if let Some(u) = &self.upload {
            // SAFETY: `ensure_upload(slot + 1)` guarantees `[slot * 32 .. (slot + 1) * 32)`
            // lies within the persistent mapped upload buffer.
            unsafe {
                (u.mapped as *mut [u32; 8]).add(slot).write(words);
            }
        }
        self.jobs.push(BatchJob {
            src_gpu,
            count_gpu,
            dst_gpu,
        });
        true
    }
}

/// Flushes any batched hoisted clamps queued on `clamps` into `clamps.list`
/// before its scratch chunks transition to `INDIRECT_ARGUMENT`. Returns `true`
/// if at least one batched dispatch was recorded.
pub(crate) fn flush_before(clamps: &HostList) -> bool {
    let mut batch = clamps.clamp_batch();
    if batch.flushed >= batch.jobs.len() {
        return false;
    }
    let (Some(c), Some((budget_resource, budget_gpu)), Some(upload_gpu)) = (
        batch_clamp(),
        clamps.budget_resource(),
        batch.upload.as_ref().map(|u| u.gpu),
    ) else {
        return false;
    };
    let l = &clamps.list;
    let start_idx = batch.flushed;
    let end_idx = batch.jobs.len();
    // SAFETY: an open list; our batched root signature and PSO; live job,
    // source, count, destination (`clamp_room`) and budget buffers.
    unsafe {
        if batch.had_unbatched {
            l.ResourceBarrier(&[uav_barrier_of(Some(budget_resource))]);
            batch.had_unbatched = false;
        }
        l.SetComputeRootSignature(&c.root);
        l.SetPipelineState(&c.pso);
        l.SetComputeRootShaderResourceView(1, upload_gpu);
        l.SetComputeRootUnorderedAccessView(5, budget_gpu);
        let mut run_start = start_idx;
        let mut run_index = 0usize;
        while run_start < end_idx {
            let src_gpu = batch.jobs[run_start].src_gpu;
            let dst_gpu = batch.jobs[run_start].dst_gpu;
            let mut count_gpu = batch.jobs[run_start].count_gpu;
            let mut run_end = run_start + 1;
            while run_end < end_idx {
                let next = &batch.jobs[run_end];
                if next.src_gpu != src_gpu || next.dst_gpu != dst_gpu {
                    break;
                }
                match (count_gpu, next.count_gpu) {
                    (Some(a), Some(b)) if a != b => break,
                    (None, Some(b)) => count_gpu = Some(b),
                    _ => {}
                }
                run_end += 1;
            }
            if run_index > 0 {
                l.ResourceBarrier(&[uav_barrier_of(Some(budget_resource))]);
            }
            let constants = [run_start as u32, (run_end - run_start) as u32, 0, 0];
            l.SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
            l.SetComputeRootShaderResourceView(2, src_gpu);
            l.SetComputeRootShaderResourceView(3, count_gpu.unwrap_or(src_gpu));
            l.SetComputeRootUnorderedAccessView(4, dst_gpu);
            l.Dispatch(1, 1, 1);
            run_start = run_end;
            run_index += 1;
        }
    }
    batch.flushed = end_idx;
    true
}

/// As [`bound`], recorded in `clamps`, a list that runs before the one that
/// executes: the buffer and the offset of the clamped records, readable as
/// indirect arguments once `clamps` ended ([`HostList::clamp_end`]).
pub(crate) fn bound_before(
    clamps: &HostList,
    (src, src_offset): (&ID3D12Resource, u64),
    count: Option<(&ID3D12Resource, u64)>,
    (kind, stride, records): (u32, u32, u32),
) -> Option<(ID3D12Resource, u64)> {
    bound_before_mode(clamps, (src, src_offset), count, (kind, stride, records), hoist_mode())
}

fn bound_before_mode(
    clamps: &HostList,
    (src, src_offset): (&ID3D12Resource, u64),
    count: Option<(&ID3D12Resource, u64)>,
    (kind, stride, records): (u32, u32, u32),
    mode: u8,
) -> Option<(ID3D12Resource, u64)> {
    let bytes = validate_layout((kind, stride, records), src_offset, count.map(|(_, off)| off))?;
    if records == 0 {
        return None;
    }
    let budget = clamps.budget()?;
    // SAFETY: live buffers.
    let (src_base, count_base) =
        unsafe { (src.GetGPUVirtualAddress(), count.map(|(b, _)| b.GetGPUVirtualAddress())) };
    // Addresses the shader's offsets (32 bits) reach, as in `record_clamp`.
    let fits = |offset: u64, len: u64| offset.checked_add(len).is_some_and(|end| end <= u64::from(u32::MAX));
    let (src_gpu, args_offset) = if fits(src_offset, bytes) {
        (src_base, src_offset as u32)
    } else {
        (src_base.checked_add(src_offset)?, 0)
    };
    let (count_gpu, has_count, count_offset) = match (count, count_base) {
        (Some((_, off)), Some(base)) if fits(off, 4) => (base, 1u32, off as u32),
        (Some((_, off)), Some(base)) => (base.checked_add(off)?, 1, 0),
        _ => (src_gpu, 0, 0),
    };
    if !src_gpu.is_multiple_of(4) || !count_gpu.is_multiple_of(4) || !budget.is_multiple_of(4) {
        return None;
    }
    let (dst, at) = clamps.clamp_room(bytes)?;
    // SAFETY: a live buffer.
    let dst_base = unsafe { dst.GetGPUVirtualAddress() };
    // Mode 7 (the default deterministic ordered mode): queue into the list's
    // batched clamp shader, flushed in `clamp_end()` as one 64-lane SIMD
    // dispatch per contiguous buffer run instead of one dispatch + global UAV
    // barrier per draw.
    if mode == 7
        && batch_clamp().is_some()
        && let Ok(dst_offset) = u32::try_from(at)
    {
        let words = [
            kind,
            stride,
            records,
            args_offset,
            has_count,
            count_offset,
            dst_offset,
            0,
        ];
        if clamps.clamp_batch().push(
            src,
            count.map(|(b, _)| b),
            src_gpu,
            (has_count != 0).then_some(count_gpu),
            dst_base,
            words,
        ) {
            return Some((dst, at));
        }
    }
    // Non-7 diagnostic modes or fallback: flush any prior batched jobs first so
    // live mode switches preserve draw order.
    if flush_before(clamps)
        && let Some((budget_resource, _)) = clamps.budget_resource()
    {
        // SAFETY: an open list and live budget buffer; serialize the
        // following unbatched dispatch after the flushed batch's budget write.
        unsafe { clamps.list.ResourceBarrier(&[uav_barrier_of(Some(budget_resource))]) };
    }
    clamps.clamp_batch().had_unbatched = true;
    let c = clamp()?;
    let dst_gpu = dst_base + at;
    let constants = [kind, stride, records, args_offset, has_count, count_offset];
    let l = &clamps.list;
    // SAFETY: an open list; our root signature and pipeline; live source,
    // count, destination (writable: `clamp_room`) and budget buffers. The
    // clamps write apart and charge the budget atomically: no barriers.
    unsafe {
        l.SetComputeRootSignature(&c.root);
        l.SetPipelineState(&c.pso);
        l.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
        l.SetComputeRootShaderResourceView(1, src_gpu);
        l.SetComputeRootShaderResourceView(2, count_gpu);
        l.SetComputeRootUnorderedAccessView(3, dst_gpu);
        l.SetComputeRootUnorderedAccessView(4, budget);
        l.Dispatch(records.div_ceil(64), 1, 1);
        // Mode 7 fallback: one clamp after another, so that the budget is
        // charged in the order of the draws.
        if mode == 7 {
            l.ResourceBarrier(&[uav_barrier()]);
        }
    }
    Some((dst, at))
}

/// After indirect execution (or if preparation failed after [`bound`]):
/// returns the scratch buffer to the common state for the next call.
pub(crate) fn done(list: &HostList, scratch: &ID3D12Resource) {
    done_on(&list.list, scratch);
}

fn done_on(list: &ID3D12GraphicsCommandList, scratch: &ID3D12Resource) {
    // SAFETY: an open list and a live scratch buffer in INDIRECT_ARGUMENT.
    unsafe {
        list.ResourceBarrier(&[transition(
            scratch,
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            D3D12_RESOURCE_STATE_COMMON,
        )])
    };
}

fn uav_barrier() -> D3D12_RESOURCE_BARRIER {
    uav_barrier_of(None)
}

fn uav_barrier_of(resource: Option<&ID3D12Resource>) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                pResource: std::mem::ManuallyDrop::new(resource.map(|r| {
                    // SAFETY: a borrowed pointer; the barrier never releases it.
                    unsafe { std::mem::transmute_copy(r) }
                })),
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
    use windows::Win32::Graphics::Dxgi::*;
    use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
    use windows::core::Interface;

    const WORKER_CS: &str = r#"
#define RS "UAV(u0)"
RWByteAddressBuffer hits : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() {
    uint prev;
    hits.InterlockedAdd(0, 1, prev);
}
"#;

    struct WarpHarness {
        device: ID3D12Device,
        queue: ID3D12CommandQueue,
        allocator: ID3D12CommandAllocator,
        list: ID3D12GraphicsCommandList,
        fence: ID3D12Fence,
        submitted: u64,
        event: HANDLE,
        clamp: Clamp,
        worker: Clamp,
    }

    impl WarpHarness {
        fn new() -> Option<Self> {
            // SAFETY: plain DXGI WARP query and D3D12 test harness setup.
            unsafe {
                let factory: IDXGIFactory4 =
                    CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
                let adapter: IDXGIAdapter = factory.EnumWarpAdapter().ok()?;
                let desc = adapter.cast::<IDXGIAdapter1>().ok()?.GetDesc1().ok()?;
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
                    return None;
                }
                let mut device: Option<ID3D12Device> = None;
                D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
                let device = device?;
                let queue: ID3D12CommandQueue = device
                    .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                        Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                        ..Default::default()
                    })
                    .ok()?;
                let allocator: ID3D12CommandAllocator = device
                    .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                    .ok()?;
                let list: ID3D12GraphicsCommandList = device
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                    .ok()?;
                let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
                let event = CreateEventW(None, false, false, None).ok()?;
                let clamp = make_for(&device).expect("clamp shader must compile on WARP");
                let worker = compile_cs(&device, WORKER_CS).expect("worker shader must compile");
                Some(Self {
                    device,
                    queue,
                    allocator,
                    list,
                    fence,
                    submitted: 0,
                    event,
                    clamp,
                    worker,
                })
            }
        }

        /// CPU-mapped L0 write-back buffer matching `Shadow`'s heap type.
        fn shared_buffer(&self, size: u64) -> (ID3D12Resource, *mut u8, u64) {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_CUSTOM,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
                MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: size.max(4),
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                ..Default::default()
            };
            let mut buffer: Option<ID3D12Resource> = None;
            let mut data = std::ptr::null_mut();
            // SAFETY: plain buffer creation and persistent CPU mapping.
            unsafe {
                self.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        D3D12_RESOURCE_STATE_COMMON,
                        None,
                        &mut buffer,
                    )
                    .unwrap();
                let buffer = buffer.unwrap();
                buffer.Map(0, None, Some(&mut data)).unwrap();
                let gpu = buffer.GetGPUVirtualAddress();
                (buffer, data.cast(), gpu)
            }
        }

        /// Device-local `DEFAULT`-heap UAV scratch buffer.
        fn default_uav(&self, size: u64) -> ID3D12Resource {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: size.max(64 << 10),
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                ..Default::default()
            };
            let mut buffer: Option<ID3D12Resource> = None;
            // SAFETY: plain device-local buffer creation.
            unsafe {
                self.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        D3D12_RESOURCE_STATE_COMMON,
                        None,
                        &mut buffer,
                    )
                    .unwrap();
            }
            buffer.unwrap()
        }

        fn signature(&self, kind: u32, stride: u32) -> ID3D12CommandSignature {
            let arg = D3D12_INDIRECT_ARGUMENT_DESC {
                Type: D3D12_INDIRECT_ARGUMENT_TYPE(kind as i32),
                ..Default::default()
            };
            let desc = D3D12_COMMAND_SIGNATURE_DESC {
                ByteStride: stride,
                NumArgumentDescs: 1,
                pArgumentDescs: &arg,
                NodeMask: 0,
            };
            let mut sig: Option<ID3D12CommandSignature> = None;
            // SAFETY: valid single-argument command signature description.
            unsafe {
                self.device
                    .CreateCommandSignature(&desc, None::<&ID3D12RootSignature>, &mut sig)
                    .unwrap();
            }
            sig.unwrap()
        }

        fn submit_and_wait(&mut self) {
            self.submitted += 1;
            // SAFETY: closing, executing, waiting on fence, and resetting our list.
            unsafe {
                self.list.Close().unwrap();
                let cmd: ID3D12CommandList = self.list.cast().unwrap();
                self.queue.ExecuteCommandLists(&[Some(cmd)]);
                self.queue.Signal(&self.fence, self.submitted).unwrap();
                self.fence
                    .SetEventOnCompletion(self.submitted, self.event)
                    .unwrap();
                WaitForSingleObject(self.event, INFINITE);
                self.allocator.Reset().unwrap();
                self.list.Reset(&self.allocator, None).unwrap();
            }
        }
    }

    fn compile_cs(device: &ID3D12Device, source: &str) -> windows::core::Result<Clamp> {
        let mut code: Option<ID3DBlob> = None;
        // SAFETY: source and entry names outlive `D3DCompile`.
        unsafe {
            D3DCompile(
                source.as_ptr().cast(),
                source.len(),
                s!("worker"),
                None,
                None,
                s!("main"),
                s!("cs_5_1"),
                0,
                0,
                &mut code,
                None,
            )?;
            let code = code.unwrap();
            let bytes = std::slice::from_raw_parts(
                code.GetBufferPointer().cast::<u8>(),
                code.GetBufferSize(),
            );
            let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
            let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: bytes.as_ptr().cast(),
                    BytecodeLength: bytes.len(),
                },
                ..Default::default()
            })?;
            Ok(Clamp { root, pso })
        }
    }

    /// The clamp compiles with the system's compiler and the host takes it.
    #[test]
    fn argument_clamp_builds() {
        if super::host().is_none() {
            return; // no host GPU here
        }
        assert!(super::clamp().is_some());
    }

    #[test]
    fn rejects_unaligned_or_undersized_layouts() {
        assert_eq!(validate_layout((0, 16, 4), 0, None), Some(64));
        assert_eq!(validate_layout((1, 20, 4), 64, Some(128)), Some(80));
        assert_eq!(validate_layout((2, 12, 0), 0, Some(0)), Some(0));
        assert_eq!(validate_layout((10, 16, 2), 8, None), Some(32));
        // Undersized strides for each kind.
        assert_eq!(validate_layout((0, 12, 1), 0, None), None);
        assert_eq!(validate_layout((1, 16, 1), 0, None), None);
        assert_eq!(validate_layout((2, 8, 1), 0, None), None);
        assert_eq!(validate_layout((10, 8, 1), 0, None), None);
        // Unknown kind.
        assert_eq!(validate_layout((3, 16, 1), 0, None), None);
        // Unaligned stride, source offset, or count offset.
        assert_eq!(validate_layout((0, 18, 1), 0, None), None);
        assert_eq!(validate_layout((0, 16, 1), 2, None), None);
        assert_eq!(validate_layout((0, 16, 1), 0, Some(6)), None);
        // Total byte size exceeding 32-bit ByteAddressBuffer indexing.
        assert_eq!(validate_layout((1, 20, u32::MAX), 0, None), None);
    }

    /// WARP D3D12 execution test proving:
    /// (a) `countBuffer` with a small count (`3`) and `4,093` garbage tail slots
    ///     (`max = 4096`) does NOT blow `BUDGET` or zero valid dispatches in
    ///     this or subsequent `ExecuteIndirect` calls in the same list,
    /// (b) `src_args` (and `count_buf`) are NOT mutated in-place, and
    /// (c) full `stride` bytes (`stride = 24` > `12`) are copied for active
    ///     records into `dst_args` at offset `0`.
    #[test]
    fn warp_count_buffer_ignores_garbage_tail_and_preserves_source() {
        let Some(mut warp) = WarpHarness::new() else {
            return;
        };
        const KIND_DISPATCH: u32 = 2;
        const STRIDE: u32 = 24; // 6 dwords: 3 dispatch group counts + 3 extra payload dwords
        const MAX: u32 = 4096;
        const ARGS_OFFSET: usize = 64; // non-zero offset with sentinel prefix
        const COUNT_OFFSET_1: usize = 128;
        const COUNT_OFFSET_2: usize = 192;

        let src_bytes = ARGS_OFFSET + (STRIDE as usize) * (MAX as usize) + 64;
        let (src_buf, src_ptr, _) = warp.shared_buffer(src_bytes as u64);
        let (count_buf, count_ptr, _) = warp.shared_buffer(512);
        let (_budget_buf, budget_ptr, budget_gpu) = warp.shared_buffer(256);
        let (_hits_buf, hits_ptr, hits_gpu) = warp.shared_buffer(256);
        let (readback_buf, readback_ptr, _) = warp.shared_buffer((STRIDE * 8) as u64);
        let dst_buf = warp.default_uav(u64::from(STRIDE) * u64::from(MAX));
        let sig = warp.signature(KIND_DISPATCH, STRIDE);

        // Fill the entire source buffer with non-zero sentinel bytes, then
        // populate 3 valid records at `ARGS_OFFSET` and 4,093 huge garbage
        // records in `3..MAX` that would exhaust `BUDGET` ~2,000x over if read.
        let mut initial_src = vec![0xA5u8; src_bytes];
        let write_record = |buf: &mut [u8], idx: usize, words: [u32; 6]| {
            let base = ARGS_OFFSET + idx * (STRIDE as usize);
            for (w, val) in words.into_iter().enumerate() {
                buf[base + w * 4..base + (w + 1) * 4].copy_from_slice(&val.to_le_bytes());
            }
        };
        write_record(
            &mut initial_src,
            0,
            [3, 2, 1, 0x1111_2222, 0x3333_4444, 0x5555_6666],
        );
        write_record(
            &mut initial_src,
            1,
            [5, 1, 1, 0x7777_8888, 0x9999_AAAA, 0xBBBB_CCCC],
        );
        // Record 2 exceeds 65,535 in X -> clamped to (65535, 1, 1).
        write_record(
            &mut initial_src,
            2,
            [100_000, 1, 1, 0xDDDD_EEEE, 0xFFFF_0001, 0x0123_4567],
        );
        for slot in 3..MAX as usize {
            // Each garbage slot would clamp to 65535 * 16 * 1 = 1,048,560 groups
            // (`cost = 67,107,840 ~= BUDGET / 2`), exhausting `BUDGET` immediately!
            write_record(
                &mut initial_src,
                slot,
                [0xFFFF_FFFF, 16, 1, 0xDEAD_BEEF, 0xBAD0_C0DE, slot as u32],
            );
        }

        // Second indirect call's source records (at offset 0 of a second buffer,
        // or later in `count_buf`): put 2 active records + garbage tail in a
        // second range to run back-to-back in the SAME command list.
        let mut initial_count = vec![0x5Au8; 512];
        initial_count[COUNT_OFFSET_1..COUNT_OFFSET_1 + 4].copy_from_slice(&3u32.to_le_bytes());
        initial_count[COUNT_OFFSET_2..COUNT_OFFSET_2 + 4].copy_from_slice(&2u32.to_le_bytes());

        // SAFETY: buffers are mapped with exact capacities above.
        unsafe {
            std::ptr::copy_nonoverlapping(initial_src.as_ptr(), src_ptr, src_bytes);
            std::ptr::copy_nonoverlapping(initial_count.as_ptr(), count_ptr, 512);
            (budget_ptr as *mut u32).write_volatile(0);
            (hits_ptr as *mut u32).write_volatile(0);
            std::ptr::write_bytes(readback_ptr, 0, (STRIDE * 8) as usize);
        }

        // Record Call #1 (`eff_count = 3` out of `MAX = 4096`), copy the first
        // 5 records of `dst_buf` out for inspection, then record Call #2
        // (`eff_count = 2` out of `MAX = 4096`) reusing `dst_buf` in the same list!
        assert!(record_clamp(
            &warp.clamp,
            &warp.list,
            &dst_buf,
            budget_gpu,
            (&src_buf, ARGS_OFFSET as u64),
            Some((&count_buf, COUNT_OFFSET_1 as u64)),
            (KIND_DISPATCH, STRIDE, MAX),
        ));
        // SAFETY: open command list, valid worker PSO/root and buffers.
        unsafe {
            warp.list.SetComputeRootSignature(&warp.worker.root);
            warp.list.SetPipelineState(&warp.worker.pso);
            warp.list.SetComputeRootUnorderedAccessView(0, hits_gpu);
            warp.list.ExecuteIndirect(
                &sig,
                MAX,
                &dst_buf,
                0,
                Some(&count_buf),
                COUNT_OFFSET_1 as u64,
            );
        }
        done_on(&warp.list, &dst_buf);

        // Snapshot `dst_buf` after Call #1 (from `COMMON`, copy 5 * STRIDE bytes,
        // then return `dst_buf` to `COMMON` before Call #2).
        // SAFETY: open command list; `dst_buf` returned to `COMMON` by `done_on`.
        unsafe {
            warp.list.ResourceBarrier(&[transition(
                &dst_buf,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            warp.list
                .CopyBufferRegion(&readback_buf, 0, &dst_buf, 0, u64::from(STRIDE * 5));
            warp.list.ResourceBarrier(&[transition(
                &dst_buf,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_COMMON,
            )]);
        }

        // Call #2 in the SAME command list (`eff_count = 2` -> records 0 and 1:
        // 6 + 5 = 11 groups). Would run empty if Call #1's tail blew `BUDGET`!
        assert!(record_clamp(
            &warp.clamp,
            &warp.list,
            &dst_buf,
            budget_gpu,
            (&src_buf, ARGS_OFFSET as u64),
            Some((&count_buf, COUNT_OFFSET_2 as u64)),
            (KIND_DISPATCH, STRIDE, MAX),
        ));
        // SAFETY: open command list, valid worker PSO/root and buffers.
        unsafe {
            warp.list.SetComputeRootSignature(&warp.worker.root);
            warp.list.SetPipelineState(&warp.worker.pso);
            warp.list.SetComputeRootUnorderedAccessView(0, hits_gpu);
            warp.list.ExecuteIndirect(
                &sig,
                MAX,
                &dst_buf,
                0,
                Some(&count_buf),
                COUNT_OFFSET_2 as u64,
            );
        }
        done_on(&warp.list, &dst_buf);

        warp.submit_and_wait();

        // SAFETY: GPU execution finished; read mapped host results.
        let (hits, spent, after_src, after_count, clamped_words) = unsafe {
            let hits = (hits_ptr as *const u32).read_volatile();
            let spent = (budget_ptr as *const u32).read_volatile();
            let after_src = std::slice::from_raw_parts(src_ptr.cast_const(), src_bytes).to_vec();
            let after_count = std::slice::from_raw_parts(count_ptr.cast_const(), 512).to_vec();
            let words = std::slice::from_raw_parts(
                readback_ptr.cast::<u32>().cast_const(),
                (STRIDE as usize / 4) * 5,
            )
            .to_vec();
            (hits, spent, after_src, after_count, words)
        };

        // Call #1: (3*2*1) + (5*1*1) + (65535*1*1) = 65,546 groups.
        // Call #2: (3*2*1) + (5*1*1) = 11 groups.
        let expected_groups = (6 + 5 + 65_535) + (6 + 5);
        assert_eq!(
            hits, expected_groups,
            "valid indirect dispatches must all execute"
        );
        assert_eq!(
            spent,
            expected_groups * 64,
            "budget must reflect only active records below countBuffer"
        );
        assert_eq!(
            after_src, initial_src,
            "src_args must never be mutated in-place"
        );
        assert_eq!(
            after_count, initial_count,
            "count_buf must never be mutated"
        );

        // Check `dst_args` records 0..3: clamped group counts + preserved extra stride words.
        assert_eq!(
            &clamped_words[0..6],
            &[3, 2, 1, 0x1111_2222, 0x3333_4444, 0x5555_6666]
        );
        assert_eq!(
            &clamped_words[6..12],
            &[5, 1, 1, 0x7777_8888, 0x9999_AAAA, 0xBBBB_CCCC]
        );
        assert_eq!(
            &clamped_words[12..18],
            &[65_535, 1, 1, 0xDDDD_EEEE, 0xFFFF_0001, 0x0123_4567]
        );
        // Inactive slot 3 was never copied from `src_args`.
        assert_ne!(clamped_words[21], 0xDEAD_BEEF);
    }

    /// WARP D3D12 test proving `BUDGET` saturates without 32-bit unsigned
    /// wraparound across >64 huge indexed-draw records (`count = None`), while
    /// preserving `StartIndexLocation`, `BaseVertexLocation`,
    /// `StartInstanceLocation`, and custom stride padding, and leaving
    /// `src_args` untouched.
    #[test]
    fn warp_budget_saturates_without_wraparound_and_preserves_draw_fields() {
        let Some(mut warp) = WarpHarness::new() else {
            return;
        };
        const KIND_INDEXED: u32 = 1;
        const STRIDE: u32 = 24; // 5 indexed-draw words + 1 trailing custom word
        const RECORDS: u32 = 128; // 2 waves of 64: old InterlockedAdd wrapped 2^32 at 64!
        const BUDGET_CAP: u32 = 1 << 27;

        let total_bytes = (STRIDE * RECORDS) as usize;
        let (src_buf, src_ptr, _) = warp.shared_buffer(total_bytes as u64);
        let (_budget_buf, budget_ptr, budget_gpu) = warp.shared_buffer(256);
        let (readback_buf, readback_ptr, _) = warp.shared_buffer(total_bytes as u64);
        let dst_buf = warp.default_uav(total_bytes as u64);

        let mut initial_src = vec![0u8; total_bytes];
        for i in 0..RECORDS as usize {
            let words: [u32; 6] = [
                u32::MAX,                    // IndexCount -> clamps to 1 << 24
                u32::MAX,                    // InstanceCount -> clamps to 4 (cost = 1 << 26)
                1000 + i as u32,             // StartIndexLocation
                (-200i32 + i as i32) as u32, // BaseVertexLocation
                5000 + i as u32,             // StartInstanceLocation
                0xCAFE_0000 | (i as u32),    // Extra stride word
            ];
            let base = i * (STRIDE as usize);
            for (w, val) in words.into_iter().enumerate() {
                initial_src[base + w * 4..base + (w + 1) * 4].copy_from_slice(&val.to_le_bytes());
            }
        }

        // SAFETY: mapped buffers have sufficient capacity.
        unsafe {
            std::ptr::copy_nonoverlapping(initial_src.as_ptr(), src_ptr, total_bytes);
            (budget_ptr as *mut u32).write_volatile(0);
            std::ptr::write_bytes(readback_ptr, 0, total_bytes);
        }

        assert!(record_clamp(
            &warp.clamp,
            &warp.list,
            &dst_buf,
            budget_gpu,
            (&src_buf, 0),
            None,
            (KIND_INDEXED, STRIDE, RECORDS),
        ));
        done_on(&warp.list, &dst_buf);

        // SAFETY: open command list; copy clamped scratch buffer for verification.
        unsafe {
            warp.list.ResourceBarrier(&[transition(
                &dst_buf,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            warp.list
                .CopyBufferRegion(&readback_buf, 0, &dst_buf, 0, total_bytes as u64);
            warp.list.ResourceBarrier(&[transition(
                &dst_buf,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_COMMON,
            )]);
        }
        warp.submit_and_wait();

        // SAFETY: GPU work completed.
        let (spent, after_src, out_words) = unsafe {
            let spent = (budget_ptr as *const u32).read_volatile();
            let after_src = std::slice::from_raw_parts(src_ptr.cast_const(), total_bytes).to_vec();
            let out_words = std::slice::from_raw_parts(
                readback_ptr.cast::<u32>().cast_const(),
                total_bytes / 4,
            )
            .to_vec();
            (spent, after_src, out_words)
        };

        assert_eq!(
            spent, BUDGET_CAP,
            "budget must saturate at BUDGET without wrapping"
        );
        assert_eq!(after_src, initial_src, "src_args must remain untouched");

        let mut kept = 0usize;
        let mut zeroed = 0usize;
        for i in 0..RECORDS as usize {
            let rec = &out_words[i * 6..(i + 1) * 6];
            if rec[0] == (1 << 24) && rec[1] == 4 {
                kept += 1;
            } else {
                assert_eq!((rec[0], rec[1]), (0, 0), "excess draw must be zeroed");
                zeroed += 1;
            }
            assert_eq!(rec[2], 1000 + i as u32);
            assert_eq!(rec[3], (-200i32 + i as i32) as u32);
            assert_eq!(rec[4], 5000 + i as u32);
            assert_eq!(rec[5], 0xCAFE_0000 | (i as u32));
        }
        assert_eq!((kept, zeroed), (2, (RECORDS as usize) - 2));
    }

    /// Exercises `HostList::indirect_scratch`, `bound`, `done`, `Host::execute`,
    /// and `HostList::reset` end-to-end on the host device.
    #[test]
    fn host_list_reuses_and_grows_scratch_across_submissions() {
        let Some(host) = super::host() else {
            return;
        };
        let Ok(list) = host.new_list() else {
            return;
        };
        let Some((src_buf, src_ptr)) = host.shared_buffer(256 << 10) else {
            return;
        };
        let Some((count_buf, count_ptr)) = host.shared_buffer(256) else {
            return;
        };
        // SAFETY: mapped host buffers.
        unsafe {
            std::ptr::write_bytes(src_ptr, 0x7F, 256 << 10);
            (count_ptr as *mut u32).write_volatile(2);
        }

        // First two calls fit inside the initial 64 KiB scratch buffer and
        // must reuse the exact same `ID3D12Resource`.
        let b1 = bound(&list, (&src_buf, 0), Some((&count_buf, 0)), (0, 16, 64)).unwrap();
        done(&list, &b1);
        let b2 = bound(&list, (&src_buf, 64), Some((&count_buf, 0)), (1, 20, 128)).unwrap();
        done(&list, &b2);
        assert_eq!(
            b1.as_raw(),
            b2.as_raw(),
            "scratch buffer within capacity must be reused"
        );

        // A call needing >64 KiB grows the scratch buffer mid-list while keeping
        // `b1` alive until reset.
        let b3 = bound(&list, (&src_buf, 0), Some((&count_buf, 0)), (1, 20, 5000)).unwrap();
        done(&list, &b3);
        assert_ne!(
            b3.as_raw(),
            b1.as_raw(),
            "larger request must grow scratch buffer"
        );

        list.close();
        host.execute(&[&list]);
        list.reset();

        // After reset, requests up to the grown high-water mark reuse `b3` with
        // zero new allocations.
        let b4 = bound(&list, (&src_buf, 0), Some((&count_buf, 0)), (1, 20, 4096)).unwrap();
        done(&list, &b4);
        assert_eq!(
            b4.as_raw(),
            b3.as_raw(),
            "grown scratch buffer must persist across reset"
        );
        list.close();
        host.execute(&[&list]);
        list.reset();
    }

    #[test]
    fn batch_argument_clamp_builds() {
        if super::host().is_none() {
            return;
        }
        assert!(super::batch_clamp().is_some());
    }

    /// Helper: copies `len` bytes from `src` (`INDIRECT_ARGUMENT`) into
    /// `dst` (`COMMON` mapped shared buffer), leaving `src` in `INDIRECT_ARGUMENT`.
    fn copy_indirect_slice(
        list: &HostList,
        dst: &ID3D12Resource,
        dst_off: u64,
        src: &ID3D12Resource,
        src_off: u64,
        len: u64,
    ) {
        // SAFETY: open list; `src` is in `INDIRECT_ARGUMENT` after `clamp_end` / `bound`.
        unsafe {
            list.list.ResourceBarrier(&[transition(
                src,
                D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            list.list.CopyBufferRegion(dst, dst_off, src, src_off, len);
            list.list.ResourceBarrier(&[transition(
                src,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            )]);
        }
    }

    /// Proves batched `mode 7` (`bound_before_mode(..., 7)` + `clamp_end`)
    /// matches per-draw `bound()` byte-for-byte across >64 consecutive single-
    /// record draws, mixed kinds (`0, 1, 2, 10`), trailing custom stride words,
    /// `count == 0` / `count < max` / `count = None`, a multi-record (>64) job
    /// with garbage tail, and exact mid-wave `BUDGET` exhaustion — while
    /// leaving source and count buffers unmodified.
    #[test]
    fn batched_bound_before_matches_per_draw_mode7_with_counts_and_trailing_words() {
        let Some(host) = super::host() else {
            return;
        };
        let (Ok(clamps_batch), Ok(list_ref), Ok(copy_list)) =
            (host.new_list(), host.new_list(), host.new_list())
        else {
            return;
        };
        const TOTAL_SRC: usize = 64 << 10;
        let Some((src_buf, src_ptr)) = host.shared_buffer(TOTAL_SRC as u64) else {
            return;
        };
        let Some((count_buf, count_ptr)) = host.shared_buffer(1024) else {
            return;
        };
        let Some((rb_batch, rb_batch_ptr)) = host.shared_buffer(TOTAL_SRC as u64) else {
            return;
        };
        let Some((rb_ref, rb_ref_ptr)) = host.shared_buffer(TOTAL_SRC as u64) else {
            return;
        };

        let mut initial_src = vec![0xA5u8; TOTAL_SRC];
        let mut initial_count = vec![0x5Au8; 1024];
        let put_u32 = |buf: &mut [u8], off: usize, v: u32| {
            buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
        };
        // Count slots: 0 => 0 (inactive), 4 => 1 (active), 8 => 67 (< 70 multi-record).
        put_u32(&mut initial_count, 0, 0);
        put_u32(&mut initial_count, 4, 1);
        put_u32(&mut initial_count, 8, 67);

        struct JobSpec {
            src_off: u64,
            count_off: Option<u64>,
            kind: u32,
            stride: u32,
            records: u32,
            expect_written_bytes: usize,
        }
        let mut specs = Vec::new();
        let mut cursor = 64usize;

        // 1) 75 consecutive single-record jobs (spanning >1 full 64-lane wave):
        // mix kinds 0, 1, 2, 10, count=None / count=Some(1) / count=Some(0),
        // extra trailing custom words, and oversized inputs that get clamped.
        for i in 0..75u32 {
            let (kind, stride) = match i % 4 {
                0 => (0u32, 20u32),  // Draw + 1 extra word
                1 => (1u32, 24u32),  // DrawIndexed + 1 extra word
                2 => (2u32, 20u32),  // Dispatch + 2 extra words
                _ => (10u32, 16u32), // DispatchMesh + 1 extra word
            };
            let count_off = match i % 5 {
                0 => None,
                1 => Some(4u64), // count = 1
                2 => Some(0u64), // count = 0 (must NOT charge budget or overwrite)
                _ => None,
            };
            let w0 = if i % 5 == 2 {
                // Huge garbage in inactive count==0 slot: must not burn budget!
                u32::MAX
            } else {
                match kind {
                    0 | 1 => 10_000 + i * 17,
                    2 => 12 + (i % 7),
                    _ => 8 + (i % 5),
                }
            };
            let w1 = if i % 5 == 2 { u32::MAX } else { 3 };
            let w2 = if kind == 10 { 4 } else { 2 }; // mesh y clamps to 1
            put_u32(&mut initial_src, cursor, w0);
            put_u32(&mut initial_src, cursor + 4, w1);
            put_u32(&mut initial_src, cursor + 8, w2);
            put_u32(&mut initial_src, cursor + 12, 0x1000_0000 | i);
            if stride >= 20 {
                put_u32(&mut initial_src, cursor + 16, 0x2000_0000 | i);
            }
            if stride >= 24 {
                put_u32(&mut initial_src, cursor + 20, 0x3000_0000 | i);
            }
            let expect_written_bytes = if count_off == Some(0) { 0 } else { stride as usize };
            specs.push(JobSpec {
                src_off: cursor as u64,
                count_off,
                kind,
                stride,
                records: 1,
                expect_written_bytes,
            });
            cursor += stride as usize;
        }

        // 2) Multi-record job (`records = 70`, `count = 67`) with garbage tail 67..70.
        let multi_stride = 24u32;
        let multi_records = 70u32;
        let multi_off = cursor;
        for r in 0..multi_records {
            let base = multi_off + (r * multi_stride) as usize;
            if r < 67 {
                put_u32(&mut initial_src, base, 4096 + r);
                put_u32(&mut initial_src, base + 4, 2);
                put_u32(&mut initial_src, base + 8, 100 + r);
                put_u32(&mut initial_src, base + 12, (-50i32 + r as i32) as u32);
                put_u32(&mut initial_src, base + 16, 200 + r);
                put_u32(&mut initial_src, base + 20, 0xABCD_0000 | r);
            } else {
                // Garbage tail that would blow BUDGET if read!
                put_u32(&mut initial_src, base, u32::MAX);
                put_u32(&mut initial_src, base + 4, u32::MAX);
                put_u32(&mut initial_src, base + 8, 0xDEAD_BEEF);
                put_u32(&mut initial_src, base + 12, 0xDEAD_BEEF);
                put_u32(&mut initial_src, base + 16, 0xDEAD_BEEF);
                put_u32(&mut initial_src, base + 20, 0xDEAD_BEEF);
            }
        }
        specs.push(JobSpec {
            src_off: multi_off as u64,
            count_off: Some(8),
            kind: 1,
            stride: multi_stride,
            records: multi_records,
            expect_written_bytes: (67 * multi_stride) as usize,
        });
        cursor += (multi_records * multi_stride) as usize;

        // 3) 80 more single-record indexed draws that intentionally exhaust
        // `BUDGET = 1 << 27` mid-wave (each costs 4,000,000 -> ~33 kept, rest zeroed),
        // plus trailing custom words that must survive budget zeroing.
        for i in 0..80u32 {
            let stride = 24u32;
            put_u32(&mut initial_src, cursor, 1_000_000);
            put_u32(&mut initial_src, cursor + 4, 4);
            put_u32(&mut initial_src, cursor + 8, 700 + i);
            put_u32(&mut initial_src, cursor + 12, 800 + i);
            put_u32(&mut initial_src, cursor + 16, 900 + i);
            put_u32(&mut initial_src, cursor + 20, 0xFACE_0000 | i);
            specs.push(JobSpec {
                src_off: cursor as u64,
                count_off: None,
                kind: 1,
                stride,
                records: 1,
                expect_written_bytes: stride as usize,
            });
            cursor += stride as usize;
        }

        // SAFETY: mapped host buffers with exact capacities above.
        unsafe {
            std::ptr::copy_nonoverlapping(initial_src.as_ptr(), src_ptr, TOTAL_SRC);
            std::ptr::copy_nonoverlapping(initial_count.as_ptr(), count_ptr, 1024);
            std::ptr::write_bytes(rb_batch_ptr, 0xCC, TOTAL_SRC);
            std::ptr::write_bytes(rb_ref_ptr, 0xCC, TOTAL_SRC);
        }

        // Record batched mode 7 path on `clamps_batch`, then copy every job's
        // output slice out on `copy_list`.
        let mut batch_locs = Vec::with_capacity(specs.len());
        let mut rb_cursor = 0u64;
        for s in &specs {
            let count_arg = s.count_off.map(|off| (&count_buf, off));
            let (dst, at) = bound_before_mode(
                &clamps_batch,
                (&src_buf, s.src_off),
                count_arg,
                (s.kind, s.stride, s.records),
                7,
            )
            .expect("batched bound_before_mode(7) must succeed");
            batch_locs.push((dst, at, rb_cursor, s.expect_written_bytes));
            rb_cursor += u64::from(s.stride * s.records);
        }
        clamps_batch.clamp_end();
        clamps_batch.close();
        for (dst, at, rb_off, len) in &batch_locs {
            if *len > 0 {
                copy_indirect_slice(&copy_list, &rb_batch, *rb_off, dst, *at, *len as u64);
            }
        }
        copy_list.close();

        // Record reference per-draw `bound()` path on `list_ref`.
        let mut ref_rb_cursor = 0u64;
        for s in &specs {
            let count_arg = s.count_off.map(|off| (&count_buf, off));
            let scratch = bound(
                &list_ref,
                (&src_buf, s.src_off),
                count_arg,
                (s.kind, s.stride, s.records),
            )
            .expect("reference bound() must succeed");
            if s.expect_written_bytes > 0 {
                copy_indirect_slice(
                    &list_ref,
                    &rb_ref,
                    ref_rb_cursor,
                    &scratch,
                    0,
                    s.expect_written_bytes as u64,
                );
            }
            done(&list_ref, &scratch);
            ref_rb_cursor += u64::from(s.stride * s.records);
        }
        list_ref.close();

        host.execute(&[&clamps_batch, &copy_list, &list_ref]);

        // SAFETY: GPU work completed; inspect readback, source, and count buffers.
        let (out_batch, out_ref, after_src, after_count) = unsafe {
            let total = rb_cursor as usize;
            let b = std::slice::from_raw_parts(rb_batch_ptr.cast_const(), total).to_vec();
            let r = std::slice::from_raw_parts(rb_ref_ptr.cast_const(), total).to_vec();
            let s = std::slice::from_raw_parts(src_ptr.cast_const(), TOTAL_SRC).to_vec();
            let c = std::slice::from_raw_parts(count_ptr.cast_const(), 1024).to_vec();
            (b, r, s, c)
        };

        assert_eq!(after_src, initial_src, "src_args must never be mutated");
        assert_eq!(after_count, initial_count, "count_buf must never be mutated");
        assert_eq!(
            out_batch, out_ref,
            "batched mode 7 output must match per-draw reference byte-for-byte"
        );

        // Sanity-check that budget actually saturated mid-tail and preserved trailing words.
        let (_, _, tail_start, _) = batch_locs[76];
        let mut tail_kept = 0usize;
        let mut tail_zeroed = 0usize;
        for i in 0..80usize {
            let off = tail_start as usize + i * 24;
            let c0 = u32::from_le_bytes(out_batch[off..off + 4].try_into().unwrap());
            let c1 = u32::from_le_bytes(out_batch[off + 4..off + 8].try_into().unwrap());
            let extra = u32::from_le_bytes(out_batch[off + 20..off + 24].try_into().unwrap());
            assert_eq!(extra, 0xFACE_0000 | (i as u32));
            if c0 == 1_000_000 && c1 == 4 {
                tail_kept += 1;
            } else {
                assert_eq!((c0, c1), (0, 0));
                tail_zeroed += 1;
            }
        }
        assert!(tail_kept > 0 && tail_zeroed > 0, "tail must cross BUDGET mid-wave");

        clamps_batch.reset();
        copy_list.reset();
        list_ref.reset();
    }

    /// Proves multi-chunk (>256 KiB `clamp_room`), multi-source, multi-count
    /// batching preserves exact draw-order budget saturation across dispatch
    /// runs (via scoped `UAV(budget)` barriers) and reuses its upload buffer
    /// cleanly across `HostList::reset()`.
    #[test]
    fn batched_bound_before_preserves_budget_order_across_chunks_and_buffers() {
        let Some(host) = super::host() else {
            return;
        };
        let (Ok(clamps), Ok(copy_list)) = (host.new_list(), host.new_list()) else {
            return;
        };
        let Some((src_a, src_a_ptr)) = host.shared_buffer(4096) else {
            return;
        };
        let Some((src_b, src_b_ptr)) = host.shared_buffer(4096) else {
            return;
        };
        let Some((cnt_a, cnt_a_ptr)) = host.shared_buffer(256) else {
            return;
        };
        let Some((cnt_b, cnt_b_ptr)) = host.shared_buffer(256) else {
            return;
        };
        let Some((rb, rb_ptr)) = host.shared_buffer(512) else {
            return;
        };

        // Each record in `src_a` / `src_b` has cost 16,777,216 (= BUDGET / 8),
        // so exactly 8 active single-record draws fit in `BUDGET = 1 << 27`
        // and draw #9 onward must run empty!
        let write_draw = |ptr: *mut u8, off: usize, count: u32, inst: u32, tag: u32| {
            // SAFETY: within 4096-byte mapped shared buffer.
            unsafe {
                let p = ptr.add(off).cast::<u32>();
                p.write(count);
                p.add(1).write(inst);
                p.add(2).write(tag);
                p.add(3).write(0);
                p.add(4).write(0);
            }
        };
        // SAFETY: mapped 256-byte count buffers.
        unsafe {
            (cnt_a_ptr as *mut u32).write(1);
            (cnt_b_ptr as *mut u32).write(0); // cnt_b is 0 -> inactive!
        }
        // Padding draws of cost 0 (`count = 0, inst = 1`) to force chunk rollover
        // past 256 KiB (`8,192` × 32-byte aligned slots = 256 KiB).
        write_draw(src_a_ptr, 0, 0, 1, 0xAAAA);
        // Active budget-charging draws (`cost = 1 << 24 = BUDGET / 8`).
        write_draw(src_a_ptr, 32, 1 << 22, 4, 0x1111);
        write_draw(src_b_ptr, 32, 1 << 22, 4, 0x2222);

        // Run 2 full submission + reset cycles to verify upload & chunk reuse.
        for cycle in 0..2u32 {
            // 1) Push 4 active draws alternating `src_a` (with `cnt_a = 1`) and
            //    `src_b` (uncounted), plus an inactive `cnt_b = 0` draw that must
            //    not consume budget.
            let mut probes = Vec::new();
            for i in 0..4u32 {
                let (src, cnt) = if i % 2 == 0 {
                    (&src_a, Some((&cnt_a, 0)))
                } else {
                    (&src_b, None)
                };
                probes.push(bound_before_mode(&clamps, (src, 32), cnt, (1, 20, 1), 7).unwrap());
            }
            // Inactive draw with `cnt_b = 0` (must NOT consume budget).
            let _inactive =
                bound_before_mode(&clamps, (&src_a, 32), Some((&cnt_b, 0)), (1, 20, 1), 7).unwrap();

            // 2) Push 8,250 zero-cost draws to spill across the 256 KiB chunk
            //    boundary into chunk #2.
            let mut last_chunk_ptr = probes[0].0.as_raw();
            let mut spilled = false;
            for _ in 0..8_250 {
                let (dst, _) =
                    bound_before_mode(&clamps, (&src_a, 0), None, (1, 20, 1), 7).unwrap();
                if dst.as_raw() != last_chunk_ptr {
                    spilled = true;
                    last_chunk_ptr = dst.as_raw();
                }
            }
            assert!(spilled, "must span at least two 256 KiB clamp_room chunks");

            // 3) Push 6 more active draws in chunk #2: draws #5..#8 (indices 4..8)
            //    must be KEPT (`8 * BUDGET/8 == BUDGET`), and draws #9..#10
            //    (indices 8..10) must be ZEROED!
            for i in 0..6u32 {
                let src = if i % 2 == 0 { &src_a } else { &src_b };
                probes.push(bound_before_mode(&clamps, (src, 32), None, (1, 20, 1), 7).unwrap());
            }

            clamps.clamp_end();
            clamps.close();

            for (idx, (dst, at)) in probes.iter().enumerate() {
                copy_indirect_slice(&copy_list, &rb, (idx * 20) as u64, dst, *at, 20);
            }
            copy_list.close();

            host.execute(&[&clamps, &copy_list]);

            for idx in 0..10usize {
                // SAFETY: GPU completed; read 5 dwords per probe.
                let words: [u32; 5] = unsafe {
                    let p = rb_ptr.add(idx * 20).cast::<u32>();
                    [p.read(), p.add(1).read(), p.add(2).read(), p.add(3).read(), p.add(4).read()]
                };
                if idx < 8 {
                    assert_eq!(
                        (words[0], words[1]),
                        (1 << 22, 4),
                        "cycle {cycle} probe {idx} must fit in BUDGET"
                    );
                } else {
                    assert_eq!(
                        (words[0], words[1]),
                        (0, 0),
                        "cycle {cycle} probe {idx} must be zeroed past BUDGET"
                    );
                }
            }

            clamps.reset();
            copy_list.reset();
        }
    }

    /// Proves interleaving batched `mode 7` with an unbatched diagnostic mode
    /// (`mode 1`, e.g. from a live `VICTORY_HOIST_FILE` switch) flushes prior
    /// jobs in order, keeps replaced upload buffers alive when grown after a
    /// mid-list flush, and preserves exact draw-order budget saturation.
    #[test]
    fn batched_bound_before_interleaves_cleanly_with_non7_mode_switch() {
        let Some(host) = super::host() else {
            return;
        };
        let (Ok(clamps), Ok(copy_list)) = (host.new_list(), host.new_list()) else {
            return;
        };
        let Some((src, src_ptr)) = host.shared_buffer(1024) else {
            return;
        };
        let Some((rb, rb_ptr)) = host.shared_buffer(512) else {
            return;
        };
        // Record at offset 0: cost = 1 << 25 (= BUDGET / 4).
        // Record at offset 32: cost = 0 (padding to grow upload buffer past 256 jobs).
        // SAFETY: within 1024-byte mapped buffer.
        unsafe {
            let p = src_ptr.cast::<u32>();
            p.write(1 << 23);
            p.add(1).write(4);
            p.add(2).write(0x1234);
            p.add(3).write(0);
            p.add(4).write(0);
            let z = src_ptr.add(32).cast::<u32>();
            z.write(0);
            z.add(1).write(1);
            z.add(2).write(0x5678);
            z.add(3).write(0);
            z.add(4).write(0);
        }

        // 2 batched mode 7 draws (uses 2/4 of BUDGET).
        let p0 = bound_before_mode(&clamps, (&src, 0), None, (1, 20, 1), 7).unwrap();
        let p1 = bound_before_mode(&clamps, (&src, 0), None, (1, 20, 1), 7).unwrap();
        // Switch to unbatched mode 1: must flush p0/p1 first, then consume 3/4 of BUDGET.
        let p2 = bound_before_mode(&clamps, (&src, 0), None, (1, 20, 1), 1).unwrap();
        // Switch back to mode 7: push 300 zero-cost jobs to force `JobUpload` reallocation
        // while `flushed > 0` (keeping old upload buffer alive!), then 2 budget draws
        // (p3 gets 4/4 of BUDGET, p4 overflows -> zeroed).
        for _ in 0..300 {
            let _ = bound_before_mode(&clamps, (&src, 32), None, (1, 20, 1), 7).unwrap();
        }
        let p3 = bound_before_mode(&clamps, (&src, 0), None, (1, 20, 1), 7).unwrap();
        let p4 = bound_before_mode(&clamps, (&src, 0), None, (1, 20, 1), 7).unwrap();

        clamps.clamp_end();
        clamps.close();

        for (i, (dst, at)) in [p0, p1, p2, p3, p4].iter().enumerate() {
            copy_indirect_slice(&copy_list, &rb, (i * 20) as u64, dst, *at, 20);
        }
        copy_list.close();
        host.execute(&[&clamps, &copy_list]);

        for i in 0..5usize {
            // SAFETY: GPU work finished.
            let (c0, c1, tag) = unsafe {
                let p = rb_ptr.add(i * 20).cast::<u32>();
                (p.read(), p.add(1).read(), p.add(2).read())
            };
            assert_eq!(tag, 0x1234);
            if i < 4 {
                assert_eq!((c0, c1), (1 << 23, 4), "draw {i} must fit in BUDGET");
            } else {
                assert_eq!((c0, c1), (0, 0), "draw {i} must be zeroed past BUDGET");
            }
        }
        clamps.reset();
        copy_list.reset();
    }

    /// Release benchmark comparing Belobug's previous per-draw `mode 7`
    /// hoisted clamps (9,000 dispatches + 9,000 global UAV barriers) against
    /// batched `mode 7` (2 dispatches + 1 scoped budget UAV barrier).
    ///
    /// Run with:
    /// `cargo test --release -p victory-gpu --locked -- --ignored --nocapture benchmark_batched_vs_per_draw_mode7_clamps`
    #[test]
    #[ignore = "benchmark"]
    fn benchmark_batched_vs_per_draw_mode7_clamps() {
        let Some(host) = super::host() else {
            return;
        };
        let (Ok(clamps_old), Ok(clamps_new)) = (host.new_list(), host.new_list()) else {
            return;
        };
        let Some(c_old) = clamp() else {
            return;
        };
        assert!(batch_clamp().is_some());

        const DRAWS: usize = 9_000;
        const STRIDE: u32 = 20;
        let src_bytes = (DRAWS * STRIDE as usize) as u64;
        let Some((src_buf, src_ptr)) = host.shared_buffer(src_bytes) else {
            return;
        };
        // SAFETY: mapped `src_bytes` buffer.
        let src_base = unsafe {
            for i in 0..DRAWS {
                let p = src_ptr.add(i * STRIDE as usize).cast::<u32>();
                p.write(300);
                p.add(1).write(1);
                p.add(2).write(0);
                p.add(3).write(0);
                p.add(4).write(0);
            }
            src_buf.GetGPUVirtualAddress()
        };

        let run_old = || {
            let budget = clamps_old.budget().unwrap();
            let l = &clamps_old.list;
            for i in 0..DRAWS {
                let (dst, at) = clamps_old.clamp_room(u64::from(STRIDE)).unwrap();
                // SAFETY: open list, live clamp pipeline and buffers.
                unsafe {
                    let dst_gpu = dst.GetGPUVirtualAddress() + at;
                    let constants = [1u32, STRIDE, 1, (i as u32) * STRIDE, 0, 0];
                    l.SetComputeRootSignature(&c_old.root);
                    l.SetPipelineState(&c_old.pso);
                    l.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
                    l.SetComputeRootShaderResourceView(1, src_base);
                    l.SetComputeRootShaderResourceView(2, src_base);
                    l.SetComputeRootUnorderedAccessView(3, dst_gpu);
                    l.SetComputeRootUnorderedAccessView(4, budget);
                    l.Dispatch(1, 1, 1);
                    l.ResourceBarrier(&[uav_barrier()]);
                }
            }
            clamps_old.clamp_end();
            clamps_old.close();
            host.execute(&[&clamps_old]);
            clamps_old.reset();
        };

        let run_new = || {
            for i in 0..DRAWS {
                let _ = bound_before_mode(
                    &clamps_new,
                    (&src_buf, (i * STRIDE as usize) as u64),
                    None,
                    (1, STRIDE, 1),
                    7,
                )
                .unwrap();
            }
            clamps_new.clamp_end();
            clamps_new.close();
            host.execute(&[&clamps_new]);
            clamps_new.reset();
        };

        for _ in 0..3 {
            run_old();
            run_new();
        }
        const ITERS: u32 = 20;
        let t0 = std::time::Instant::now();
        for _ in 0..ITERS {
            run_old();
        }
        let old_ms = t0.elapsed().as_secs_f64() * 1e3 / f64::from(ITERS);

        let t1 = std::time::Instant::now();
        for _ in 0..ITERS {
            run_new();
        }
        let new_ms = t1.elapsed().as_secs_f64() * 1e3 / f64::from(ITERS);

        eprintln!(
            "{DRAWS} hoisted mode-7 indirect draw clamps: per-draw {old_ms:.2} ms/frame vs batched {new_ms:.2} ms/frame ({:.1}x)",
            old_ms / new_ms.max(1e-6)
        );
    }
}

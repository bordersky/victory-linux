//! End-to-end and stage-by-stage timing of the ordinary shader translation
//! pipeline on compact, deterministic synthetic GFX10 programs (no title files
//! or GPU required).
//!
//! Scope & limits: synthetic CS/PS workloads exercising ALU-heavy straight-line
//! code, divergent CFG, guarded vector runs, SGPR-to-VGPR spills, and pixel
//! interpolation. Does not represent real title shader corpora, VS/MS/GS
//! stages, DXBC/cache decompression, or host D3D12 PSO creation.

use std::hint::black_box;
use std::time::{Duration, Instant};

use victory_shader::native::{
    ComputeIo, ExportPatch, HwStage, NativeProgram, PsInput, ResourceType, RootParam, RootType,
    StageIo, Usage, UsedResource,
};
use victory_shader::{Container, emit, gfx10, io, ir, resource, sgpr_spill, structure};

const S_ENDPGM: u32 = 0xBF81_0000;
const fn sop1(op: u8, d: u8, s: u8) -> u32 {
    0xBE80_0000 | (d as u32) << 16 | (op as u32) << 8 | s as u32
}
const fn sop2(op: u8, d: u8, s0: u8, s1: u8) -> u32 {
    0x8000_0000 | (op as u32 & 0x7F) << 23 | (d as u32 & 0x7F) << 16 | (s1 as u32) << 8 | s0 as u32
}
const fn sopp(op: u8, simm16: i16) -> u32 {
    0xBF80_0000 | (op as u32) << 16 | (simm16 as u16 as u32)
}
const fn vop2(op: u8, d: u8, s0: u16, v1: u8) -> u32 {
    (op as u32) << 25 | (d as u32) << 17 | (v1 as u32) << 9 | (s0 as u32 & 0x1FF)
}
const fn vopc(op: u8, s0: u16, v1: u8) -> u32 {
    0x7C00_0000 | (op as u32) << 17 | (v1 as u32) << 9 | (s0 as u32 & 0x1FF)
}
const fn s_load_x4(d: u8, base: u8, off: u32) -> [u32; 2] {
    [0xF408_0000 | (d as u32) << 6 | (base as u32 >> 1), (125 << 25) | (off & 0x1F_FFFF)]
}
const fn buf_op(op: u8, v: u8, rsrc: u8, off: u16) -> [u32; 2] {
    [
        0xE000_0000 | (op as u32) << 18 | (off as u32 & 0xFFF),
        0x8000_0000 | ((rsrc as u32 >> 2) & 0x1F) << 16 | (v as u32) << 8,
    ]
}

fn make_cs(name: &'static str, w: &[u32], wave64: bool, vgprs: u16, uavs: usize) -> (&'static str, NativeProgram) {
    let used = (0..uavs)
        .map(|k| UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: (k * 8) as u32,
            lower_bound: k as u32,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        })
        .collect();
    let p = NativeProgram {
        stage: HwStage::Cs,
        wave64,
        vgprs,
        layout: vec![
            RootParam { root: RootType::Constants, table_offset_dwords: 0, sgprs: 4, base_register: 0, user_data: 0, slot: 0 },
            RootParam { root: RootType::Table, table_offset_dwords: 0, sgprs: 1, base_register: 0, user_data: 4, slot: 1 },
        ],
        used,
        io: StageIo::Cs(ComputeIo { rsrc2: 5 << 1, threads: [if wave64 { 64 } else { 32 }, 1, 1] }),
        code: w.iter().flat_map(|x| x.to_le_bytes()).collect(),
    };
    (name, p)
}

fn cs_head(w: &mut Vec<u32>, vgprs: u8, descs: usize) {
    w.push(sop1(0x03, 5, 0x80)); // s_mov_b32 s5, 0
    for k in 0..descs {
        w.extend(s_load_x4(8 + (k as u8 % 12) * 4, 4, (k * 32) as u32));
    }
    for v in 1..vgprs {
        w.push(vop2(0x25, v, 128 + (v as u16 % 32), 0));
    }
}

fn cs_tail(w: &mut Vec<u32>, vgprs: u8) {
    for v in 1..vgprs {
        w.push(vop2(0x25, 0, 256 + v as u16, 0));
    }
    w.extend(buf_op(0x1C, 0, 8, 0)); // buffer_store_dword
    w.push(S_ENDPGM);
}

fn alu_heavy() -> (&'static str, NativeProgram) {
    let mut w = Vec::new();
    cs_head(&mut w, 32, 4);
    for i in 0..2000usize {
        let (d, a, b) = (1 + (i % 31) as u8, 1 + ((i + 3) % 31) as u8, 1 + ((i + 7) % 31) as u8);
        let op = [0x25, 0x03, 0x08, 0x1D][i % 4];
        w.push(vop2(op, d, 256 + a as u16, b));
        if i % 128 == 64 {
            w.extend(buf_op(0x0C, d, 8 + ((i / 128) % 4) as u8 * 4, (i % 256) as u16 * 4));
        }
    }
    cs_tail(&mut w, 32);
    make_cs("alu_heavy_2000", &w, false, 32, 4)
}

fn divergent_cfg() -> (&'static str, NativeProgram) {
    let mut w = Vec::new();
    cs_head(&mut w, 32, 2);
    for d in 0..64usize {
        w.push(vopc(0xC4, 128 + (d % 16) as u16, 1 + (d % 31) as u8));
        w.push(sop1(0x3C, 60, 106)); // s_and_saveexec_b32 s60, vcc_lo
        w.push(sopp(0x08, 16)); // s_cbranch_execz +16
        for k in 0..16usize {
            let (dst, src) = (1 + ((d + k) % 31) as u8, 1 + ((d + k + 5) % 31) as u8);
            w.push(vop2(0x25, dst, 256 + src as u16, dst));
        }
        w.push(sop2(0x10, 126, 126, 60)); // s_or_b32 exec_lo, exec_lo, s60
    }
    cs_tail(&mut w, 32);
    make_cs("divergent_cfg_64x16", &w, false, 32, 2)
}

fn guarded_runs() -> (&'static str, NativeProgram) {
    let mut w = Vec::new();
    cs_head(&mut w, 32, 1);
    for m in 0..16usize {
        w.push(vopc(0xD2, 128, 1 + (m % 16) as u8)); // v_cmpx_eq_u32
        for k in 0..64usize {
            let (dst, a) = (1 + ((m + k) % 31) as u8, 1 + ((m + k + 3) % 31) as u8);
            w.push(vop2(0x25, dst, 256 + a as u16, dst));
        }
    }
    cs_tail(&mut w, 32);
    make_cs("guarded_runs_16x64", &w, true, 32, 1)
}

fn sgpr_spill_heavy() -> (&'static str, NativeProgram) {
    let mut w = Vec::new();
    cs_head(&mut w, 24, 2);
    w.push(0x7E00_0280 | (23u32 << 17)); // v_mov_b32 v23, 0
    for r in 0..24usize {
        let lo = ((r * 2) % 30) as u32;
        w.push(vopc(0xC4, 132, 1 + (r % 20) as u8));
        w.push(sop1(0x24, 60, 106)); // s_and_saveexec_b64 s[60:61], vcc
        w.extend([0xD761_0017, 60 | ((128 + lo) << 9)]); // v_writelane_b32 v23, s60, lo
        w.extend([0xD761_0017, 61 | ((129 + lo) << 9)]); // v_writelane_b32 v23, s61, lo+1
        w.push(sop1(0x04, 60, 128)); // s_mov_b64 s[60:61], 0
        w.push(sopp(0x08, 12));
        for k in 0..12usize {
            let dst = 1 + ((r + k) % 20) as u8;
            w.push(vop2(0x25, dst, 129, dst));
        }
        w.extend([0xD760_003E, (256 + 23) | ((128 + lo) << 9)]); // v_readlane_b32 s62, v23, lo
        w.extend([0xD760_003F, (256 + 23) | ((129 + lo) << 9)]); // v_readlane_b32 s63, v23, lo+1
        w.push(sop2(0x11, 126, 126, 62)); // s_or_b64 exec, exec, s[62:63]
    }
    cs_tail(&mut w, 24);
    make_cs("sgpr_spill_24x12", &w, true, 24, 2)
}

fn pixel_interp() -> (&'static str, NativeProgram) {
    let mut w = Vec::new();
    for attr in 0..2u32 {
        for ch in 0..4u32 {
            let d = 2 + attr * 4 + ch;
            w.push(0xC800_0000 | (d << 18) | (attr << 10) | (ch << 8)); // v_interp_p1_f32
            w.push(0xC801_0001 | (d << 18) | (attr << 10) | (ch << 8)); // v_interp_p2_f32
        }
    }
    for v in 10..32u8 {
        w.push(vop2(0x03, v, 256 + (2 + (v % 8)) as u16, 2 + ((v + 3) % 8)));
    }
    for d in 0..32usize {
        w.push(vopc(0xC4, 128 + (d % 16) as u16, 2 + (d % 30) as u8));
        w.push(sop1(0x3C, 60, 106));
        w.push(sopp(0x08, 16));
        for k in 0..16usize {
            let (dst, src) = (2 + ((d + k) % 30) as u8, 2 + ((d + k + 5) % 30) as u8);
            w.push(vop2(0x03, dst, 256 + src as u16, dst));
        }
        w.push(sop2(0x10, 126, 126, 60));
    }
    for v in 6..32u8 {
        w.push(vop2(0x03, 2 + (v % 4), 256 + v as u16, 2 + (v % 4)));
    }
    w.extend([0xF800_180F, 0x0504_0302, S_ENDPGM]);
    let ps_in = |row, attr| PsInput {
        usage: Usage::Generic, row, mask: 0xF, attr, flat: false, fp16: false, pack2: false, row2: row,
    };
    let p = NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 32,
        layout: vec![],
        used: vec![],
        io: StageIo::Ps {
            inputs: vec![ps_in(1, 0), ps_in(2, 1)],
            input_addr: 1 << 1,
            db_shader_control: 0,
            patches: Vec::<ExportPatch>::new(),
        },
        code: w.iter().flat_map(|x| x.to_le_bytes()).collect(),
    };
    ("pixel_interp_32x16", p)
}

fn dxbc_container(parts: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut off = 32 + 4 * parts.len();
    let mut offsets = Vec::with_capacity(parts.len());
    for (_, data) in parts {
        offsets.push(off as u32);
        off += 8 + data.len();
    }
    let mut out = Vec::with_capacity(off);
    out.extend_from_slice(b"DXBC");
    out.extend_from_slice(&[0u8; 16]);
    for v in [1u32, off as u32, parts.len() as u32] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for o in offsets {
        out.extend_from_slice(&o.to_le_bytes());
    }
    for (fourcc, data) in parts {
        out.extend_from_slice(fourcc);
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
    }
    out
}

fn sig_part(rows: &[(&str, u32, u32, u32)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    out.extend_from_slice(&8u32.to_le_bytes());
    let mut name_off = 8 + 32 * rows.len() as u32;
    let mut names = Vec::new();
    for &(name, idx, sys, reg) in rows {
        for v in [0u32, name_off, idx, sys, 3, reg] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&[0xF, 0xF, 0, 0, 0, 0, 0, 0]);
        name_off += name.len() as u32 + 1;
        names.extend_from_slice(name.as_bytes());
        names.push(0);
    }
    out.extend_from_slice(&names);
    out
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn main() {
    let cases = [alu_heavy(), divergent_cfg(), guarded_runs(), sgpr_spill_heavy(), pixel_interp()];
    let cs_bytes = dxbc_container(&[]);
    let ps_bytes = dxbc_container(&[
        (*b"ISG1", sig_part(&[("TEXCOORD", 0, 0, 1), ("TEXCOORD", 1, 0, 2)])),
        (*b"OSG1", sig_part(&[("SV_Target", 0, 64, 0)])),
    ]);
    let cs_c = Container::parse(&cs_bytes).unwrap();
    let ps_c = Container::parse(&ps_bytes).unwrap();

    const SAMPLES: usize = 11;
    const ITERS: u32 = 16;
    println!("translate_pipeline: {SAMPLES} samples x {ITERS} iters/sample (release, synthetic CS/PS workloads)");
    println!("note: e2e_total = ir_bld..emit (6 additive stages); [decode] (inside ir_bld) and [dead] (inside emit) are standalone sub-timings");
    println!(
        "{:<21} {:>5} {:>4} {:>7} | {:>9} ({:>6}..{:>6}) | {:>6} {:>5} {:>4} {:>6} {:>5} {:>6} | {:>8} {:>6} | fnv1a_64",
        "workload", "insts", "blks", "dxil_B", "e2e_total", "min", "max", "ir_bld", "spill", "io", "struct", "resrc", "emit", "[decode]", "[dead]"
    );
    println!("{}", "-".repeat(162));

    let mut sum_med = Duration::ZERO;
    for (name, native) in &cases {
        let c = if native.stage == HwStage::Ps { &ps_c } else { &cs_c };
        let code = native.patched_code();
        let n_insts = gfx10::decode(&code).unwrap().len();
        let mut p = ir::build(&code, native.wave64, native.vgprs).unwrap();
        let n_blks = p.blocks.len();
        sgpr_spill::forward(&mut p);
        let io_r = io::raise(&p, native).unwrap();
        let st = structure::structure(&p).unwrap();
        let res = resource::track_with(&p, &native.layout, native.stage, false).unwrap();
        let host = emit::emit(c, native, &p, &st, &io_r, &res, emit::Target::ANY).unwrap();
        let (dxil_len, hash) = (host.dxil.len(), fnv1a(&host.dxil));

        let mut samples = [[Duration::ZERO; SAMPLES]; 9];
        for s in 0..SAMPLES {
            let mut acc = [Duration::ZERO; 9];
            for _ in 0..ITERS {
                let d0 = Instant::now();
                black_box(gfx10::decode(black_box(&code)).unwrap());
                acc[0] += d0.elapsed();

                let t0 = Instant::now();
                let mut p = ir::build(black_box(&code), native.wave64, native.vgprs).unwrap();
                let t1 = Instant::now();
                sgpr_spill::forward(&mut p);
                let t2 = Instant::now();
                let io_r = io::raise(&p, native).unwrap();
                let t3 = Instant::now();
                let st = structure::structure(&p).unwrap();
                let t4 = Instant::now();
                let res = resource::track_with(&p, &native.layout, native.stage, false).unwrap();
                let t5 = Instant::now();
                black_box(emit::emit(c, native, &p, &st, &io_r, &res, emit::Target::ANY).unwrap());
                let t6 = Instant::now();

                let dd0 = Instant::now();
                black_box(sgpr_spill::dead(&p));
                acc[3] += dd0.elapsed();

                acc[1] += t1 - t0;
                acc[2] += t2 - t1;
                acc[4] += t3 - t2;
                acc[5] += t4 - t3;
                acc[6] += t5 - t4;
                acc[7] += t6 - t5;
                acc[8] += t6 - t0;
            }
            for (dst, total) in samples.iter_mut().zip(acc) {
                dst[s] = total / ITERS;
            }
        }
        for row in &mut samples {
            row.sort_unstable();
        }
        let us = |d: Duration| d.as_secs_f64() * 1e6;
        let m = |k: usize| us(samples[k][SAMPLES / 2]);
        sum_med += samples[8][SAMPLES / 2];
        println!(
            "{:<21} {:>5} {:>4} {:>7} | {:>8.0}u ({:>5.0}u..{:>5.0}u) | {:>5.0}u {:>4.0}u {:>3.0}u {:>5.0}u {:>4.0}u {:>5.0}u | {:>7.0}u {:>5.0}u | {:#018x}",
            name, n_insts, n_blks, dxil_len,
            m(8), us(samples[8][0]), us(samples[8][SAMPLES - 1]),
            m(1), m(2), m(4), m(5), m(6), m(7),
            m(0), m(3), hash
        );
    }
    println!("{}", "-".repeat(162));
    println!("Sum of e2e_total medians: {:.2} ms", sum_med.as_secs_f64() * 1e3);
}

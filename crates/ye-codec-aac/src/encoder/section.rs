//! section_data 组装：逐频带码本选择、相邻同码本段合并、scalefactor
//! DPCM 差分链。
//!
//! ## 段合并（硬要求）
//!
//! 相邻同码本频带**必须合并成一个段**（段开销 = 4 bit 码本号 + 5 bit
//! 长度，5 bit 长度全 1 为续读逃逸：`L = 31·k + r` 按 `31…31 r` 链写，
//! 对齐解码侧 `parse_section_data`）。44.1 kHz 长窗 ~49 带，逐带一段
//! 每帧多耗 ~440 bit——段合并把典型内容的段数压到个位数（静带并入
//! 码本 0 段）。
//!
//! ## 码本选择（贪心最小位）
//!
//! 逐活跃带按带内最大 |q| 取**幅值域可行**的候选码本集，逐一按真实
//! 编码位数（符号码 + 符号位 + escape，[`super::quant::group_cost`]）
//! 评估取最小；平手取小号（quad 优先——同样位数的 quad 多携一倍系数）。
//! 非活跃带（量化全零、不进 sf 链）一律码本 0。
//!
//! ## scalefactor 链
//!
//! 解码侧差分链只消费**非零码本段**（码本 0 带不读码字也不推进链），
//! 常规链自 `global_gain` 起步。写侧链构建：首个活跃带 sf 即
//! global_gain，其后 `delta = clamp(sf[i] − sf[prev], −60, 60)`（码表
//! 值域；越界钳制使带 sf 偏离心理声学目标——码率环整体移位时钳口
//! 不变，行为确定）。链值全域 0..=255（句法）。

use super::bitwriter::BitWriter;
use super::quant::{cb_dim, cb_max_mag, group_cost};
use crate::tables::scalefactor_codes;

/// 一个编码段：码本 + 覆盖频带数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Section {
    /// section codebook（0..=11）。
    pub cb: u8,
    /// 段长（频带数；写侧按 31 逃逸链展开）。
    pub len: u16,
}

/// 长窗段长度逃逸值（5 bit 全 1）。
const SECT_ESCAPE: u32 = 31;

/// 逐频带选码本：`active[j]` = 该带进 sf 链（活跃）；活跃带按量化值
/// 贪心选最小位码本，非活跃带码本 0。返回 `max_sfb` 项。
pub(crate) fn choose_codebooks(quants: &[i32], active: &[bool], swb_offset: &[u16]) -> Vec<u8> {
    let n = active.len();
    let mut out = Vec::with_capacity(n);
    for j in 0..n {
        if !active[j] {
            out.push(0);
            continue;
        }
        let (lo, hi) = (swb_offset[j] as usize, swb_offset[j + 1] as usize);
        let band = &quants[lo..hi];
        let max_mag = band.iter().map(|q| q.abs()).max().unwrap_or(0);
        // 候选：幅值域可行 + 维度整除带宽（swb 边界全 4 对齐，恒满足）
        let mut best_cb = 2u8;
        let mut best_cost = usize::MAX;
        for cb in 1u8..=11 {
            if max_mag > cb_max_mag(cb) && cb != 11 {
                continue;
            }
            if max_mag > 15 && cb != 11 {
                continue;
            }
            if !band.len().is_multiple_of(cb_dim(cb)) {
                continue;
            }
            let mut cost = 0usize;
            let dim = cb_dim(cb);
            let mut ok = true;
            for grp in band.chunks(dim) {
                match group_cost(cb, grp) {
                    Some(c) => cost += c,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok && cost < best_cost {
                best_cost = cost;
                best_cb = cb;
            }
        }
        out.push(best_cb);
    }
    out
}

/// 相邻同码本频带合并成段。
pub(crate) fn merge_sections(codebooks: &[u8]) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    for &cb in codebooks {
        match out.last_mut() {
            Some(sec) if sec.cb == cb => sec.len += 1,
            _ => out.push(Section { cb, len: 1 }),
        }
    }
    out
}

/// 构建差分链：`sf_off[j]` = 心理声学 sf + 全局移位（钳 0..=255），
/// `active[j]` 标记进链频带。返回 (global_gain, 逐带链值；非活跃带 0)。
pub(crate) fn build_sf_chain(sf_off: &[i16], active: &[bool]) -> (u8, Vec<i16>) {
    let mut chain = vec![0i16; sf_off.len()];
    let mut prev: Option<i16> = None;
    let mut global_gain = 128u8;
    for j in 0..sf_off.len() {
        if !active[j] {
            continue;
        }
        let target = sf_off[j];
        let val = match prev {
            None => target,
            Some(p) => p + (target - p).clamp(-60, 60),
        };
        let val = val.clamp(0, 255);
        if prev.is_none() {
            global_gain = val as u8;
        }
        chain[j] = val;
        prev = Some(val);
    }
    (global_gain, chain)
}

/// 写 section_data（长窗：长度 5 bit，31 逃逸链）。
pub(crate) fn write_sections(w: &mut BitWriter, sections: &[Section]) {
    for sec in sections {
        w.put(u32::from(sec.cb), 4);
        let mut rem = sec.len;
        while rem >= SECT_ESCAPE as u16 {
            w.put(SECT_ESCAPE, 5);
            rem -= SECT_ESCAPE as u16;
        }
        w.put(u32::from(rem), 5);
    }
}

/// 写 scale_factor_data：非零码本带的差分码字（符号 = delta+60）。
pub(crate) fn write_scalefactors(w: &mut BitWriter, sections: &[Section], chain: &[i16]) {
    let (codes, bits) = scalefactor_codes();
    let mut idx = 0usize; // sections 展开后的频带号
    let mut prev: Option<i16> = None;
    for sec in sections {
        for _ in 0..sec.len {
            if sec.cb != 0 {
                let sf = chain[idx];
                let delta = match prev {
                    None => 0i32, // 首带：global_gain 本身，delta 0（1 bit 码）
                    Some(p) => i32::from(sf) - i32::from(p),
                };
                let delta = delta.clamp(-60, 60);
                let sym = (delta + 60) as usize;
                w.put(codes[sym], u32::from(bits[sym]));
                prev = Some(sf);
            }
            idx += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{BitReader, IcsInfo, ONLY_LONG, parse_scale_factor_data};
    use crate::tables;

    fn ics(max_sfb: u8) -> IcsInfo {
        IcsInfo {
            window_sequence: ONLY_LONG,
            window_shape: false,
            max_sfb,
            num_window_groups: 1,
            group_len: [1, 0, 0, 0, 0, 0, 0, 0],
            swb_offset: tables::swb_offset_long(4).unwrap(),
            num_swb: tables::num_swb_long(4).unwrap(),
            tns_max_bands: tables::tns_max_bands_long(4).unwrap(),
        }
    }

    /// 段合并与逃逸链：49 带全 cb0 → 单段 31+18；混合段往返。
    #[test]
    fn sections_merge_and_escape_chain() {
        let cbs = vec![0u8; 49];
        let secs = merge_sections(&cbs);
        assert_eq!(secs, vec![Section { cb: 0, len: 49 }]);
        let mut w = BitWriter::new();
        write_sections(&mut w, &secs);
        w.align_zero();
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let got = crate::syntax::parse_section_data(&mut r, &ics(49)).unwrap();
        assert_eq!(got, cbs);

        // 恰 31 带：31 + 0 两节
        let cbs = vec![2u8; 31];
        let mut w = BitWriter::new();
        write_sections(&mut w, &merge_sections(&cbs));
        w.align_zero();
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(
            crate::syntax::parse_section_data(&mut r, &ics(31)).unwrap(),
            cbs
        );

        // 混合段：0×3 | 2×5 | 11×1 | 0×1
        let cbs = [0, 0, 0, 2, 2, 2, 2, 2, 11, 0];
        let secs = merge_sections(&cbs);
        assert_eq!(secs.len(), 4);
        let mut w = BitWriter::new();
        write_sections(&mut w, &secs);
        w.align_zero();
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(
            crate::syntax::parse_section_data(&mut r, &ics(10)).unwrap(),
            cbs.to_vec()
        );
    }

    /// 码本选择：静带 0、小值带 quad（2）、大值带高号。
    #[test]
    fn codebook_selection_shapes() {
        let swb = tables::swb_offset_long(4).unwrap();
        let mut quants = vec![0i32; 1024];
        // 带 0 [0,4)：小值；带 1 [4,8)：中值（含幅值 4 → 排除 quad
        // 3/4 与幅值域 ≤3 的候选）；带 2+ 全零
        quants[0] = 1;
        quants[2] = -1;
        quants[4] = 4;
        quants[5] = -2;
        quants[6] = 1;
        quants[7] = 3;
        let active = vec![true, true, false, false];
        let cbs = choose_codebooks(&quants, &active, swb);
        assert_eq!(cbs[0], 2, "±1 值应选 quad 无符号（cb2）");
        assert!(
            (5..=8).contains(&cbs[1]),
            "幅值 4 带应选 pair（5..8），得 {}",
            cbs[1]
        );
        assert_eq!(cbs[2], 0);
        // 极大值 → cb11
        quants[4] = 100;
        quants[5] = -8191;
        quants[6] = 900;
        let cbs = choose_codebooks(&quants, &active, swb);
        assert_eq!(cbs[1], 11);
    }

    /// 差分链：首带 = global_gain（delta 0）、±60 钳制、全 0..=255。
    #[test]
    fn sf_chain_build_and_write() {
        let sf_off = vec![180i16, 200, 255, 100, 130];
        let active = vec![true, true, true, true, true];
        let (gg, chain) = build_sf_chain(&sf_off, &active);
        assert_eq!(gg, 180);
        // 255→100 差 −155 钳 −60 → 195；195→130 差 −65 钳 −60 → 135
        assert_eq!(chain, vec![180, 200, 255, 195, 135]);
        // 写 → 解码侧链重建一致
        let sections = vec![Section { cb: 2, len: 5 }];
        let mut w = BitWriter::new();
        write_scalefactors(&mut w, &sections, &chain);
        w.align_zero();
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let got = parse_scale_factor_data(&mut r, &[2, 2, 2, 2, 2], gg, &ics(5)).unwrap();
        assert_eq!(got, chain);
    }

    /// 非活跃带跳过链：0 号带 cb0 不耗 sf 码、不推进链。
    #[test]
    fn sf_chain_skips_zero_bands() {
        let sf_off = vec![100i16, 100, 100];
        let active = vec![false, true, true];
        let (gg, chain) = build_sf_chain(&sf_off, &active);
        assert_eq!(gg, 100);
        assert_eq!(chain, vec![0, 100, 100]);
        let sections = vec![Section { cb: 0, len: 1 }, Section { cb: 2, len: 2 }];
        let mut w = BitWriter::new();
        write_scalefactors(&mut w, &sections, &chain);
        w.align_zero();
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let got = parse_scale_factor_data(&mut r, &[0, 2, 2], gg, &ics(3)).unwrap();
        assert_eq!(got, chain);
    }
}

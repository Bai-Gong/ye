//! raw data block 组装与码率环：单声道 SCE / 立体声 CPE（common_window=1
//! + ms_present=0，两声道各自独立编码）、FIL 填充补齐预算、ADTS 包装。
//!
//! ## 元素排布（硬性要求）
//!
//! `channelConfiguration=2` 的首元素必须是 **CPE**（两个 SCE 是非标准
//! 排布，FFmpeg 期望 CPE）：`id=1 → tag → common_window=1 → 共享
//! ics_info（一次）→ ms_present=0 → 两路 global_gain+各自
//! section/scalefactor/spectral（不带 ics_info）`。单声道为 SCE。
//! pulse/tns/gain_control 一律 present=0（长窗合法流）。
//!
//! ## 码率环（全局 sf 移位）
//!
//! 每帧预算 `bit_rate×1024/sample_rate` 位（含 ADTS 头 56 位 → 折算
//! 载荷目标字节数）。心理声学 sf 为质量目标（细侧），超预算时**整帧
//! 上移所有 SFB 的 sf**（量化统一变粗、位数单调下降）：先 16 步粗探
//! 首个不超预算的移位，再在（超, 适）区间二分取最小适移位；迭代有
//! 上限，sf 钳到 255 后谱必然全零（最小帧），收敛有保证。
//!
//! ## FIL 填充（恒定码率门）
//!
//! 正弦类低熵内容按心理声学目标编码会**远低于**预算（码率环只粗化
//! 不细化），以标准 **fill 元素**（载荷全零，id=6）把每帧载荷补齐到
//! 目标字节数——恒定码率门（±20%）对任意内容成立；填充量是字节级
//! 精确的（段开销 7/15 位与 END 3 位 + 对齐位联合求解）。
//!
//! ## ADTS 包装
//!
//! 7 字节头（无 CRC）+ raw data block；profile=1（LC）、
//! buffer_fullness=0x7FF（变码率语义）、num_raw_blocks=1。

use super::bitwriter::BitWriter;
use super::psy;
use super::quant::{cb_dim, quantize_band, write_group};
use super::section::{
    Section, build_sf_chain, choose_codebooks, merge_sections, write_scalefactors, write_sections,
};

/// 单声道一帧的完整编码产物（写侧中间表示，测试用于参考重建）。
#[derive(Debug, Clone)]
pub(crate) struct ChannelQuant {
    /// global_gain（该声道差分链种子）。
    pub global_gain: u8,
    /// 共享 max_sfb（CPE 两声道一致）。
    pub max_sfb: u8,
    /// 逐频带 section codebook（0..max_sfb 项；主链经 `sections`
    /// 消费，逐带值供测试参考重建对拍）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub codebooks: Vec<u8>,
    /// 合并后的段表。
    pub sections: Vec<Section>,
    /// 逐频带 sf 链值（码本 0 带为 0 占位）。
    pub chain: Vec<i16>,
    /// 量化谱（1024；非活跃带恒 0）。
    pub quants: Vec<i32>,
}

/// 一次码率环尝试的产物。
struct Attempt {
    /// 字节对齐后的载荷位数（不含 FIL 填充）。
    bits: usize,
    /// 各声道编码产物。
    channels: Vec<ChannelQuant>,
}

/// 心理声学 sf + 全局移位 → 逐带钳制 0..=255。
fn shifted_sf(psy_sf: &[i16], offset: i32) -> Vec<i16> {
    psy_sf
        .iter()
        .map(|&s| (i32::from(s) + offset).clamp(0, 255) as i16)
        .collect()
}

/// 单声道一次尝试：临时量化判活跃 →（共享 max_sfb 下）差分链 →
/// 终量化 → 选码本 → 段合并。
///
/// `shared_max_sfb = None` 时以自身最后活跃带 +1 为 max_sfb（SCE /
/// CPE 第一遍）；`Some(s)` 时用共享值并把 `s` 以上频带强制非活跃
/// （CPE 另一声道拖高共享值时，本声道高带并成码本 0 段）。
fn channel_attempt(
    coeffs: &[f32],
    sf_off: &[i16],
    shared_max_sfb: Option<u8>,
    swb_offset: &[u16],
    num_swb: usize,
) -> ChannelQuant {
    let mut quants = vec![0i32; coeffs.len()];
    let mut active = vec![false; num_swb];
    for j in 0..num_swb {
        let (lo, hi) = (swb_offset[j] as usize, swb_offset[j + 1] as usize);
        quantize_band(&coeffs[lo..hi], sf_off[j], &mut quants[lo..hi]);
        active[j] = quants[lo..hi].iter().any(|&q| q != 0);
    }
    let mut max_sfb = usize::from(shared_max_sfb.unwrap_or(0));
    if max_sfb == 0 {
        max_sfb = 1 + active.iter().rposition(|&a| a).unwrap_or(0);
    }
    max_sfb = max_sfb.clamp(1, num_swb);
    for a in active.iter_mut().skip(max_sfb) {
        *a = false;
    }
    let (global_gain, chain_full) = build_sf_chain(sf_off, &active);
    // 终量化：活跃带按链值 sf、非活跃带清零
    for j in 0..num_swb {
        let (lo, hi) = (swb_offset[j] as usize, swb_offset[j + 1] as usize);
        if active[j] {
            quantize_band(&coeffs[lo..hi], chain_full[j], &mut quants[lo..hi]);
        } else {
            for q in &mut quants[lo..hi] {
                *q = 0;
            }
        }
    }
    let codebooks = choose_codebooks(&quants, &active, swb_offset);
    let sections = merge_sections(&codebooks[..max_sfb]);
    ChannelQuant {
        global_gain,
        max_sfb: max_sfb as u8,
        codebooks: codebooks[..max_sfb].to_vec(),
        sections,
        chain: chain_full[..max_sfb].to_vec(),
        quants,
    }
}

/// 写共享 ics_info（长窗/sine/无预测）。
fn write_ics_info(w: &mut BitWriter, max_sfb: u8) {
    w.put(0, 1); // reserved
    w.put(0, 2); // window_sequence = ONLY_LONG
    w.put(0, 1); // window_shape = 0（sine）
    w.put(u32::from(max_sfb), 6);
    w.put(0, 1); // predictor_data_present
}

/// 写单声道载荷（不含 global_gain/ics_info）：section/scalefactor/
/// 三个 absence 位/spectral。
fn write_channel_payload(w: &mut BitWriter, cq: &ChannelQuant, swb_offset: &[u16]) {
    write_sections(w, &cq.sections);
    write_scalefactors(w, &cq.sections, &cq.chain);
    w.put(0, 1); // pulse_data_present
    w.put(0, 1); // tns_data_present
    w.put(0, 1); // gain_control_data_present
    // spectral_data：按段遍历频带，组宽度 = 码本维度
    let mut band = 0usize;
    for sec in &cq.sections {
        if sec.cb != 0 {
            let dim = cb_dim(sec.cb);
            for _ in 0..sec.len {
                let lo = swb_offset[band] as usize;
                let hi = swb_offset[band + 1] as usize;
                for grp in cq.quants[lo..hi].chunks(dim) {
                    write_group(w, sec.cb, grp);
                }
                band += 1;
            }
        } else {
            band += sec.len as usize;
        }
    }
    debug_assert_eq!(band, cq.max_sfb as usize);
}

/// 写元素序列（SCE 或 CPE；不含 FIL/END/对齐）。
fn write_elements(w: &mut BitWriter, channels: &[ChannelQuant], swb_offset: &[u16]) {
    if channels.len() == 1 {
        let cq = &channels[0];
        w.put(0, 3); // id = SCE
        w.put(0, 4); // element_instance_tag
        w.put(u32::from(cq.global_gain), 8);
        write_ics_info(w, cq.max_sfb);
        write_channel_payload(w, cq, swb_offset);
    } else {
        let shared = channels[0].max_sfb.max(channels[1].max_sfb);
        w.put(1, 3); // id = CPE
        w.put(0, 4); // element_instance_tag
        w.put(1, 1); // common_window
        write_ics_info(w, shared);
        w.put(0, 2); // ms_present = 0（不做 M/S，两声道独立）
        for cq in channels {
            w.put(u32::from(cq.global_gain), 8);
            write_channel_payload(w, cq, swb_offset);
        }
    }
}

/// 以 FIL（fill 元素，载荷零字节）填充到 `target_bits`（8 的倍数）。
///
/// 每个 FIL 消耗 `7+8c`（普通段，c ≤ 14）或 `15+8c`（逃逸段，
/// c ∈ 15..=269，4 bit 全 1 续 8 位扩展）位；逐个选取不越过目标的
/// 最大段，与 END 3 位 + 对齐位联合恰好落满目标字节。连最小 FIL
/// 都放不下时提前收尾（帧比目标短数位，仅极小预算的角落形态）。
fn emit_fill(w: &mut BitWriter, target_bits: usize) {
    loop {
        let cur = w.bit_len();
        let end = (cur + 3).div_ceil(8) * 8; // 不加 FIL 时的终点
        if end >= target_bits {
            return;
        }
        // 选不越目标的最大 FIL：普通段 c=0..=14、逃逸段 c=15..=269
        let mut best: Option<(bool, u32, usize)> = None; // (逃逸?, count, 终点)
        let consider = |esc: bool, count: u32, best: &mut Option<(bool, u32, usize)>| {
            let hdr = if esc { 15 } else { 7 };
            let new_end = (cur + hdr + 8 * count as usize + 3).div_ceil(8) * 8;
            if new_end <= target_bits && best.as_ref().is_none_or(|b| new_end > b.2) {
                *best = Some((esc, count, new_end));
            }
        };
        for count in 0u32..=14 {
            consider(false, count, &mut best);
        }
        for count in 15u32..=269 {
            consider(true, count, &mut best);
        }
        let Some((esc, count, _)) = best else {
            return;
        };
        w.put(6, 3); // id = FIL
        if esc {
            w.put(15, 4);
            w.put(count - 14, 8); // 扩展字节：total = 15 + ext − 1
        } else {
            w.put(count, 4);
        }
        for _ in 0..count {
            w.put(0, 8);
        }
    }
}

/// 一帧编码主链（码率环 + FIL 填充）。`coeffs` 逐声道；`
/// `payload_target_bytes` = 含 ADTS 头折算后的载荷目标（0 = 关闭
/// 填充，仅裸编码）。返回（字节对齐 raw data block，各声道中间
/// 产物——确定性编码，供测试参考重建复用）。
pub(crate) fn encode_frame(
    coeffs: &[&[f32]],
    swb_offset: &[u16],
    num_swb: usize,
    payload_target_bytes: usize,
) -> (Vec<u8>, Vec<ChannelQuant>) {
    let psy_sf: Vec<Vec<i16>> = coeffs
        .iter()
        .map(|c| psy::analyze(c, swb_offset, num_swb))
        .collect();
    let target_bits = payload_target_bytes * 8;

    let attempt = |offset: i32| -> Attempt {
        let sf_off: Vec<Vec<i16>> = psy_sf.iter().map(|p| shifted_sf(p, offset)).collect();
        // 第一遍：各自 max_sfb → 共享值
        let shared = sf_off
            .iter()
            .zip(coeffs.iter())
            .map(|(sf, c)| channel_attempt(c, sf, None, swb_offset, num_swb).max_sfb)
            .max()
            .unwrap_or(1);
        let channels: Vec<ChannelQuant> = sf_off
            .iter()
            .zip(coeffs.iter())
            .map(|(sf, c)| channel_attempt(c, sf, Some(shared), swb_offset, num_swb))
            .collect();
        let mut w = BitWriter::new();
        write_elements(&mut w, &channels, swb_offset);
        w.align_zero();
        Attempt {
            bits: w.bit_len(),
            channels,
        }
    };

    // 码率环：offset 0（心理声学目标）起步；超预算 → 粗探 + 二分
    //（目标 0 = 无码率约束：纯心理声学编码，不填充）。
    let mut chosen = attempt(0);
    if target_bits > 0 && chosen.bits > target_bits {
        let mut lo = 0i32; // 已知超预算
        let mut hi: Option<i32> = None;
        let mut o = 16i32;
        for _ in 0..24 {
            let a = attempt(o);
            if a.bits <= target_bits {
                chosen = a;
                hi = Some(o);
                break;
            }
            lo = o;
            o += 16;
        }
        if let Some(mut h) = hi {
            // 二分：最小适移位（bits 随移位单调下降，钳制下近似单调）
            while h - lo > 1 {
                let mid = lo + (h - lo) / 2;
                let a = attempt(mid);
                if a.bits <= target_bits {
                    chosen = a;
                    h = mid;
                } else {
                    lo = mid;
                }
            }
        }
        // hi = None（24 步粗探未收敛）：保留最粗尝试，如实超预算
    }

    // 终写：元素 + FIL 填充 + END + 对齐
    let mut w = BitWriter::new();
    write_elements(&mut w, &chosen.channels, swb_offset);
    if target_bits > 0 {
        emit_fill(&mut w, target_bits);
    }
    w.put(7, 3); // id = END
    w.align_zero();
    (w.into_bytes(), chosen.channels)
}

/// ADTS 包装：7 字节头（无 CRC）+ 载荷。
///
/// `sampling_index` 0..=12、`channel_config` 1|2 由调用方保证；
/// profile=1（object type 2 = LC）。
pub(crate) fn adts_wrap(sampling_index: u8, channel_config: u8, payload: &[u8]) -> Vec<u8> {
    let frame_len = payload.len() + 7;
    debug_assert!(frame_len <= 8191, "ADTS 帧长 13 位上限");
    let mut w = BitWriter::new();
    w.put(0xFFF, 12); // syncword
    w.put(0, 1); // MPEG-4
    w.put(0, 2); // layer
    w.put(1, 1); // protection_absent
    w.put(1, 2); // profile = LC − 1
    w.put(u32::from(sampling_index), 4);
    w.put(0, 1); // private
    w.put(u32::from(channel_config), 3);
    w.put(0, 2); // original/home
    w.put(0, 2); // copyright
    w.put(frame_len as u32, 13);
    w.put(0x7FF, 11); // buffer_fullness：变码率
    w.put(0, 2); // num_raw_blocks − 1
    let mut out = w.into_bytes();
    out.extend_from_slice(payload);
    out
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{AudioSpecificConfig, RawElement, parse_adts, parse_raw_data_block_at};

    fn asc_44_stereo() -> AudioSpecificConfig {
        AudioSpecificConfig {
            object_type: 2,
            sampling_index: 4,
            sample_rate: 44_100,
            channel_config: 2,
            frame_length_short: false,
        }
    }

    /// 合成确定性立体声谱（正弦 + 谐波），MDCT 域直接构造。
    fn synth_coeffs(seed: f64) -> Vec<f32> {
        let mut v = vec![0.0f32; 1024];
        for (k, x) in v.iter_mut().enumerate() {
            let kk = k as f64;
            *x = (3.0e7 * (kk * 0.31 + seed).cos()
                + 2.0e6 * (kk * 1.7 + seed).sin()
                + 5.0e4 * (kk * 0.05).cos()) as f32;
        }
        v
    }

    /// SCE/CPE 句法冒烟：单/双声道各编一帧，自家解码器句法层解析
    /// exit-clean 且元素形态正确（长窗/sine/无 ms/无 tns）。
    #[test]
    fn raw_block_shapes_sce_and_cpe() {
        let swb = crate::tables::swb_offset_long(4).unwrap();
        let num = crate::tables::num_swb_long(4).unwrap() as usize;

        // SCE
        let mono = synth_coeffs(0.1);
        let (payload, _) = encode_frame(&[&mono], swb, num, 0); // 无约束（0 = 关）
        let asc = AudioSpecificConfig {
            channel_config: 1,
            ..asc_44_stereo()
        };
        let rdb = parse_raw_data_block_at(&payload, 0, &asc).unwrap();
        assert_eq!(rdb.elements.len(), 1);
        let RawElement::Single(ch) = &rdb.elements[0] else {
            panic!("SCE");
        };
        assert_eq!(ch.ics.window_sequence, 0);
        assert!(!ch.ics.window_shape);
        assert!(ch.tns.is_none());
        assert_eq!(ch.codebooks.len(), ch.ics.max_sfb as usize);
        assert!(ch.ics.max_sfb >= 1);

        // CPE
        let l = synth_coeffs(0.2);
        let r = synth_coeffs(0.9);
        let (payload, _) = encode_frame(&[&l, &r], swb, num, 200);
        let rdb = parse_raw_data_block_at(&payload, 0, &asc_44_stereo()).unwrap();
        let RawElement::Pair { common, ch } = &rdb.elements[0] else {
            panic!("CPE");
        };
        let common = common.as_ref().expect("common_window=1");
        assert_eq!(common.ms_present, 0);
        assert_eq!(ch[0].ics.max_sfb, ch[1].ics.max_sfb, "共享 max_sfb");
    }

    /// 段合并收益：调性内容（少数活跃带 + 大片静带）段数远小于频带数
    ///（44.1k 49 带 → 段 ≤ 12；静带并入码本 0 段）。
    #[test]
    fn sections_are_merged() {
        let swb = crate::tables::swb_offset_long(4).unwrap();
        let num = crate::tables::num_swb_long(4).unwrap() as usize;
        // 单音（系数 20 = 带 5）+ 谐波（系数 60 = 带 15）+ 低噪底
        //（幅值 100 远低于绝对听阈 → 静带）
        let mut mono = vec![0.0f32; 1024];
        for (k, v) in mono.iter_mut().enumerate() {
            *v = 100.0 * ((k * 7) as f32).cos();
        }
        mono[20] = 3.0e7;
        mono[60] = 2.0e6;
        let sf_off = psy::analyze(&mono, swb, num);
        let cq = channel_attempt(&mono, &sf_off, None, swb, num);
        assert!(cq.sections.len() <= 12, "段数 {} 过多", cq.sections.len());
        // 相邻段码本互异（合并不留相邻同码本残段）
        for pair in cq.sections.windows(2) {
            assert_ne!(pair[0].cb, pair[1].cb);
        }
        assert_eq!(
            cq.sections.iter().map(|s| s.len as usize).sum::<usize>(),
            cq.max_sfb as usize
        );
    }

    /// 码率环收敛：极小预算强制粗化，FIL 填充后载荷恰为目标字节数。
    #[test]
    fn rate_loop_converges_and_pads() {
        let swb = crate::tables::swb_offset_long(4).unwrap();
        let num = crate::tables::num_swb_long(4).unwrap() as usize;
        let l = synth_coeffs(0.5);
        let r = synth_coeffs(1.4);
        // 心理声学目标的裸载荷（无约束）远超 20 字节 → 码率环必粗化
        let (raw, _) = encode_frame(&[&l, &r], swb, num, 0);
        assert!(raw.len() > 20, "裸载荷 {} 应超小预算", raw.len());
        // 20 字节预算：粗化收敛 + FIL 填充恰为 20 字节
        let (payload, _) = encode_frame(&[&l, &r], swb, num, 20);
        assert_eq!(payload.len(), 20, "码率环收敛 + 填充到目标");
        // 64 kbps 立体声 44.1k 预算（185−7 字节）
        let (payload, _) = encode_frame(&[&l, &r], swb, num, 185 - 7);
        assert_eq!(payload.len(), 178);
    }

    /// ADTS 头往返：自家 parse_adts 逐字段一致。
    #[test]
    fn adts_roundtrip() {
        let payload = [0u8; 32];
        let frame = adts_wrap(4, 2, &payload);
        assert_eq!(frame.len(), 39);
        let h = parse_adts(&frame).unwrap();
        assert_eq!(h.object_type, 2);
        assert_eq!(h.sampling_index, 4);
        assert_eq!(h.channel_config, 2);
        assert_eq!(h.frame_length, 39);
        assert_eq!(h.header_length, 7);
    }
}

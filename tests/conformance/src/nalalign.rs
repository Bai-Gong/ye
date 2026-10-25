//! NAL 序列与 `h264_mp4toannexb` 的参数集插入对齐（离线可单测）。
//!
//! 对拍口径 a（NAL 序列 bit-exact）要求两侧 NAL 序列可逐个比较，但
//! ffmpeg 侧经 `h264_mp4toannexb` 转出的 Annex B 流会按其语义补插
//! avcC（extradata）里的参数集，而 MP4 包内 NAL 不带参数集——本模块
//! 把我方序列按同一语义补插后再比较（比较的是码流本体的 NAL 序列）。
//!
//! ## 插入语义（2026-10-24 施工图②以 45-IDR 向量实测锁定）
//!
//! 以 `ffmpeg -g 1` 造 45 帧 IDR 向量实测，`h264_mp4toannexb` 输出的
//! NAL 型列为 `[SEI, SPS, PPS, IDR] × 45`（45×SPS + 45×PPS）：**每个
//! IDR 之前**都插 extradata 参数集，不是只插首个；首个 IDR 前有 SEI
//! 时插在 SEI 之后（IDR 紧前）。据此实现：逐 IDR 前置补插，自上一个
//! IDR（不含）至本 IDR（不含）窗口内已带内出现过的参数集不重复插
//! （缺哪个补哪个，SPS 在前 PPS 在后）。已知近似：ffmpeg 按包粒度
//! 判断关键帧包内是否带参数集，本实现按「上一个 IDR 以来的窗口」
//! 判断——对带内参数集出现在 GOP 中部的码流两者可能不同；我方向量
//! （libx264 + MP4，参数集全在 avcC）不会触发该差异，真出现时
//! NAL 对拍会响亮失败并以 `YE_CONFORMANCE_DEBUG_NALS=1` 排查。

use ye_codec_h264::AvcConfigRecord;

/// NAL 首字节的 unit type（低 5 位）。
pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map(|b| b & 0x1F).unwrap_or(255)
}

/// 按 `h264_mp4toannexb` 的参数集语义补插 avcC 的 SPS/PPS（见模块文档）。
/// 全流无 IDR 时：流内已有 SPS 则原样返回，否则参数集前置到流首。
pub fn align_parameter_sets(nals: &[Vec<u8>], record: &AvcConfigRecord) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(nals.len() + 8);
    let mut sps_seen = false; // 上一个 IDR 之后窗口内是否已见带内 SPS
    let mut pps_seen = false;
    let mut any_idr = false;
    for nal in nals {
        match nal_type(nal) {
            5 => {
                any_idr = true;
                if !sps_seen {
                    out.extend(record.sps_nals.iter().cloned());
                }
                if !pps_seen {
                    out.extend(record.pps_nals.iter().cloned());
                }
                out.push(nal.clone());
                sps_seen = false;
                pps_seen = false;
            }
            7 => {
                sps_seen = true;
                out.push(nal.clone());
            }
            8 => {
                pps_seen = true;
                out.push(nal.clone());
            }
            _ => out.push(nal.clone()),
        }
    }
    if !any_idr && !sps_seen {
        let mut head: Vec<Vec<u8>> = record.sps_nals.clone();
        head.extend(record.pps_nals.iter().cloned());
        head.extend(out);
        return head;
    }
    out
}

/// 尾 NAL 去尾随零字节（trailing_zero_8bits 归一化：起始码切分对尾零
/// 的归属两侧语义不同，非载荷差异）。
pub fn trim_trailing_zeros(nals: &mut [Vec<u8>]) {
    if let Some(last) = nals.last_mut() {
        while last.last() == Some(&0) {
            last.pop();
        }
    }
}

/// 首个 NAL 差异的诊断描述。
pub fn first_nal_diff(ours: &[Vec<u8>], theirs: &[Vec<u8>]) -> String {
    let common = ours.len().min(theirs.len());
    for i in 0..common {
        if ours[i] != theirs[i] {
            let ty = |nal: &[u8]| nal.first().map(|b| b & 0x1F).unwrap_or(255);
            return format!(
                "第 {i} 个 NAL（type 我方={} ffmpeg={}，len 我方={} ffmpeg={}）",
                ty(&ours[i]),
                ty(&theirs[i]),
                ours[i].len(),
                theirs[i].len()
            );
        }
    }
    format!("前 {common} 个一致，长度发散")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 假 avcC 记录（内容不参与对齐，只有条目数）。
    fn record() -> AvcConfigRecord {
        AvcConfigRecord {
            configuration_version: 1,
            profile_indication: 66,
            profile_compatibility: 192,
            level_indication: 30,
            length_size: 4,
            sps_nals: vec![vec![0x67, 0xAA]],
            pps_nals: vec![vec![0x68, 0xBB]],
        }
    }

    const SEI: &[u8] = &[0x06];
    const IDR: &[u8] = &[0x65];
    const NON_IDR: &[u8] = &[0x41];
    const SPS: &[u8] = &[0x67, 0x11];
    const PPS: &[u8] = &[0x68, 0x22];

    fn v(bytes: &[u8]) -> Vec<u8> {
        bytes.to_vec()
    }

    /// 单 IDR 流（施工图①既有形态）：SEI → 插 [SPS PPS] → IDR → P…
    #[test]
    fn single_idr_inserts_once() {
        let nals = [v(SEI), v(IDR), v(NON_IDR), v(NON_IDR)];
        let aligned = align_parameter_sets(&nals, &record());
        let types: Vec<u8> = aligned.iter().map(|n| nal_type(n)).collect();
        assert_eq!(types, vec![6, 7, 8, 5, 1, 1]);
        // 补插的是 avcC 里的参数集（0xAA/0xBB 载荷），非流内假设值
        assert_eq!(aligned[1], vec![0x67, 0xAA]);
        assert_eq!(aligned[2], vec![0x68, 0xBB]);
    }

    /// 多 IDR 流（施工图② 45-IDR 向量实测 ffmpeg 形态 `[SEI,SPS,PPS,IDR]×N`）：
    /// 每个 IDR 前都补插，SEI 后、IDR 前。
    #[test]
    fn every_idr_gets_parameter_sets() {
        let nals = [v(SEI), v(IDR), v(IDR), v(NON_IDR), v(IDR)];
        let aligned = align_parameter_sets(&nals, &record());
        let types: Vec<u8> = aligned.iter().map(|n| nal_type(n)).collect();
        assert_eq!(types, vec![6, 7, 8, 5, 7, 8, 5, 1, 7, 8, 5]);
    }

    /// 带内已有参数集的 IDR 不重复插（缺哪个只补哪个）。
    #[test]
    fn in_band_parameter_sets_not_duplicated() {
        // 窗口内已带 SPS+PPS → 全不补
        let nals = [v(SPS), v(PPS), v(IDR), v(IDR)];
        let aligned = align_parameter_sets(&nals, &record());
        let types: Vec<u8> = aligned.iter().map(|n| nal_type(n)).collect();
        assert_eq!(types, vec![7, 8, 5, 7, 8, 5]); // 第 2 个 IDR 起正常补插
        // 只有 SPS 带 → 只补 PPS
        let nals = [v(SPS), v(IDR)];
        let aligned = align_parameter_sets(&nals, &record());
        let types: Vec<u8> = aligned.iter().map(|n| nal_type(n)).collect();
        assert_eq!(types, vec![7, 8, 5]);
    }

    /// 全流无 IDR：已有带内 SPS 原样；无任何参数集则前置。
    #[test]
    fn no_idr_fallback() {
        let nals = [v(SPS), v(NON_IDR)];
        let aligned = align_parameter_sets(&nals, &record());
        assert_eq!(aligned.len(), 2);
        let nals = [v(NON_IDR), v(NON_IDR)];
        let aligned = align_parameter_sets(&nals, &record());
        let types: Vec<u8> = aligned.iter().map(|n| nal_type(n)).collect();
        assert_eq!(types, vec![7, 8, 1, 1]);
    }

    #[test]
    fn trailing_zeros_and_first_diff() {
        let mut nals = vec![vec![0x65, 0x00, 0x00]];
        trim_trailing_zeros(&mut nals);
        assert_eq!(nals[0], vec![0x65]);
        let a = [vec![1u8, 2], vec![3u8, 4]];
        let b = [vec![1u8, 2], vec![3u8, 5]];
        assert!(first_nal_diff(&a, &b).contains("第 1 个 NAL"));
        assert!(first_nal_diff(&a, &a).contains("长度发散")); // 等长一致
        let c = [vec![1u8, 2]];
        assert!(first_nal_diff(&c, &a).contains("长度发散"));
    }
}

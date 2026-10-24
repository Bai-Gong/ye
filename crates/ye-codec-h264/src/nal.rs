//! NAL 层：单元分类、AVCC/Annex B 双向拆包与 avcC 记录解析。
//!
//! ## 输入契约（施工图①/契约 docs/contracts.md §MP4 第 6 条）
//!
//! 解码器输入 = **MP4 原生 AVCC**（长度前缀 NAL）+ **avcC extradata
//! 原样透传**（容器不解释，解码器在此解析出长度前缀宽度与带内
//! SPS/PPS）。Annex B（起始码）只作为测试工具与 ffmpeg oracle
//! 对拍格式（`h264_mp4toannexb`），见 [`split_annexb`]。
//!
//! ## 语义约定
//!
//! - 拆包函数只切边界，不动载荷字节（防竞争字节由
//!   [`unescape_rbsp`] 在进 SPS/PPS 解析前单独去除）；
//! - 数据驱动失败（无起始码、长度前缀越界、截断……）一律
//!   `Err(H264Error::InvalidData)`，绝不 panic；
//! - Annex B 侧（测试工具）对畸形输入从宽：空 NAL（连续起始码）
//!   跳过、起始码前导零容忍、尾 NAL 到文件末尾为止。

use std::borrow::Cow;

use crate::error::H264Error;

// —— NAL 头与分类 ——

/// NAL 单元分类（H.264 表 7-1 的解码器视角归并）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq, Hash`
/// （全字段单元变体，纯值类型）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NalCategory {
    /// 类型 1–4：非 IDR 片（含数据分片片 2–4；现代码流只有 1）。
    SliceNonIdr,
    /// 类型 5：IDR 片（关键帧片）。
    SliceIdr,
    /// 类型 6：SEI。
    Sei,
    /// 类型 7：SPS。
    Sps,
    /// 类型 8：PPS。
    Pps,
    /// 类型 9：AUD（访问单元分界）。
    Aud,
    /// 其余（0/10–31：EOS、EOSB、filler、保留与未指定）。
    Other,
}

impl NalCategory {
    /// 是否 VCL（片）NAL——类型 1–5。
    pub fn is_vcl(self) -> bool {
        matches!(self, NalCategory::SliceNonIdr | NalCategory::SliceIdr)
    }
}

/// NAL 单元头（H.264 §7.3.1）：forbidden_zero(1) + nal_ref_idc(2) +
/// nal_unit_type(5)。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq, Hash`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NalHeader {
    /// nal_ref_idc（2 位，0..=3）：丢弃优先级。
    pub nal_ref_idc: u8,
    /// nal_unit_type（5 位，0..=31）。
    pub nal_unit_type: u8,
}

impl NalHeader {
    /// 解析 NAL 首字节；forbidden_zero_bit 置位 → `InvalidData`。
    pub fn parse(byte: u8) -> Result<Self, H264Error> {
        if byte & 0x80 != 0 {
            return Err(H264Error::InvalidData(format!(
                "forbidden_zero_bit is set in nal header byte 0x{byte:02x}"
            )));
        }
        Ok(NalHeader {
            nal_ref_idc: (byte >> 5) & 0x3,
            nal_unit_type: byte & 0x1F,
        })
    }

    /// 回编码为首字节。
    pub fn to_byte(self) -> u8 {
        (self.nal_ref_idc << 5) | self.nal_unit_type
    }

    /// 单元分类。
    pub fn category(self) -> NalCategory {
        match self.nal_unit_type {
            1..=4 => NalCategory::SliceNonIdr,
            5 => NalCategory::SliceIdr,
            6 => NalCategory::Sei,
            7 => NalCategory::Sps,
            8 => NalCategory::Pps,
            9 => NalCategory::Aud,
            _ => NalCategory::Other,
        }
    }
}

/// 校验并切分 NAL：`(头, 载荷)`——载荷从第二字节起（去头不去防竞争）。
pub fn nal_header_and_payload(nal: &[u8]) -> Result<(NalHeader, &[u8]), H264Error> {
    let (&first, payload) = nal
        .split_first()
        .ok_or_else(|| H264Error::InvalidData("nal unit is empty".into()))?;
    Ok((NalHeader::parse(first)?, payload))
}

// —— Annex B（起始码；测试工具与 oracle 对拍格式）——

/// 在 `data[from..]` 中定位起始码 `00 00 01`，返回首个 `00` 的下标。
fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// 把 Annex B 字节流拆成 NAL 单元（**不含**起始码、**含**头字节）。
///
/// - 3 字节（`00 00 01`）与 4 字节（`00 00 00 01`）起始码等价识别；
///   起始码的前导零（无论几个 `00`）归起始码，不归前一个 NAL；
/// - 首个起始码之前只允许零字节，非零前导 → `InvalidData`；
/// - 非空输入无任何起始码 → `InvalidData`；空输入 → 空表；
/// - 连续起始码间的空 NAL 跳过（工具从宽）；末 NAL 延伸到数据末尾。
pub fn split_annexb(data: &[u8]) -> Result<Vec<&[u8]>, H264Error> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let Some(first_sc) = find_start_code(data, 0) else {
        return Err(H264Error::InvalidData(
            "annexb stream has no start code".into(),
        ));
    };
    if data[..first_sc].iter().any(|&b| b != 0) {
        return Err(H264Error::InvalidData(
            "annexb stream has garbage before first start code".into(),
        ));
    }
    let mut nals: Vec<&[u8]> = Vec::new();
    let mut start = first_sc + 3;
    while let Some(sc) = find_start_code(data, start) {
        // 起始码前的尾随零归起始码：回退到首个非零字节
        let mut end = sc;
        while end > start && data[end - 1] == 0 {
            end -= 1;
        }
        if end > start {
            nals.push(&data[start..end]);
        }
        start = sc + 3;
    }
    if start < data.len() {
        nals.push(&data[start..]);
    }
    Ok(nals)
}

/// NAL 序列序列化为 Annex B（一律 4 字节起始码 `00 00 00 01`，
/// 对齐 `h264_mp4toannexb` 输出惯例）；空 NAL 跳过。
pub fn to_annexb(nals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        if nal.is_empty() {
            continue;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    out
}

// —— AVCC（长度前缀；MP4 原生形态，解码器输入契约）——

/// 校验长度前缀字节数（avcC 的 lengthSizeMinusOne+1）。
fn check_length_size(length_size: usize) -> Result<(), H264Error> {
    if length_size == 0 || length_size > 4 {
        return Err(H264Error::InvalidArguments("length_size must be 1..=4"));
    }
    Ok(())
}

/// 大端读取 `bytes`（≤ 8 字节）为 usize。
fn be_len(bytes: &[u8]) -> usize {
    let mut v: u64 = 0;
    for &b in bytes {
        v = (v << 8) | u64::from(b);
    }
    v as usize
}

/// 把 AVCC 字节流（长度前缀 NAL 首尾相接）拆成 NAL 单元。
///
/// - `length_size`（1..=4）来自 [`AvcConfigRecord::parse`]；
/// - 前缀声明的长度为 0 或越出剩余数据 → `InvalidData`；前缀自身被
///   截断 → `InvalidData`；空输入 → 空表；
/// - NAL 内容不校验（解析在 SPS/PPS 层做），只切边界。
pub fn split_avcc(data: &[u8], length_size: usize) -> Result<Vec<&[u8]>, H264Error> {
    check_length_size(length_size)?;
    let mut nals: Vec<&[u8]> = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let rest = data.len() - pos;
        if rest < length_size {
            return Err(H264Error::InvalidData(format!(
                "avcc length prefix truncated at offset {pos}"
            )));
        }
        let len = be_len(&data[pos..pos + length_size]);
        pos += length_size;
        if len == 0 {
            return Err(H264Error::InvalidData(format!(
                "avcc declares zero-length nal at offset {pos}"
            )));
        }
        if len > data.len() - pos {
            return Err(H264Error::InvalidData(format!(
                "avcc nal overruns buffer at offset {pos}: declared {len} bytes, {} left",
                data.len() - pos
            )));
        }
        nals.push(&data[pos..pos + len]);
        pos += len;
    }
    Ok(nals)
}

/// NAL 序列序列化为 AVCC（长度前缀宽度 `length_size`）。
/// 某 NAL 长度超出前缀可表达范围（如 2 字节前缀遇 >65535 的 NAL）→
/// `InvalidData`；空 NAL 跳过。
pub fn to_avcc(nals: &[&[u8]], length_size: usize) -> Result<Vec<u8>, H264Error> {
    check_length_size(length_size)?;
    let max_len: u64 = (1u64 << (8 * length_size as u64)) - 1;
    let mut out = Vec::new();
    for nal in nals {
        if nal.is_empty() {
            continue;
        }
        if nal.len() as u64 > max_len {
            return Err(H264Error::InvalidData(format!(
                "nal of {} bytes exceeds {}-byte length prefix",
                nal.len(),
                length_size
            )));
        }
        let len_bytes = (nal.len() as u64).to_be_bytes();
        out.extend_from_slice(&len_bytes[8 - length_size..]);
        out.extend_from_slice(nal);
    }
    Ok(out)
}

/// AVCC 字节流 → Annex B 字节流（对拍 ffmpeg `h264_mp4toannexb` 用）。
pub fn avcc_to_annexb(data: &[u8], length_size: usize) -> Result<Vec<u8>, H264Error> {
    Ok(to_annexb(&split_avcc(data, length_size)?))
}

/// Annex B 字节流 → AVCC 字节流（反向工具）。
pub fn annexb_to_avcc(data: &[u8], length_size: usize) -> Result<Vec<u8>, H264Error> {
    to_avcc(&split_annexb(data)?, length_size)
}

// —— RBSP（防竞争去除）——

/// 去防竞争（emulation prevention，§7.3.1）：NAL 载荷中的
/// `00 00 03` → `00 00`。无转义序列时零拷贝借用（`Cow::Borrowed`）。
///
/// 宽松语义（对齐 FFmpeg `ff_h2645_extract_rbsp`）：`00 00 03` 一律
/// 丢弃 `03`，不校验后随字节是否 ≤ 3（畸形流在后续字段解析报错）。
pub fn unescape_rbsp(nal: &[u8]) -> Cow<'_, [u8]> {
    let has_escape = {
        let mut i = 0usize;
        let mut found = false;
        while i + 3 <= nal.len() {
            if nal[i] == 0 && nal[i + 1] == 0 && nal[i + 2] == 3 {
                found = true;
                break;
            }
            i += 1;
        }
        found
    };
    if !has_escape {
        return Cow::Borrowed(nal);
    }
    let mut out = Vec::with_capacity(nal.len());
    let mut i = 0usize;
    while i < nal.len() {
        if i + 3 <= nal.len() && nal[i] == 0 && nal[i + 1] == 0 && nal[i + 2] == 3 {
            out.extend_from_slice(&[0, 0]);
            i += 3;
        } else {
            out.push(nal[i]);
            i += 1;
        }
    }
    Cow::Owned(out)
}

// —— avcC（AVCDecoderConfigurationRecord，ISO 14496-15 §5.3.3.1）——

/// avcC 记录：容器 extradata 的解码器侧解析结果。
///
/// 契约：MP4 读出的 `extra_data`（avcC 载荷）**原样透传**，本结构在
/// 解码器侧还原出 AVCC 长度前缀宽度与带内 SPS/PPS NAL。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`（owned 拷贝供初始化缓存）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvcConfigRecord {
    /// configurationVersion（1 为常见值；不做版本拒绝，记录备用）。
    pub configuration_version: u8,
    /// AVCProfileIndication（与 SPS 的 profile_idc 一致）。
    pub profile_indication: u8,
    /// profile_compatibility 字节（constraint 标志的容器侧副本）。
    pub profile_compatibility: u8,
    /// AVCLevelIndication（与 SPS 的 level_idc 一致，level×10）。
    pub level_indication: u8,
    /// 长度前缀字节数（lengthSizeMinusOne+1，1..=4）。
    pub length_size: usize,
    /// 带内 SPS NAL 列表（完整 NAL，含头字节）。
    pub sps_nals: Vec<Vec<u8>>,
    /// 带内 PPS NAL 列表（完整 NAL，含头字节）。
    pub pps_nals: Vec<Vec<u8>>,
}

impl AvcConfigRecord {
    /// 解析 avcC 载荷（容器 `extra_data` 原样字节）。
    ///
    /// - 保留位不做校验（FFmpeg 同样忽略）；记录尾随扩展字节忽略；
    /// - 声明的 SPS/PPS 条目缺失（截断）→ `InvalidData`；
    /// - 各 NAL 首字节做 forbidden_zero_bit 校验。
    pub fn parse(data: &[u8]) -> Result<Self, H264Error> {
        if data.len() < 7 {
            return Err(H264Error::InvalidData(format!(
                "avcC record too short: {} bytes",
                data.len()
            )));
        }
        let length_size = usize::from(data[4] & 0x03) + 1;
        let num_sps = usize::from(data[5] & 0x1F);
        let mut pos = 6usize;
        let mut sps_nals = Vec::with_capacity(num_sps);
        for _ in 0..num_sps {
            sps_nals.push(take_len_prefixed_nal(data, &mut pos, "avcC sps")?);
        }
        let Some(&npps) = data.get(pos) else {
            return Err(H264Error::InvalidData(
                "avcC pps count byte truncated".into(),
            ));
        };
        pos += 1;
        let num_pps = usize::from(npps);
        let mut pps_nals = Vec::with_capacity(num_pps);
        for _ in 0..num_pps {
            pps_nals.push(take_len_prefixed_nal(data, &mut pos, "avcC pps")?);
        }
        Ok(AvcConfigRecord {
            configuration_version: data[0],
            profile_indication: data[1],
            profile_compatibility: data[2],
            level_indication: data[3],
            length_size,
            sps_nals,
            pps_nals,
        })
    }
}

/// 读取一个 `u16 长度前缀 + NAL` 条目并推进 `pos`。
fn take_len_prefixed_nal(
    data: &[u8],
    pos: &mut usize,
    what: &'static str,
) -> Result<Vec<u8>, H264Error> {
    if data.len() < *pos + 2 {
        return Err(H264Error::InvalidData(format!(
            "{what} length prefix truncated"
        )));
    }
    let len = u16::from_be_bytes([data[*pos], data[*pos + 1]]) as usize;
    *pos += 2;
    if len == 0 {
        return Err(H264Error::InvalidData(format!("{what}: zero-length nal")));
    }
    if data.len() < *pos + len {
        return Err(H264Error::InvalidData(format!(
            "{what} nal truncated: declared {len} bytes"
        )));
    }
    let nal = data[*pos..*pos + len].to_vec();
    *pos += len;
    NalHeader::parse(nal[0])?;
    Ok(nal)
}

#[cfg(test)]
mod tests {
    use super::*;

    // —— NAL 头与分类 ——

    #[test]
    fn nal_header_round_trip_and_category() {
        // 0x67 = 0_11_00111：ref_idc=3、type=7（SPS）
        let h = NalHeader::parse(0x67).unwrap();
        assert_eq!((h.nal_ref_idc, h.nal_unit_type), (3, 7));
        assert_eq!(h.to_byte(), 0x67);
        assert_eq!(h.category(), NalCategory::Sps);
        // 0x65：type=5（IDR 片）
        let idr = NalHeader::parse(0x65).unwrap();
        assert_eq!(idr.category(), NalCategory::SliceIdr);
        assert!(idr.category().is_vcl());
        // 0x41：type=1（非 IDR 片）
        let slice = NalHeader::parse(0x41).unwrap();
        assert_eq!(slice.category(), NalCategory::SliceNonIdr);
        assert!(slice.category().is_vcl());
        // 0x06 SEI、0x09 AUD、0x0C filler → Other
        assert_eq!(NalHeader::parse(0x06).unwrap().category(), NalCategory::Sei);
        assert_eq!(NalHeader::parse(0x09).unwrap().category(), NalCategory::Aud);
        assert_eq!(
            NalHeader::parse(0x0C).unwrap().category(),
            NalCategory::Other
        );
        // forbidden 位置位 → Err
        assert!(matches!(
            NalHeader::parse(0xE7),
            Err(H264Error::InvalidData(msg)) if msg.contains("forbidden")
        ));
    }

    // —— Annex B 拆包 ——

    #[test]
    fn split_annexb_3_and_4_byte_start_codes() {
        // 4 字节起始码 + 3 字节起始码混合；前导零容忍
        let data = [
            0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, // SPS（2 字节载荷）
            0x00, 0x00, 0x01, 0x68, 0xBB, 0xCC, // PPS（3 字节起始码）
            0x00, 0x00, 0x01, 0x65, 0xDD, // IDR
        ];
        let nals = split_annexb(&data).unwrap();
        assert_eq!(
            nals,
            vec![
                &[0x67, 0xAA][..],
                &[0x68, 0xBB, 0xCC][..],
                &[0x65, 0xDD][..]
            ]
        );
    }

    #[test]
    fn split_annexb_trailing_zeros_belong_to_next_start_code() {
        // NAL 载荷末尾的 00 00 是下一 4 字节起始码的一部分
        let data = [
            0x00, 0x00, 0x01, 0x67, 0x11, 0x00, 0x00, // SPS + 尾随零
            0x00, 0x01, 0x68, 0x22, // 下一起始码（00 00 00 01）
        ];
        let nals = split_annexb(&data).unwrap();
        assert_eq!(nals, vec![&[0x67, 0x11][..], &[0x68, 0x22][..]]);
    }

    #[test]
    fn split_annexb_empty_and_errors() {
        assert_eq!(split_annexb(&[]).unwrap(), Vec::<&[u8]>::new());
        // 非空但无起始码
        assert!(matches!(
            split_annexb(&[0x67, 0x42]),
            Err(H264Error::InvalidData(msg)) if msg.contains("no start code")
        ));
        // 首个起始码前有非零垃圾
        assert!(matches!(
            split_annexb(&[0xFF, 0x00, 0x00, 0x01, 0x67]),
            Err(H264Error::InvalidData(msg)) if msg.contains("garbage")
        ));
        // 首个起始码前的零字节（zero_byte 前导）容忍
        let nals = split_annexb(&[0x00, 0x00, 0x00, 0x00, 0x01, 0x67]).unwrap();
        assert_eq!(nals, vec![&[0x67][..]]);
        // 连续起始码（空 NAL）跳过
        let nals = split_annexb(&[0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 0x67, 0x42]).unwrap();
        assert_eq!(nals, vec![&[0x67, 0x42][..]]);
    }

    #[test]
    fn annexb_round_trip() {
        let nals: Vec<&[u8]> = vec![
            &[0x67, 0x11, 0x22],
            &[0x68, 0x33],
            &[0x65, 0x44, 0x00, 0x11],
        ];
        let annexb = to_annexb(&nals);
        let expect = [
            0, 0, 0, 1, 0x67, 0x11, 0x22, 0, 0, 0, 1, 0x68, 0x33, 0, 0, 0, 1, 0x65, 0x44, 0x00,
            0x11,
        ];
        assert_eq!(annexb, expect);
        assert_eq!(split_annexb(&annexb).unwrap(), nals);
    }

    // —— AVCC 拆包 ——

    #[test]
    fn split_avcc_basic_and_round_trip() {
        // length_size=4：[00 00 00 02 67 AA] [00 00 00 03 68 BB CC]
        let data = [
            0x00, 0x00, 0x00, 0x02, 0x67, 0xAA, 0x00, 0x00, 0x00, 0x03, 0x68, 0xBB, 0xCC,
        ];
        let nals = split_avcc(&data, 4).unwrap();
        assert_eq!(nals, vec![&[0x67, 0xAA][..], &[0x68, 0xBB, 0xCC][..]]);
        assert_eq!(to_avcc(&nals, 4).unwrap(), data);
        // length_size=1 / 2 / 3 同样可拆可回
        for ls in 1..=3usize {
            let rebuilt = to_avcc(&nals, ls).unwrap();
            assert_eq!(split_avcc(&rebuilt, ls).unwrap(), nals, "length_size={ls}");
        }
        // 空输入 → 空
        assert_eq!(split_avcc(&[], 4).unwrap(), Vec::<&[u8]>::new());
    }

    #[test]
    fn split_avcc_errors() {
        // length_size 越界
        for bad in [0usize, 5] {
            assert!(matches!(
                split_avcc(&[0, 0, 0, 1, 0x67], bad),
                Err(H264Error::InvalidArguments("length_size must be 1..=4"))
            ));
        }
        // 前缀截断（剩 2 字节 < length_size=4）
        assert!(matches!(
            split_avcc(&[0x67, 0xAA], 4),
            Err(H264Error::InvalidData(msg)) if msg.contains("prefix truncated")
        ));
        // 零长 NAL
        assert!(matches!(
            split_avcc(&[0x00, 0x00, 0x00, 0x00, 0x67], 4),
            Err(H264Error::InvalidData(msg)) if msg.contains("zero-length")
        ));
        // 声明长度越出缓冲
        assert!(matches!(
            split_avcc(&[0x00, 0x00, 0x00, 0x09, 0x67], 4),
            Err(H264Error::InvalidData(msg)) if msg.contains("overruns")
        ));
    }

    #[test]
    fn to_avcc_rejects_overflowing_length() {
        let big = vec![0x41u8; 300];
        let nals: Vec<&[u8]> = vec![&big];
        // 1 字节前缀最大 255
        assert!(matches!(
            to_avcc(&nals, 1),
            Err(H264Error::InvalidData(msg)) if msg.contains("exceeds")
        ));
        // 2 字节前缀容得下 300
        assert!(to_avcc(&nals, 2).is_ok());
    }

    #[test]
    fn avcc_annexb_conversions_round_trip() {
        let nals: Vec<Vec<u8>> = vec![
            vec![0x67, 0x11, 0x22, 0x33],
            vec![0x68, 0x44],
            vec![0x65, 0x55, 0x66],
            vec![0x41, 0x77, 0x00, 0x00], // 载荷含零字节
        ];
        let refs: Vec<&[u8]> = nals.iter().map(|v| v.as_slice()).collect();
        // AVCC → AnnexB → 拆包 → 原序列
        let annexb = avcc_to_annexb(&to_avcc(&refs, 4).unwrap(), 4).unwrap();
        assert_eq!(split_annexb(&annexb).unwrap(), refs);
        // AnnexB → AVCC → 拆包 → 原序列
        let avcc = annexb_to_avcc(&to_annexb(&refs), 4).unwrap();
        assert_eq!(split_avcc(&avcc, 4).unwrap(), refs);
    }

    // —— RBSP 防竞争 ——

    #[test]
    fn unescape_rbsp_borrows_when_clean() {
        let clean = [0x67, 0x42, 0xC0, 0x1E, 0xDA, 0x79];
        match unescape_rbsp(&clean) {
            Cow::Borrowed(b) => assert_eq!(b, &clean[..]),
            Cow::Owned(_) => panic!("无转义序列应零拷贝借用"),
        }
    }

    #[test]
    fn unescape_rbsp_drops_emulation_bytes() {
        // 00 00 03 01 → 00 00 01；00 00 03 03 → 00 00 03
        let nal = [0xAB, 0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x03, 0x03, 0xCD];
        assert_eq!(
            unescape_rbsp(&nal).as_ref(),
            &[0xAB, 0x00, 0x00, 0x01, 0x00, 0x00, 0x03, 0xCD][..]
        );
        // 单 03 不在前导 00 00 后：不动
        let no_esc = [0x03, 0x00, 0x03, 0x01];
        assert_eq!(unescape_rbsp(&no_esc).as_ref(), &no_esc[..]);
        // 末尾悬垂 00 00（无 03）：不动
        let tail = [0x11, 0x00, 0x00];
        assert_eq!(unescape_rbsp(&tail).as_ref(), &tail[..]);
    }

    // —— avcC 记录 ——

    /// 手工黄金 avcC：version 1、Baseline(66)/C0/level 30、length 4 字节、
    /// 1 SPS（67 42 C0 1E DA 79）+ 1 PPS（68 CE 3C 80）。
    const GOLDEN_AVCC: &[u8] = &[
        0x01, 0x42, 0xC0, 0x1E, 0xFF,
        0xE1, // 头 6 字节：lsm1=3(FF 低 2 位)、nSPS=1(E1 低 5 位)
        0x00, 0x06, 0x67, 0x42, 0xC0, 0x1E, 0xDA, 0x79, // SPS：u16 长 6 + NAL
        0x01, // nPPS = 1
        0x00, 0x04, 0x68, 0xCE, 0x3C, 0x80, // PPS：u16 长 4 + NAL
    ];

    #[test]
    fn avcc_record_golden() {
        let rec = AvcConfigRecord::parse(GOLDEN_AVCC).unwrap();
        assert_eq!(rec.configuration_version, 1);
        assert_eq!(rec.profile_indication, 66);
        assert_eq!(rec.profile_compatibility, 0xC0);
        assert_eq!(rec.level_indication, 30);
        assert_eq!(rec.length_size, 4);
        assert_eq!(rec.sps_nals, vec![vec![0x67, 0x42, 0xC0, 0x1E, 0xDA, 0x79]]);
        assert_eq!(rec.pps_nals, vec![vec![0x68, 0xCE, 0x3C, 0x80]]);
    }

    #[test]
    fn avcc_record_errors() {
        // 过短
        assert!(matches!(
            AvcConfigRecord::parse(&[0x01, 0x42, 0xC0]),
            Err(H264Error::InvalidData(msg)) if msg.contains("too short")
        ));
        // nSPS=1 但 SPS 条目截断
        let mut truncated = GOLDEN_AVCC[..10].to_vec();
        truncated[5] = 0xE1; // nSPS=1（原本就是）
        assert!(matches!(
            AvcConfigRecord::parse(&truncated),
            Err(H264Error::InvalidData(msg)) if msg.contains("sps")
        ));
        // PPS 计数字节缺失（截到 SPS 条目末尾，不含 nPPS 字节）
        let no_pps_count = &GOLDEN_AVCC[..14];
        assert!(matches!(
            AvcConfigRecord::parse(no_pps_count),
            Err(H264Error::InvalidData(msg)) if msg.contains("pps count")
        ));
        // NAL 首字节 forbidden 位置位
        let mut bad_nal = GOLDEN_AVCC.to_vec();
        bad_nal[8] = 0xE7; // SPS NAL 头
        assert!(matches!(
            AvcConfigRecord::parse(&bad_nal),
            Err(H264Error::InvalidData(msg)) if msg.contains("forbidden")
        ));
    }
}

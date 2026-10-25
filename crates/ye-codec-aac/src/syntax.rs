//! AAC 码流语法层：位读取器、AudioSpecificConfig、ADTS/LOAS 剥壳、
//! raw data block 全量语法（noiseless tools：section/scalefactor/
//! pulse/tns 数据）。
//!
//! 语法承载的谱系数 Huffman 解谱与重量化由本模块在语法点为界调用
//! [`crate::spectral`] 完成（位流上 spectral_data 紧随 tns_data 之后，
//! 两模块共享同一 [`BitReader`] 推进）。
//!
//! ## 语法对齐声明
//!
//! 位序、字段宽度与重建链与 ISO/IEC 14496-3 一致，并与对拍 oracle
//! （FFmpeg `aacdec_template.c`，tag n4.4）逐项核对：
//! - section_data 逃逸长度位宽 3（EIGHT_SHORT）/ 5（长窗），全 1 续读；
//! - scalefactor 三条差分链（常规/强度/噪声）初值
//!   `global_gain / global_gain−90 / 0`，差分跨窗组累积；
//! - 常规链值域 0..=255（越界即语法违例）；强度链按 FFmpeg 裁剪区间
//!   [−155,100]；
//! - pulse：num_pulse = u(2)+1，首脉冲 sfb u(6)+偏移 u(5)，后续相对；
//! - tns：n_filt u(2−is8) 直接值（0 = 无）；coef_res 每窗一次；
//!   order 上限 7（短）/ 12（LC 长）。
//!
//! ## 失败模式
//!
//! 位越界、保留值（ms_present=3、sect_cb=12、LC 的 predictor 等）、
//! 越界频带号 → [`AacError::InvalidData`]；PCE/CCE/PNS 噪声码本/
//! SBR 扩展等超出 AAC-LC 范围的形态 → [`AacError::Unsupported`]
//! （如实报错，绝不静默错帧）。

use crate::error::AacError;
use crate::spectral;
use crate::tables::{self, HuffTable};

// ---------------------------------------------------------------------------
// 位读取器
// ---------------------------------------------------------------------------

/// MSB-first 位读取器（码流只读视图，越界一律 `Err` 不 panic）。
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    /// 位偏移（指向下一未读位）。
    pos: usize,
}

impl<'a> BitReader<'a> {
    /// 构造：`data` 全长可用。
    pub(crate) fn new(data: &'a [u8]) -> BitReader<'a> {
        BitReader { data, pos: 0 }
    }

    /// 构造：自 `bit_offset` 位起可读（LOAS 载荷非字节对齐）。
    pub(crate) fn new_at(data: &'a [u8], bit_offset: usize) -> BitReader<'a> {
        BitReader {
            data,
            pos: bit_offset,
        }
    }

    /// 剩余可读位数。
    pub(crate) fn remaining(&self) -> usize {
        self.data.len() * 8 - self.pos.min(self.data.len() * 8)
    }

    /// 读 1 位。
    pub(crate) fn bit(&mut self) -> Result<u8, AacError> {
        if self.pos >= self.data.len() * 8 {
            return Err(AacError::InvalidData("位读取越界".into()));
        }
        let byte = self.data[self.pos / 8];
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(b)
    }

    /// 读 `n` 位（n ≤ 32），MSB 在前，返回值低位对齐。
    pub(crate) fn bits(&mut self, n: u32) -> Result<u32, AacError> {
        debug_assert!(n <= 32);
        if self.remaining() < n as usize {
            return Err(AacError::InvalidData(format!("读 {n} 位越界")));
        }
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | u32::from(self.bit()?);
        }
        Ok(v)
    }

    /// 跳过到字节边界。
    pub(crate) fn byte_align(&mut self) {
        self.pos = self.pos.div_ceil(8) * 8;
    }

    /// 跳过 `n` 位。
    pub(crate) fn skip(&mut self, n: usize) -> Result<(), AacError> {
        if self.remaining() < n {
            return Err(AacError::InvalidData("跳位越界".into()));
        }
        self.pos += n;
        Ok(())
    }

    /// 高位窥视 `n` 位（不推进；越界位按 0 补齐）。
    pub(crate) fn peek(&self, n: u32) -> u32 {
        let mut v = 0u32;
        for i in 0..n {
            let p = self.pos + i as usize;
            let b = if p < self.data.len() * 8 {
                (self.data[p / 8] >> (7 - p % 8)) & 1
            } else {
                0
            };
            v = (v << 1) | u32::from(b);
        }
        v
    }

    /// Huffman 解码：逐位读入并以 (len, code) 查表。
    pub(crate) fn huff(&mut self, table: &HuffTable) -> Result<u16, AacError> {
        let mut code = 0u32;
        for len in 1..=table.max_len() {
            code = (code << 1) | u32::from(self.bit()?);
            if let Some(sym) = table.symbol(len, code) {
                return Ok(sym);
            }
        }
        Err(AacError::InvalidData(
            "Huffman 码字不匹配（码表耗尽）".into(),
        ))
    }

    /// scalefactor 差分链一步：符号索引 − 60。
    pub(crate) fn sf_delta(&mut self) -> Result<i32, AacError> {
        let s = self.huff(tables::scalefactor_huff())?;
        Ok(i32::from(s) - 60)
    }

    /// 谱码本一步：返回符号索引。
    pub(crate) fn spectral_symbol(&mut self, cb: u8) -> Result<u16, AacError> {
        let table = tables::spectral_huff(cb)?;
        self.huff(table)
    }
}

// ---------------------------------------------------------------------------
// AudioSpecificConfig
// ---------------------------------------------------------------------------

/// AudioSpecificConfig（ISO/IEC 14496-3 §1.6.2.1）解析结果（GA 子集）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AudioSpecificConfig {
    /// audioObjectType（本实现支持集：2 = AAC-LC）。
    pub object_type: u8,
    /// samplingFrequencyIndex（0..=12）。
    pub sampling_index: u8,
    /// 采样率（Hz）。
    pub sample_rate: u32,
    /// channelConfiguration（支持集：1 = 单声道、2 = 立体声）。
    pub channel_config: u8,
    /// frameLengthFlag：true = 960/120 帧长。
    pub frame_length_short: bool,
}

impl AudioSpecificConfig {
    /// 从位流解析（含 GASpecificConfig）。
    ///
    /// # Errors
    /// object type ≠ 2（AAC-LC）→ [`AacError::Unsupported`]；
    /// channel_config = 0（PCE）或 > 2 → [`AacError::Unsupported`]；
    /// 采样率索引 15（显式 24 位）→ [`AacError::Unsupported`]（无频带表）；
    /// 其余语法违例 → [`AacError::InvalidData`]。
    pub(crate) fn parse(r: &mut BitReader) -> Result<AudioSpecificConfig, AacError> {
        let object_type = r.bits(5)? as u8;
        let object_type = if object_type == 31 {
            32 + r.bits(6)? as u8
        } else {
            object_type
        };
        if object_type != 2 {
            return Err(AacError::Unsupported(format!(
                "audioObjectType={object_type}（仅支持 AAC-LC=2）"
            )));
        }
        let sfi = r.bits(4)? as u8;
        let sample_rate = if sfi == 15 {
            let rate = r.bits(24)?;
            return Err(AacError::Unsupported(format!(
                "显式采样率 {rate} Hz（无 scalefactor 频带表）"
            )));
        } else {
            *tables::SAMPLE_RATES.get(sfi as usize).ok_or_else(|| {
                AacError::InvalidData(format!("samplingFrequencyIndex {sfi} 越界"))
            })?
        };
        let channel_config = r.bits(4)? as u8;
        // GASpecificConfig
        let frame_length_short = r.bit()? == 1;
        if r.bit()? == 1 {
            r.skip(14)?; // coreCoderDelay
        }
        let extension_flag = r.bit()? == 1;
        if channel_config == 0 {
            return Err(AacError::Unsupported(
                "channelConfiguration=0（program_config_element 布局）".into(),
            ));
        }
        if channel_config > 2 {
            return Err(AacError::Unsupported(format!(
                "channelConfiguration={channel_config}（>2 声道）"
            )));
        }
        if extension_flag {
            return Err(AacError::Unsupported(
                "GASpecificConfig extension_flag=1".into(),
            ));
        }
        Ok(AudioSpecificConfig {
            object_type,
            sampling_index: sfi,
            sample_rate,
            channel_config,
            frame_length_short,
        })
    }
}

// ---------------------------------------------------------------------------
// ADTS 剥壳
// ---------------------------------------------------------------------------

/// ADTS 帧头（ISO/IEC 13818-7 §6.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AdtsHeader {
    /// object type（profile + 1；2 = LC）。
    pub object_type: u8,
    /// samplingFrequencyIndex。
    pub sampling_index: u8,
    /// channelConfiguration。
    pub channel_config: u8,
    /// 帧总长（字节，含头）。
    pub frame_length: usize,
    /// 头长（7，或带 CRC 的 9）。
    pub header_length: usize,
}

/// 探测：缓冲首部是否为 ADTS 同步字。
pub(crate) fn is_adts(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == 0xFF && data[1] & 0xF6 == 0xF0
}

/// 解析一个 ADTS 帧头（`data` 首字节起）。
///
/// # Errors
/// 同步字/层/长度违例 → [`AacError::InvalidData`]；
/// num_aac_frames ≠ 1、profile 非 LC → [`AacError::Unsupported`]。
pub(crate) fn parse_adts(data: &[u8]) -> Result<AdtsHeader, AacError> {
    if data.len() < 7 {
        return Err(AacError::InvalidData("ADTS 头截断（<7 字节）".into()));
    }
    let mut r = BitReader::new(data);
    if r.bits(12)? != 0xFFF {
        return Err(AacError::InvalidData("ADTS 同步字缺失".into()));
    }
    let _mpeg_id = r.bits(1)?;
    if r.bits(2)? != 0 {
        return Err(AacError::InvalidData("ADTS layer ≠ 0".into()));
    }
    let protection_absent = r.bits(1)?;
    let profile = r.bits(2)? as u8;
    let sampling_index = r.bits(4)? as u8;
    if sampling_index as usize >= tables::SAMPLE_RATES.len() {
        return Err(AacError::InvalidData(format!(
            "ADTS samplingFrequencyIndex {sampling_index} 越界"
        )));
    }
    let _private = r.bits(1)?;
    let channel_config = r.bits(3)? as u8;
    let _orig_home = r.bits(2)?;
    let _copyright_id = r.bits(2)?;
    let frame_length = r.bits(13)? as usize;
    let _buffer_fullness = r.bits(11)?;
    let num_frames = r.bits(2)? + 1;
    if num_frames != 1 {
        return Err(AacError::Unsupported(format!(
            "ADTS num_aac_frames={num_frames}（>1 raw data block/帧）"
        )));
    }
    let header_length = if protection_absent == 1 { 7 } else { 9 };
    if frame_length < header_length {
        return Err(AacError::InvalidData("ADTS 帧长短于头".into()));
    }
    if frame_length > data.len() {
        return Err(AacError::InvalidData(format!(
            "ADTS 帧长 {frame_length} 超出可用 {} 字节",
            data.len()
        )));
    }
    let object_type = profile + 1;
    if object_type != 2 {
        return Err(AacError::Unsupported(format!(
            "ADTS profile → object type {object_type}（仅 LC）"
        )));
    }
    Ok(AdtsHeader {
        object_type,
        sampling_index,
        channel_config,
        frame_length,
        header_length,
    })
}

// ---------------------------------------------------------------------------
// LOAS/LATM 剥壳
// ---------------------------------------------------------------------------

/// LOAS/LATM 流状态（StreamMuxConfig 跨帧记忆）。
#[derive(Debug, Clone, Default)]
pub(crate) struct LoasState {
    /// 带内取得的 ASC。
    pub config: Option<AudioSpecificConfig>,
    frame_length_type: u8,
    frame_length_bits: u16,
}

/// LOAS 帧解析结果。
pub(crate) struct LoasFrame<'a> {
    /// raw data block 载荷所在字节切片（自包含其首位的字节起）。
    pub payload: &'a [u8],
    /// 载荷首位的位偏移（相对 `payload[0]`，非字节对齐时非 0）。
    pub bit_offset: usize,
    /// 载荷有效位长。
    pub payload_bits: usize,
}

/// 探测：缓冲首部是否为 LOAS 同步字（0x56x）。
pub(crate) fn is_loas(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == 0x56 && data[1] & 0xE0 == 0xE0
}

impl LoasState {
    /// 解析一个 LOAS 帧（AudioSyncStream）。
    ///
    /// # Errors
    /// 同步字/长度违例 → [`AacError::InvalidData`]；
    /// 多 program/多 layer/CELP-HVXC 定长 → [`AacError::Unsupported`]。
    pub(crate) fn parse_frame<'a>(&mut self, data: &'a [u8]) -> Result<LoasFrame<'a>, AacError> {
        if data.len() < 3 {
            return Err(AacError::InvalidData("LOAS 帧截断（<3 字节）".into()));
        }
        let mut r = BitReader::new(data);
        if r.bits(11)? != 0x2B7 {
            return Err(AacError::InvalidData("LOAS 同步字缺失".into()));
        }
        let mux_length = r.bits(13)? as usize + 3;
        if mux_length > data.len() {
            return Err(AacError::InvalidData(format!(
                "LOAS 帧长 {mux_length} 超出可用 {} 字节",
                data.len()
            )));
        }

        let use_same_mux = r.bit()? == 1;
        if !use_same_mux {
            self.read_stream_mux_config(&mut r)?;
        } else if self.config.is_none() {
            return Err(AacError::InvalidData(
                "LOAS 复用先前配置但首帧未带配置".into(),
            ));
        }

        // PayloadLengthInfo
        let payload_bits = match self.frame_length_type {
            0 => {
                let mut mux_slot_length = 0usize;
                loop {
                    let tmp = r.bits(8)? as usize;
                    mux_slot_length += tmp;
                    if tmp != 255 {
                        break;
                    }
                }
                mux_slot_length * 8
            }
            1 => self.frame_length_bits as usize,
            3..=5 => {
                r.skip(2)?; // mux_slot_length_coded
                return Err(AacError::Unsupported(
                    "LOAS frame_length_type=3/4/5（CELP）".into(),
                ));
            }
            _ => {
                return Err(AacError::Unsupported(
                    "LOAS frame_length_type=6/7（HVXC）".into(),
                ));
            }
        };
        if payload_bits == 0 || r.remaining() < payload_bits {
            return Err(AacError::InvalidData(format!(
                "LOAS 载荷 {payload_bits} 位越界（剩 {}）",
                r.remaining()
            )));
        }
        let bit_offset = r.pos % 8;
        let payload = &data[r.pos / 8..mux_length];
        Ok(LoasFrame {
            payload,
            bit_offset,
            payload_bits,
        })
    }

    /// StreamMuxConfig（接受集对齐 FFmpeg read_stream_mux_config）。
    fn read_stream_mux_config(&mut self, r: &mut BitReader) -> Result<(), AacError> {
        let audio_mux_version = r.bits(1)? as u8;
        let mut audio_mux_version_a = 0u8;
        if audio_mux_version == 1 {
            audio_mux_version_a = r.bits(1)? as u8;
        }
        if audio_mux_version_a != 0 {
            return Err(AacError::Unsupported("audioMuxVersionA=1".into()));
        }
        if audio_mux_version == 1 {
            Self::latm_get_value(r)?; // taraFullness
        }
        let _all_same_time = r.bit()?;
        let _num_sub_frames = r.bits(6)?;
        if r.bits(4)? != 0 {
            return Err(AacError::Unsupported("LATM 多 program".into()));
        }
        if r.bits(3)? != 0 {
            return Err(AacError::Unsupported("LATM 多 layer".into()));
        }
        if audio_mux_version == 0 {
            self.config = Some(AudioSpecificConfig::parse(r)?);
        } else {
            let asc_len = Self::latm_get_value(r)?; // ascLen（位）
            let before = r.remaining();
            self.config = Some(AudioSpecificConfig::parse(r)?);
            let consumed = before - r.remaining();
            r.skip(asc_len.saturating_sub(consumed))?; // asclen 对齐余量
        }
        self.frame_length_type = r.bits(3)? as u8;
        match self.frame_length_type {
            0 => {
                r.skip(8)?; // latmBufferFullness
            }
            1 => {
                self.frame_length_bits = r.bits(9)? as u16;
            }
            3..=5 => {
                r.skip(6)?; // CELP 表索引
            }
            6 | 7 => {
                r.skip(1)?; // HVXC 表索引
            }
            _ => {}
        }
        if r.bit()? == 1 {
            // otherData
            if audio_mux_version == 1 {
                Self::latm_get_value(r)?; // other_data_bits
            } else {
                loop {
                    let esc = r.bit()?;
                    r.skip(8)?;
                    if esc == 0 {
                        break;
                    }
                }
            }
        }
        if r.bit()? == 1 {
            r.skip(8)?; // config_crc
        }
        Ok(())
    }

    /// latm_get_value：u(2) 字节数 + 数据（返回位长）。
    fn latm_get_value(r: &mut BitReader) -> Result<usize, AacError> {
        let length = r.bits(2)? as usize;
        let bits = (length + 1) * 8;
        r.skip(bits)?;
        Ok(bits)
    }
}

// ---------------------------------------------------------------------------
// raw data block 语法
// ---------------------------------------------------------------------------

/// 窗序列常量（ISO window_sequence）。
pub(crate) const ONLY_LONG: u8 = 0;
/// 长→短过渡窗。
pub(crate) const LONG_START: u8 = 1;
/// 八短窗。
pub(crate) const EIGHT_SHORT: u8 = 2;

/// section codebook 常量。
pub(crate) const CB_ZERO: u8 = 0;
/// PNS 噪声码本（13）。
pub(crate) const CB_NOISE: u8 = 13;
/// 强度立体声·反相（14，INTENSITY_BT2）。
pub(crate) const CB_INTENSITY_HI: u8 = 14;
/// 强度立体声·同相（15，INTENSITY_BT）。
pub(crate) const CB_INTENSITY_LO: u8 = 15;

/// ICS 信息（ics_info + 派生频带几何）。
#[derive(Debug, Clone)]
pub(crate) struct IcsInfo {
    /// window_sequence（0..=3）。
    pub window_sequence: u8,
    /// window_shape：0 = KBD，1 = sine。
    pub window_shape: bool,
    /// max_sfb。
    pub max_sfb: u8,
    /// 窗组数（1..=8）。
    pub num_window_groups: u8,
    /// 各窗组长（窗数；仅前 num_window_groups 项有效）。
    pub group_len: [u8; 8],
    /// 频带边界表（num_swb+1 项）。
    pub swb_offset: &'static [u16],
    /// 频带数。
    pub num_swb: u8,
    /// TNS 最大频带数。
    pub tns_max_bands: u8,
}

/// 单通道（SCE/LFE/CPE 成员）完整语法与谱。
#[derive(Debug, Clone)]
pub(crate) struct ChannelSyntax {
    /// ics（共窗时为共享副本）。
    pub ics: IcsInfo,
    /// 逐 (组×频带) section codebook（组优先，每组 max_sfb 项）。
    pub codebooks: Vec<u8>,
    /// 重建 scalefactor 链（同排布；强度位 = is_pos，ZERO 位 = 0）。
    pub scalefactors: Vec<i16>,
    /// tns_data。
    pub tns: Option<TnsData>,
    /// 重量化谱（EIGHT_SHORT 为 8×128 拼接，pulse 已应用）。
    pub coeffs: Vec<f32>,
}

/// pulse_data。
#[derive(Debug, Clone)]
pub(crate) struct PulseData {
    /// 各脉冲系数位置。
    pub pos: Vec<u16>,
    /// 各脉冲幅度 u(4)。
    pub amp: Vec<u8>,
}

/// 单窗单滤波器 TNS 数据。
#[derive(Debug, Clone)]
pub(crate) struct TnsFilter {
    /// 滤波长度（频带数，自窗末向上计数）。
    pub length: u8,
    /// 阶数。
    pub order: u8,
    /// 方向（false = 升序）。
    pub direction: bool,
    /// 格点系数值（解析时经 [`crate::tables::tns_coef_map`] 折算）。
    pub coefs: Vec<f32>,
}

/// tns_data。
#[derive(Debug, Clone)]
pub(crate) struct TnsData {
    /// 每窗滤波器列表（长窗 1 窗、短窗 8 窗）。
    pub filters: Vec<Vec<TnsFilter>>,
}

/// raw data block 元素。
#[derive(Debug, Clone)]
pub(crate) enum RawElement {
    /// 单通道元素（SCE/LFE）。
    Single(ChannelSyntax),
    /// 通道对元素（CPE）。
    Pair {
        /// 共窗信息（common_window=1 时）。
        common: Option<PairCommon>,
        /// 两通道。
        ch: [ChannelSyntax; 2],
    },
    /// 数据流元素（载荷已跳过）。
    DataStream,
    /// 填充元素（载荷已跳过）。
    Fill,
}

/// CPE 共窗信息。
#[derive(Debug, Clone)]
pub(crate) struct PairCommon {
    /// ms_present。
    pub ms_present: u8,
    /// ms_used 位图（组×频带）。
    pub ms_used: Vec<bool>,
}

/// 解析 ics_info（ISO 4.6.2.2）。
///
/// # Errors
/// 保留位、LC predictor、max_sfb > num_swb → [`AacError::InvalidData`]/
/// [`AacError::Unsupported`]。
pub(crate) fn parse_ics_info(
    r: &mut BitReader,
    asc: &AudioSpecificConfig,
) -> Result<IcsInfo, AacError> {
    if r.bit()? != 0 {
        return Err(AacError::InvalidData("ics_info 保留位为 1".into()));
    }
    let window_sequence = r.bits(2)? as u8;
    let window_shape = r.bit()? == 1;
    let (max_sfb_bits, short) = if window_sequence == EIGHT_SHORT {
        (4u32, true)
    } else {
        (6u32, false)
    };
    let max_sfb = r.bits(max_sfb_bits)? as u8;
    let mut num_window_groups = 1u8;
    let mut group_len = [1u8; 8];
    if short {
        for _ in 0..7 {
            if r.bit()? == 1 {
                group_len[num_window_groups as usize - 1] += 1;
            } else {
                num_window_groups += 1;
            }
        }
    }
    let idx = asc.sampling_index as usize;
    let (swb_offset, num_swb, tns_max_bands) = if short {
        if asc.frame_length_short {
            (
                tables::swb_offset_short_120(idx).ok_or_else(|| bad_sfi(idx))?,
                tables::num_swb_short_120(idx).ok_or_else(|| bad_sfi(idx))?,
                tables::tns_max_bands_short(idx).ok_or_else(|| bad_sfi(idx))?,
            )
        } else {
            (
                tables::swb_offset_short(idx).ok_or_else(|| bad_sfi(idx))?,
                tables::num_swb_short(idx).ok_or_else(|| bad_sfi(idx))?,
                tables::tns_max_bands_short(idx).ok_or_else(|| bad_sfi(idx))?,
            )
        }
    } else if asc.frame_length_short {
        (
            tables::swb_offset_long_960(idx).ok_or_else(|| bad_sfi(idx))?,
            tables::num_swb_long_960(idx).ok_or_else(|| bad_sfi(idx))?,
            tables::tns_max_bands_long(idx).ok_or_else(|| bad_sfi(idx))?,
        )
    } else {
        (
            tables::swb_offset_long(idx).ok_or_else(|| bad_sfi(idx))?,
            tables::num_swb_long(idx).ok_or_else(|| bad_sfi(idx))?,
            tables::tns_max_bands_long(idx).ok_or_else(|| bad_sfi(idx))?,
        )
    };
    if !short {
        // predictor_data_present：LC 不允许（Main/LTP 工具不在范围）
        if r.bit()? == 1 {
            return Err(AacError::Unsupported(
                "predictor_data_present=1（AAC-Main/LTP 预测器）".into(),
            ));
        }
    }
    if max_sfb > num_swb {
        return Err(AacError::InvalidData(format!(
            "max_sfb={max_sfb} 超频带数 {num_swb}"
        )));
    }
    Ok(IcsInfo {
        window_sequence,
        window_shape,
        max_sfb,
        num_window_groups,
        group_len,
        swb_offset,
        num_swb,
        tns_max_bands,
    })
}

fn bad_sfi(idx: usize) -> AacError {
    AacError::InvalidData(format!("采样率索引 {idx} 无频带表"))
}

/// individual_channel_stream：global_gain →（独立窗时 ics_info 由调用方
/// 先解析）→ section/scalefactor/pulse/tns/gain_control → spectral。
///
/// # Errors
/// 见各子语法；PNS 噪声码本 → [`AacError::Unsupported`]。
pub(crate) fn parse_channel(
    r: &mut BitReader,
    ics: &IcsInfo,
    global_gain: u8,
) -> Result<ChannelSyntax, AacError> {
    let codebooks = parse_section_data(r, ics)?;
    let scalefactors = parse_scale_factor_data(r, &codebooks, global_gain, ics)?;
    // pulse_data_present（scale_flag 恒 0——LC）
    let pulse = if r.bit()? == 1 {
        if ics.window_sequence == EIGHT_SHORT {
            return Err(AacError::InvalidData(
                "pulse 工具不允许出现在 EIGHT_SHORT".into(),
            ));
        }
        Some(parse_pulse(r, ics)?)
    } else {
        None
    };
    // tns_data_present
    let tns = if r.bit()? == 1 {
        Some(parse_tns(r, ics)?)
    } else {
        None
    };
    // gain_control_data_present：SSR 工具——按 FFmpeg 语义跳过语法（LC 不出现）
    // Ff08LC 6d414e0d4f1a51fa73b0Ff1b51fa73b052198df38fc78bed6cd530014e0d65bd52a0589e76caFf09
    if r.bit()? == 1 {
        skip_gain_control(r, ics)?;
    }
    // spectral_data：解谱 + 重量化 + pulse 应用（pulse/tns 数据联合点）
    let coeffs = spectral::decode_and_requant(r, ics, &codebooks, &scalefactors, &pulse)?;
    Ok(ChannelSyntax {
        ics: ics.clone(),
        codebooks,
        scalefactors,
        tns,
        coeffs,
    })
}

/// section_data（ISO 4.6.2.1）：返回组优先的逐频带 codebook。
///
/// # Errors
/// sect_cb ∈ {12, 13}（保留/PNS）→ [`AacError::Unsupported`]；
/// 越界 → [`AacError::InvalidData`]。
pub(crate) fn parse_section_data(r: &mut BitReader, ics: &IcsInfo) -> Result<Vec<u8>, AacError> {
    let n_bands = ics.num_window_groups as usize * ics.max_sfb as usize;
    let mut out = Vec::with_capacity(n_bands);
    let len_bits = if ics.window_sequence == EIGHT_SHORT {
        3u32
    } else {
        5u32
    };
    let escape = (1u32 << len_bits) - 1;
    for _g in 0..ics.num_window_groups {
        let mut k = 0usize;
        while k < ics.max_sfb as usize {
            let sect_cb = r.bits(4)? as u8;
            if sect_cb == 12 {
                return Err(AacError::InvalidData("sect_cb=12（保留）".into()));
            }
            if sect_cb == CB_NOISE {
                return Err(AacError::Unsupported(
                    "PNS 噪声码本（noiseless 重建噪声波形是实现定义，".to_string()
                        + "如需支持回施工图裁决）",
                ));
            }
            let mut sect_end = k;
            loop {
                let incr = r.bits(len_bits)? as usize;
                sect_end += incr;
                if sect_end > ics.max_sfb as usize {
                    return Err(AacError::InvalidData(format!(
                        "section 长度越出 max_sfb={}（{}）",
                        ics.max_sfb, sect_end
                    )));
                }
                if incr as u32 != escape {
                    break;
                }
            }
            for _ in k..sect_end {
                out.push(sect_cb);
            }
            k = sect_end;
        }
    }
    Ok(out)
}

/// scale_factor_data（ISO 4.6.2.3）：三条差分链重建。
///
/// 常规链 = global_gain 起步、值域 0..=255；强度链 = 0 起步、裁剪
/// [−155,100]；噪声链不支持（section 层已拒）。ZERO 频带不耗码字。
///
/// # Errors
/// 常规链越界 / 位越界 → [`AacError::InvalidData`]。
pub(crate) fn parse_scale_factor_data(
    r: &mut BitReader,
    codebooks: &[u8],
    global_gain: u8,
    ics: &IcsInfo,
) -> Result<Vec<i16>, AacError> {
    let _ = ics;
    let mut offset_regular = i32::from(global_gain);
    let mut offset_intensity = 0i32;
    let mut out = Vec::with_capacity(codebooks.len());
    for &cb in codebooks {
        match cb {
            CB_ZERO => out.push(0),
            CB_INTENSITY_HI | CB_INTENSITY_LO => {
                offset_intensity += r.sf_delta()?;
                let clipped = offset_intensity.clamp(-155, 100);
                out.push(clipped as i16);
            }
            _ => {
                offset_regular += r.sf_delta()?;
                if !(0..=255).contains(&offset_regular) {
                    return Err(AacError::InvalidData(format!(
                        "scalefactor 链值 {offset_regular} 越界（0..=255）"
                    )));
                }
                out.push(offset_regular as i16);
            }
        }
    }
    Ok(out)
}

/// pulse_data（ISO 4.6.2.5；对齐 FFmpeg decode_pulses 的位置语义）。
///
/// # Errors
/// 频带号/位置越界 → [`AacError::InvalidData`]。
pub(crate) fn parse_pulse(r: &mut BitReader, ics: &IcsInfo) -> Result<PulseData, AacError> {
    let num_pulse = r.bits(2)? as usize + 1;
    let pulse_swb = r.bits(6)? as usize;
    if pulse_swb >= ics.num_swb as usize {
        return Err(AacError::InvalidData(format!(
            "pulse_start_sfb={pulse_swb} 超频带数 {}",
            ics.num_swb
        )));
    }
    let limit = *ics
        .swb_offset
        .last()
        .ok_or_else(|| AacError::InvalidData("频带边界表为空".into()))? as usize;
    let mut pos = Vec::with_capacity(num_pulse);
    let mut amp = Vec::with_capacity(num_pulse);
    pos.push(ics.swb_offset[pulse_swb] as usize + r.bits(5)? as usize);
    if pos[0] >= limit {
        return Err(AacError::InvalidData(format!(
            "pulse 位置 {} 超系数区 {}",
            pos[0], limit
        )));
    }
    amp.push(r.bits(4)? as u8);
    for _ in 1..num_pulse {
        let p = r.bits(5)? as usize + pos.last().copied().unwrap_or(0);
        if p >= limit {
            return Err(AacError::InvalidData(format!(
                "pulse 位置 {p} 超系数区 {limit}"
            )));
        }
        pos.push(p);
        amp.push(r.bits(4)? as u8);
    }
    Ok(PulseData {
        pos: pos.into_iter().map(|p| p as u16).collect(),
        amp,
    })
}

/// tns_data（ISO 4.6.2.6；对齐 FFmpeg decode_tns）。
///
/// # Errors
/// order 超上限 / 位越界 → [`AacError::InvalidData`]。
pub(crate) fn parse_tns(r: &mut BitReader, ics: &IcsInfo) -> Result<TnsData, AacError> {
    let is8 = ics.window_sequence == EIGHT_SHORT;
    let tns_max_order: u8 = if is8 { 7 } else { 12 };
    let num_windows = if is8 { 8usize } else { 1 };
    let mut filters = Vec::with_capacity(num_windows);
    for _w in 0..num_windows {
        let n_filt = r.bits(if is8 { 1 } else { 2 })? as usize;
        let mut filts = Vec::with_capacity(n_filt);
        if n_filt > 0 {
            let coef_res = r.bit()?;
            for _ in 0..n_filt {
                let length = r.bits(if is8 { 4 } else { 6 })? as u8;
                let order = r.bits(if is8 { 3 } else { 5 })? as u8;
                if order > tns_max_order {
                    return Err(AacError::InvalidData(format!(
                        "TNS 阶数 {order} 超上限 {tns_max_order}"
                    )));
                }
                let mut direction = false;
                let mut coefs = Vec::new();
                if order > 0 {
                    direction = r.bit()? == 1;
                    let coef_compress = r.bit()?;
                    let coef_len = u32::from(coef_res) + 3 - u32::from(coef_compress);
                    let sel = 2 * coef_compress as usize + coef_res as usize;
                    let map = tables::tns_coef_map(sel);
                    debug_assert_eq!(map.len(), 1 << coef_len);
                    for _ in 0..order {
                        let v = r.bits(coef_len)? as usize;
                        coefs.push(*map.get(v).ok_or_else(|| {
                            AacError::InvalidData(
                                "TNS 系数索引越界（coef_len 与映射表不符）".into(),
                            )
                        })?);
                    }
                }
                filts.push(TnsFilter {
                    length,
                    order,
                    direction,
                    coefs,
                });
            }
        }
        filters.push(filts);
    }
    Ok(TnsData { filters })
}

/// gain_control_data 语法跳过（SSR 工具；对齐 FFmpeg decode_gain_control
/// 的读取形状——仅越过位，不施加增益）。
fn skip_gain_control(r: &mut BitReader, ics: &IcsInfo) -> Result<(), AacError> {
    /// [窗数, 首窗标志, 位宽] per window_sequence。
    const GAIN_MODE: [[u32; 3]; 4] = [[1, 0, 5], [2, 1, 2], [8, 0, 2], [2, 1, 5]];
    let mode = GAIN_MODE[ics.window_sequence as usize];
    let max_band = r.bits(2)? as usize;
    for _bd in 0..max_band {
        for wd in 0..mode[0] {
            let adjust_num = r.bits(3)? as usize;
            for _ in 0..adjust_num {
                let extra = if wd == 0 && mode[1] == 1 { 4 } else { mode[2] };
                r.skip(4 + extra as usize)?;
            }
        }
    }
    Ok(())
}

/// raw data block 解析结果。
pub(crate) struct RawDataBlock<'a> {
    /// 元素序列。
    pub elements: Vec<RawElement>,
    marker: std::marker::PhantomData<&'a [u8]>,
}

/// 解析 raw data block（SCE/CPE/LFE/DSE/FIL/END；CCE/PCE 拒绝），
/// 自 `bit_offset` 位起（LOAS 载荷非字节对齐；ADTS/raw 传 0）。
///
/// # Errors
/// 见 [`parse_channel`] 与各元素语法。
pub(crate) fn parse_raw_data_block_at<'a>(
    data: &'a [u8],
    bit_offset: usize,
    asc: &AudioSpecificConfig,
) -> Result<RawDataBlock<'a>, AacError> {
    let mut reader = BitReader::new_at(data, bit_offset);
    let mut elements = Vec::new();
    loop {
        if reader.remaining() < 3 {
            return Err(AacError::InvalidData(
                "raw data block 缺元素 id（越界）".into(),
            ));
        }
        let elem_type = reader.bits(3)? as u8;
        if elem_type == 7 {
            break; // END
        }
        // element_instance_tag：SCE/CPE/CCE/LFE/DSE 有；FIL 无（id 后
        // 直接是 count）——PCE 的 tag 由其自身语法携带（本实现不支持）。
        if elem_type != 6 {
            let _instance_tag = reader.bits(4)?;
        }
        match elem_type {
            0 | 3 => {
                let global_gain = reader.bits(8)? as u8;
                let ics = parse_ics_info(&mut reader, asc)?;
                let ch = parse_channel(&mut reader, &ics, global_gain)?;
                elements.push(RawElement::Single(ch));
            }
            1 => {
                let common_window = reader.bit()? == 1;
                let (common, shared_ics) = if common_window {
                    let ics = parse_ics_info(&mut reader, asc)?;
                    let ms_present = reader.bits(2)? as u8;
                    if ms_present == 3 {
                        return Err(AacError::InvalidData("ms_present=3（保留值）".into()));
                    }
                    let n = ics.num_window_groups as usize * ics.max_sfb as usize;
                    let ms_used = if ms_present == 1 {
                        let mut v = Vec::with_capacity(n);
                        for _ in 0..n {
                            v.push(reader.bit()? == 1);
                        }
                        v
                    } else {
                        vec![ms_present == 2; n]
                    };
                    (
                        Some(PairCommon {
                            ms_present,
                            ms_used,
                        }),
                        Some(ics),
                    )
                } else {
                    (None, None)
                };
                let gg0 = reader.bits(8)? as u8;
                let ch0 = match &shared_ics {
                    Some(ics) => parse_channel(&mut reader, ics, gg0)?,
                    None => {
                        let ics = parse_ics_info(&mut reader, asc)?;
                        parse_channel(&mut reader, &ics, gg0)?
                    }
                };
                let gg1 = reader.bits(8)? as u8;
                let ch1 = match &shared_ics {
                    Some(ics) => parse_channel(&mut reader, ics, gg1)?,
                    None => {
                        let ics = parse_ics_info(&mut reader, asc)?;
                        parse_channel(&mut reader, &ics, gg1)?
                    }
                };
                elements.push(RawElement::Pair {
                    common,
                    ch: [ch0, ch1],
                });
            }
            2 => {
                return Err(AacError::Unsupported(
                    "coupling_channel_element（CCE）".into(),
                ));
            }
            4 => {
                let byte_align = reader.bit()? == 1;
                let mut count = reader.bits(8)? as usize;
                if count == 255 {
                    count += reader.bits(8)? as usize;
                }
                if byte_align {
                    reader.byte_align();
                }
                reader.skip(count * 8)?;
                elements.push(RawElement::DataStream);
            }
            5 => {
                return Err(AacError::Unsupported(
                    "program_config_element（PCE）".into(),
                ));
            }
            6 => {
                let mut count = reader.bits(4)? as usize;
                if count == 15 {
                    count += (reader.bits(8)? as usize).wrapping_sub(1);
                }
                if reader.remaining() < count * 8 {
                    return Err(AacError::InvalidData("FIL 载荷越界".into()));
                }
                if count > 0 {
                    let ext_type = reader.peek(4) as u8;
                    if matches!(ext_type, 13 | 14) {
                        return Err(AacError::Unsupported("SBR 扩展载荷（HE-AAC）".into()));
                    }
                }
                reader.skip(count * 8)?;
                elements.push(RawElement::Fill);
            }
            _ => {
                return Err(AacError::InvalidData(format!("元素 id {elem_type} 未知")));
            }
        }
    }
    Ok(RawDataBlock {
        elements,
        marker: std::marker::PhantomData,
    })
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 位读取器基础（MSB-first、越界、peek、align）。
    #[test]
    fn bitreader_basics() {
        let data = [0b1011_0010, 0b0100_0001];
        let mut r = BitReader::new(&data);
        assert_eq!(r.bit().unwrap(), 1);
        assert_eq!(r.bits(3).unwrap(), 0b011);
        assert_eq!(r.peek(4), 0b0010);
        assert_eq!(r.bits(4).unwrap(), 0b0010);
        r.byte_align(); // 已对齐（8 位）
        assert_eq!(r.bits(8).unwrap(), 0b0100_0001);
        assert!(r.bit().is_err(), "越界读");
        let mut r2 = BitReader::new(&[0xFF]);
        assert_eq!(r2.peek(12), 0b1111_1111_0000, "越界 peek 补零");
        assert_eq!(r2.remaining(), 8);
        assert!(r2.skip(9).is_err());
    }

    /// ASC：AAC-LC 44.1kHz 立体声（ffmpeg aac 编码器典型 esds 值
    /// 0x1210 的逐位形态）。
    #[test]
    fn asc_parse_lc_44100_stereo() {
        // object=2(00010) sfi=4(0100) chan=2(0010) fl=0 dep=0 ext=0
        let bits = "00010 0100 0010 0 0 0";
        let bytes = bits_to_bytes(bits);
        let mut r = BitReader::new(&bytes);
        let asc = AudioSpecificConfig::parse(&mut r).unwrap();
        assert_eq!(asc.object_type, 2);
        assert_eq!(asc.sampling_index, 4);
        assert_eq!(asc.sample_rate, 44_100);
        assert_eq!(asc.channel_config, 2);
        assert!(!asc.frame_length_short);
        // 16 位 ASC（= 0x1210 的位形态），无余位
        assert_eq!(r.remaining(), 0);
    }

    /// ASC 拒绝路径：非 LC、PCE 布局、>2 声道、显式采样率。
    #[test]
    fn asc_parse_rejections() {
        let mk = |s: &str| {
            let b = bits_to_bytes(s);
            let mut r = BitReader::new(&b);
            AudioSpecificConfig::parse(&mut r)
        };
        assert!(matches!(
            mk("00011 0100 0010 0 0 0"),
            Err(AacError::Unsupported(ref m)) if m.contains("AAC-LC")
        ));
        assert!(matches!(
            mk("00010 0100 0000 0 0 0"),
            Err(AacError::Unsupported(ref m)) if m.contains("program_config")
        ));
        assert!(matches!(
            mk("00010 0100 0011 0 0 0"),
            Err(AacError::Unsupported(ref m)) if m.contains("声道")
        ));
        assert!(matches!(
            mk("00010 1111 000000000000000101101100 0010 0 0 0"),
            Err(AacError::Unsupported(ref m)) if m.contains("显式采样率")
        ));
    }

    /// ADTS：构造一个最小头并往返。
    #[test]
    fn adts_roundtrip() {
        let mut w = BitWriter::new();
        w.put(0xFFF, 12);
        w.put(0, 1); // mpeg id
        w.put(0, 2); // layer
        w.put(1, 1); // protection_absent
        w.put(1, 2); // profile = LC−1
        w.put(4, 4); // 44100
        w.put(0, 1);
        w.put(2, 3); // stereo
        w.put(0, 2); // original/home
        w.put(0, 2); // copyright_id
        w.put(7, 13); // frame_length（含头）
        w.put(0x7FF, 11);
        w.put(0, 2); // num_frames−1
        let hdr = &w.bytes;
        assert_eq!(hdr.len(), 7);
        assert!(is_adts(hdr));
        let h = parse_adts(hdr).unwrap();
        assert_eq!(h.object_type, 2);
        assert_eq!(h.sampling_index, 4);
        assert_eq!(h.channel_config, 2);
        assert_eq!(h.frame_length, 7);
        assert_eq!(h.header_length, 7);
        // 同步字错误
        assert!(parse_adts(&[0x0F, 0xF1, 0, 0, 0, 0, 0]).is_err());
        // CRC 在场（protection_absent=0）→ header_length 9、帧长 16
        let mut w2 = BitWriter::new();
        w2.put(0xFFF, 12);
        w2.put(0, 1);
        w2.put(0, 2);
        w2.put(0, 1); // protection_absent=0 → 有 CRC
        w2.put(1, 2);
        w2.put(4, 4);
        w2.put(0, 1);
        w2.put(2, 3);
        w2.put(0, 2);
        w2.put(0, 2); // copyright_id
        w2.put(16, 13);
        w2.put(0x7FF, 11);
        w2.put(0, 2);
        let mut v = w2.bytes;
        v.resize(16, 0); // 补 CRC 与载荷
        let h2 = parse_adts(&v).unwrap();
        assert_eq!(h2.header_length, 9);
        assert_eq!(h2.frame_length, 16);
    }

    /// LOAS 探测。
    #[test]
    fn loas_detect() {
        assert!(is_loas(&[0x56, 0xE0]));
        assert!(!is_loas(&[0x56, 0x00]));
        assert!(is_adts(&[0xFF, 0xF1]));
    }

    /// section_data：单长窗两组段（cb=1×3 带 + cb=0×2 带），5 位逃逸。
    #[test]
    fn section_data_groups() {
        let _asc = test_asc();
        // 手工位流：reserved(0) seq(00)=ONLY_LONG shape(0) max_sfb=6(000101?
        // 6 位 = 000110) predictor(0)，section: cb1 逃逸...
        // 直接构造 IcsInfo 更清晰——此处测 parse_section_data 本体
        let ics = IcsInfo {
            window_sequence: ONLY_LONG,
            window_shape: false,
            max_sfb: 6,
            num_window_groups: 1,
            group_len: [1, 0, 0, 0, 0, 0, 0, 0],
            swb_offset: tables::swb_offset_long(4).unwrap(),
            num_swb: tables::num_swb_long(4).unwrap(),
            tns_max_bands: tables::tns_max_bands_long(4).unwrap(),
        };
        // 段1：cb=1 长 3；段2：cb=0 长 2；段3：cb=2 长 1
        let mut w = BitWriter::new();
        w.put(1, 4);
        w.put(3, 5);
        w.put(0, 4);
        w.put(2, 5);
        w.put(2, 4);
        w.put(1, 5);
        let mut r = BitReader::new(&w.bytes);
        let cbs = parse_section_data(&mut r, &ics).unwrap();
        assert_eq!(cbs, vec![1, 1, 1, 0, 0, 2]);
        // sect_cb=12 → InvalidData
        let mut w = BitWriter::new();
        w.put(12, 4);
        w.put(1, 5);
        let mut r = BitReader::new(&w.bytes);
        assert!(parse_section_data(&mut r, &ics).is_err());
        // sect_cb=13（PNS）→ Unsupported
        let mut w = BitWriter::new();
        w.put(13, 4);
        w.put(1, 5);
        let mut r = BitReader::new(&w.bytes);
        assert!(matches!(
            parse_section_data(&mut r, &ics),
            Err(AacError::Unsupported(_))
        ));
    }

    /// scalefactor 链：常规 + 强度混合，delta 从真码表取码字。
    #[test]
    fn scalefactor_chains() {
        let ics = IcsInfo {
            window_sequence: ONLY_LONG,
            window_shape: false,
            max_sfb: 3,
            num_window_groups: 1,
            group_len: [1, 0, 0, 0, 0, 0, 0, 0],
            swb_offset: tables::swb_offset_long(4).unwrap(),
            num_swb: tables::num_swb_long(4).unwrap(),
            tns_max_bands: tables::tns_max_bands_long(4).unwrap(),
        };
        let codebooks = vec![1u8, 15u8, 15u8];
        let global_gain = 100u8;
        // 78018868Ff1a7b2653f7 60Ff08delta 0Ff09= 1 4f4d 0Ff1b7b2653f7 61Ff08+1Ff09= 4 4f4d 1010Ff080xaFf09
        // 7b2653f7 59Ff0822121Ff09= 3 4f4d 110Ff080xcFf09
        let mut w = BitWriter::new();
        w.put(0xa, 4); // 5e3889c4 +1
        w.put(0x0, 1); // 5f3a5ea6 0
        w.put(0xc, 3); // 5f3a5ea6 22121
        let mut r = BitReader::new(&w.bytes);
        let sf = parse_scale_factor_data(&mut r, &codebooks, global_gain, &ics).unwrap();
        assert_eq!(sf, vec![101, 0, -1]);
        // 常规链越界（global_gain=255, +1 → 256）
        let mut w = BitWriter::new();
        w.put(0xa, 4);
        let mut r = BitReader::new(&w.bytes);
        assert!(parse_scale_factor_data(&mut r, &codebooks, 255, &ics).is_err());
    }

    /// pulse/tns 语法小样。
    #[test]
    fn pulse_and_tns_syntax() {
        let ics = IcsInfo {
            window_sequence: ONLY_LONG,
            window_shape: false,
            max_sfb: 4,
            num_window_groups: 1,
            group_len: [1, 0, 0, 0, 0, 0, 0, 0],
            swb_offset: tables::swb_offset_long(4).unwrap(),
            num_swb: tables::num_swb_long(4).unwrap(),
            tns_max_bands: tables::tns_max_bands_long(4).unwrap(),
        };
        // pulse：num=2（u2=1），swb=1，off=3，amp=5；pos1=+7, amp=0
        let mut w = BitWriter::new();
        w.put(1, 2);
        w.put(1, 6);
        w.put(3, 5);
        w.put(5, 4);
        w.put(7, 5);
        w.put(0, 4);
        let mut r = BitReader::new(&w.bytes);
        let p = parse_pulse(&mut r, &ics).unwrap();
        assert_eq!(p.pos, vec![ics.swb_offset[1] + 3, ics.swb_offset[1] + 10]);
        assert_eq!(p.amp, vec![5, 0]);
        // tns：长窗 n_filt=1 coef_res=1 length=10 order=2 dir=0 comp=1
        // coef_len=3（map_1_4 8 项？）——cc=1,cr=1 → sel=3（map_1_4，len 8）
        // coef_len = 1+3−1 = 3 ✓
        let mut w = BitWriter::new();
        w.put(1, 2); // n_filt=1
        w.put(1, 1); // coef_res=1
        w.put(10, 6); // length
        w.put(2, 5); // order=2
        w.put(0, 1); // direction
        w.put(1, 1); // coef_compress=1
        w.put(0b000, 3);
        w.put(0b111, 3);
        let mut r = BitReader::new(&w.bytes);
        let t = parse_tns(&mut r, &ics).unwrap();
        assert_eq!(t.filters.len(), 1);
        let f = &t.filters[0][0];
        assert_eq!((f.length, f.order, f.direction), (10, 2, false));
        // cc=1,cr=1 → sel=3（TNS_MAP_1_4）：索引 0 → 0.0、7 → 0.18374951
        assert_eq!(f.coefs, vec![0.0, 0.183_749_51_f32]);
        // 阶数越界：order=13 > 12
        let mut w = BitWriter::new();
        w.put(1, 2);
        w.put(0, 1);
        w.put(0, 6);
        w.put(13, 5);
        let mut r = BitReader::new(&w.bytes);
        assert!(parse_tns(&mut r, &ics).is_err());
    }

    fn test_asc() -> AudioSpecificConfig {
        AudioSpecificConfig {
            object_type: 2,
            sampling_index: 4,
            sample_rate: 44_100,
            channel_config: 2,
            frame_length_short: false,
        }
    }

    /// 测试位串工具：空格分隔的 0/1 串 → 字节。
    fn bits_to_bytes(bits: &str) -> Vec<u8> {
        let mut w = BitWriter::new();
        for ch in bits.chars() {
            if ch == '0' || ch == '1' {
                w.put(u32::from(ch == '1'), 1);
            }
        }
        w.bytes
    }

    /// 测试位写入器（MSB-first）。
    struct BitWriter {
        bytes: Vec<u8>,
        pos: usize,
    }

    impl BitWriter {
        fn new() -> BitWriter {
            BitWriter {
                bytes: Vec::new(),
                pos: 0,
            }
        }
        fn put(&mut self, v: u32, n: u32) {
            for i in (0..n).rev() {
                let bit = (v >> i) & 1;
                if self.pos.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                let last = self.bytes.last_mut().expect("刚 push");
                *last |= (bit as u8) << (7 - self.pos % 8);
                self.pos += 1;
            }
        }
    }
}

//! SPS/PPS 解析（H.264 §7.3.2.1 / §7.3.2.2）。
//!
//! 定位：**解码要用的字段面**（profile/level/宽高/poc 类型/位深/裁剪），
//! 不追求全字段覆盖（VUI 内容、scaling list 内容只跳过不存储）。
//! profile 的 Baseline 边界判定以 [`Sps::is_baseline`] 供 M2 软解（④）
//! 与硬解路径（⑨）的准入门槛消费——码流层只报告不拒绝。
//!
//! 输入是完整 NAL（含头字节，AVCC/Annex B 拆出来的原样字节）；
//! 内部先去防竞争（[`crate::unescape_rbsp`]）再进位读取器。

use crate::bitreader::BitReader;
use crate::error::H264Error;
use crate::nal::{nal_header_and_payload, unescape_rbsp};

/// profile 档位（ffprobe `profile` 字段的同口径归并）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq, Hash`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Profile {
    /// profile_idc = 66（Baseline 家族；Constrained Baseline 由
    /// `constraint_set(1)` 区分，解码门槛上同属 Baseline 边界内）。
    Baseline,
    /// profile_idc = 77（Main）。
    Main,
    /// profile_idc = 100（High）。
    High,
    /// 其他 profile_idc（Extended 88、High10 110……）。
    Other(u8),
}

impl Profile {
    /// profile_idc → 档位。
    pub fn from_idc(idc: u8) -> Profile {
        match idc {
            66 => Profile::Baseline,
            77 => Profile::Main,
            100 => Profile::High,
            other => Profile::Other(other),
        }
    }
}

/// profile_idc 属于需读 chroma/位深/scaling 字段的高档位集
/// （H.264 表 7-3：High 及以上档位）。
fn is_high_profile(idc: u8) -> bool {
    matches!(
        idc,
        44 | 83 | 86 | 100 | 110 | 118 | 122 | 128 | 134 | 135 | 138 | 139 | 244
    )
}

// —— 带 field 名的读取包装（错误可定位字段）——

fn u1f(r: &mut BitReader<'_>, field: &str) -> Result<bool, H264Error> {
    r.u1()
        .map_err(|e| H264Error::InvalidData(format!("{field}: {e}")))
}

fn uef(r: &mut BitReader<'_>, field: &str) -> Result<u64, H264Error> {
    r.ue()
        .map_err(|e| H264Error::InvalidData(format!("{field}: {e}")))
}

fn sef(r: &mut BitReader<'_>, field: &str) -> Result<i64, H264Error> {
    r.se()
        .map_err(|e| H264Error::InvalidData(format!("{field}: {e}")))
}

fn narrow(v: u64, max: u64, field: &str) -> Result<u64, H264Error> {
    if v > max {
        return Err(H264Error::InvalidData(format!(
            "{field} = {v} exceeds {max}"
        )));
    }
    Ok(v)
}

fn narrow_i32(v: i64, field: &str) -> Result<i32, H264Error> {
    i32::try_from(v).map_err(|_| H264Error::InvalidData(format!("{field} = {v} exceeds i32")))
}

/// 4x4 zigzag 扫描（spec 表 7-5；与 macroblock::ZIGZAG 同值，独立持有
/// 避免 crate 内循环依赖）。
const ZIGZAG4: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];

/// 8x8 zigzag 扫描（spec 表 7-6 = FFmpeg ff_zigzag_direct 逐字）。
const ZIGZAG8: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27,
    20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58,
    59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// jvt 缺省 4x4 量化表（FFmpeg default_scaling4 逐字；仅"使用缺省矩阵"
/// 语义（首 delta 落 0）时启用，未发 scaling list 时用全 16 平表）。
const JVT_SCALING4: [[u8; 16]; 2] = [
    [6, 13, 20, 28, 13, 20, 28, 32, 20, 28, 32, 37, 28, 32, 37, 42],
    [10, 14, 20, 24, 14, 20, 24, 27, 20, 24, 27, 30, 24, 27, 30, 34],
];

/// jvt 缺省 8x8 量化表（FFmpeg default_scaling8 逐字）。
const JVT_SCALING8: [[u8; 64]; 2] = [
    [
        6, 10, 13, 16, 18, 23, 25, 27, 10, 11, 16, 18, 23, 25, 27, 29, 13, 16, 18, 23, 25, 27, 29,
        31, 16, 18, 23, 25, 27, 29, 31, 33, 18, 23, 25, 27, 29, 31, 33, 36, 23, 25, 27, 29, 31,
        33, 36, 38, 25, 27, 29, 31, 33, 36, 38, 40, 27, 29, 31, 33, 36, 38, 40, 42,
    ],
    [
        9, 13, 15, 17, 19, 21, 22, 24, 13, 13, 17, 19, 21, 22, 24, 25, 15, 17, 19, 21, 22, 24, 25,
        27, 17, 19, 21, 22, 24, 25, 27, 28, 19, 21, 22, 24, 25, 27, 28, 30, 21, 22, 24, 25, 27,
        28, 30, 32, 22, 24, 25, 27, 28, 30, 32, 33, 24, 25, 27, 28, 30, 32, 33, 35,
    ],
];

/// 全 16 平表（未发任何 scaling list 时的有效量化矩阵）。
const FLAT4: [u8; 16] = [16; 16];

/// 解析一条 scaling list（FFmpeg decode_scaling_list 逐字：
/// present=0 → 整条取 `fallback`；首 delta 落 0 → 整条取 `jvt`；
/// 否则 delta 链推进，nextScale 归 0 后沿用 lastScale）。
fn parse_scaling_list(
    r: &mut BitReader<'_>,
    size: usize,
    jvt: &[u8],
    fallback: &[u8],
) -> Result<Vec<u8>, H264Error> {
    let scan: &[usize] = if size == 16 { &ZIGZAG4 } else { &ZIGZAG8 };
    if !u1f(r, "scaling_list_present_flag")? {
        return Ok(fallback.to_vec());
    }
    let mut out = vec![0u8; size];
    let mut last: i64 = 8;
    let mut next: i64 = 8;
    for i in 0..size {
        if next != 0 {
            let v = sef(r, "delta_scale")?;
            if !(-128..=127).contains(&v) {
                return Err(H264Error::InvalidData(format!("delta_scale {v} invalid")));
            }
            next = (last + v) & 0xff;
        }
        if i == 0 && next == 0 {
            // 使用缺省矩阵（useDefaultScalingMatrix 语义）
            return Ok(jvt[..size].to_vec());
        }
        let v = if next != 0 { next } else { last };
        out[scan[i]] = v as u8;
        last = v;
    }
    Ok(out)
}

/// SPS/PPS 的 scaling matrix 组解析（FFmpeg decode_scaling_matrices 逐字，
/// 4:2:0 面：6 条 4x4 + 2 条 8x8；回退链：4x4[1]←[0]、[2]←[1]、
/// 4x4[4]←[3]、[5]←[4]、8x8 组←SPS 对应组（PPS 层）或 jvt 缺省）。
/// `sps4`/`sps8`：上层已解析的 SPS 有效矩阵（SPS 层解析传 None）。
fn parse_scaling_matrices(
    r: &mut BitReader<'_>,
    parse_8x8: bool,
    sps4: Option<&[[u8; 16]; 6]>,
    sps8: Option<&[[u8; 64]; 2]>,
) -> Result<([[u8; 16]; 6], Option<[[u8; 64]; 2]>), H264Error> {
    let fb4: [&[u8]; 2] = match sps4 {
        Some(s) => [&s[0], &s[3]],
        None => [&JVT_SCALING4[0], &JVT_SCALING4[1]],
    };
    let mut s4: [Vec<u8>; 6] = [Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    s4[0] = parse_scaling_list(r, 16, &JVT_SCALING4[0], fb4[0])?;
    s4[1] = parse_scaling_list(r, 16, &JVT_SCALING4[0], &s4[0])?;
    s4[2] = parse_scaling_list(r, 16, &JVT_SCALING4[0], &s4[1])?;
    s4[3] = parse_scaling_list(r, 16, &JVT_SCALING4[1], fb4[1])?;
    s4[4] = parse_scaling_list(r, 16, &JVT_SCALING4[1], &s4[3])?;
    s4[5] = parse_scaling_list(r, 16, &JVT_SCALING4[1], &s4[4])?;
    let mut out4: [[u8; 16]; 6] = [[0; 16]; 6];
    for (o, v) in out4.iter_mut().zip(s4.iter()) {
        *o = v
            .clone()
            .try_into()
            .expect("parse_scaling_list 尺寸恒为 16");
    }
    if !parse_8x8 {
        return Ok((out4, None));
    }
    let fb8: [&[u8]; 2] = match sps8 {
        Some(s) => [&s[0], &s[1]],
        None => [&JVT_SCALING8[0], &JVT_SCALING8[1]],
    };
    let l0 = parse_scaling_list(r, 64, &JVT_SCALING8[0], fb8[0])?;
    let l1 = parse_scaling_list(r, 64, &JVT_SCALING8[1], fb8[1])?;
    let out8: [[u8; 64]; 2] = [
        l0.try_into().expect("parse_scaling_list 尺寸恒为 64"),
        l1.try_into().expect("parse_scaling_list 尺寸恒为 64"),
    ];
    Ok((out4, Some(out8)))
}

// —— SPS ——

/// 序列参数集（解码器要用的字段面）。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`（纯值快照，供初始化缓存）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sps {
    /// profile_idc（66=Baseline、77=Main、100=High……）。
    pub profile_idc: u8,
    /// constraint 标志原字节：bit7=set0 … bit2=set5、低 2 位保留。
    pub constraint_flags: u8,
    /// level_idc（= level×10，如 30 = Level 3.0；ffprobe `level` 同口径）。
    pub level_idc: u8,
    /// SPS 编号（0..=31）。
    pub seq_parameter_set_id: u8,
    /// 色度采样（0=单色、1=4:2:0、2=4:2:2、3=4:4:4；高档位缺省 1）。
    pub chroma_format_idc: u8,
    /// 独立色彩平面（chroma_format_idc==3 时可置位；置位后
    /// ChromaArrayType 视作 0）。
    pub separate_colour_plane_flag: bool,
    /// luma 位深 − 8（0..=6）。
    pub bit_depth_luma_minus8: u8,
    /// chroma 位深 − 8（0..=6）。
    pub bit_depth_chroma_minus8: u8,
    /// log2(MaxFrameNum) − 4。
    pub log2_max_frame_num_minus4: u8,
    /// poc 类型（0：显式 LSB；1：Delta；2：隐式 = 帧号）。
    pub pic_order_cnt_type: u8,
    /// 类型 0 的 log2(MaxPicOrderCntLsb) − 4。
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    /// 类型 1 的恒零 Delta 标志。
    pub delta_pic_order_always_zero_flag: bool,
    /// 类型 1 的非参考帧偏移。
    pub offset_for_non_ref_pic: i32,
    /// 类型 1 的顶到底场偏移。
    pub offset_for_top_to_bottom_field: i32,
    /// 类型 1 的逐参考帧偏移表。
    pub offsets_for_ref_frames: Vec<i32>,
    /// 最大参考帧数。
    pub max_num_ref_frames: u16,
    /// 帧号缺口允许标志。
    pub gaps_in_frame_num_value_allowed_flag: bool,
    /// 宏块列数（pic_width_in_mbs_minus1 + 1 的正值）。
    pub pic_width_in_mbs: u16,
    /// 宏块行组数（pic_height_in_map_units_minus1 + 1 的正值）。
    pub pic_height_in_map_units: u16,
    /// 仅帧编码（1 = 无场宏块；场流置 0，高度×2）。
    pub frame_mbs_only_flag: bool,
    /// 帧/场自适应（frame_mbs_only=0 时有效）。
    pub mb_adaptive_frame_field_flag: bool,
    /// direct 8×8 推断约束。
    pub direct_8x8_inference_flag: bool,
    /// 左/右/顶/底裁剪（chroma 单位；显示尺寸换算见 [`Sps::width`]）。
    pub frame_crop_left: u16,
    /// 同上，右。
    pub frame_crop_right: u16,
    /// 同上，顶。
    pub frame_crop_top: u16,
    /// 同上，底。
    pub frame_crop_bottom: u16,
    /// VUI 存在标志（内容不解析，供后续按需扩展）。
    pub vui_parameters_present_flag: bool,
    /// 高档位 scaling matrix（6 条 4x4 有效值；缺省 = None = 平表）。
    /// `Some` 时 PPS 缺席项回退到此。
    pub scaling4: Option<[[u8; 16]; 6]>,
    /// 高档位 scaling matrix 8x8（IntraY / InterY；chroma 4:2:0 只有这两条）。
    pub scaling8: Option<[[u8; 64]; 2]>,
}

impl Sps {
    /// 解析完整 SPS NAL（含头字节）。类型非 7 → `InvalidData`；
    /// 字段越界/位耗尽 → `InvalidData`（带字段名）。
    /// 裁剪量超出尺寸在解析末尾校验，`width()`/`height()` 因此免失败。
    pub fn parse(nal: &[u8]) -> Result<Self, H264Error> {
        let (header, payload) =
            nal_header_and_payload(nal).map_err(|e| H264Error::InvalidData(format!("sps: {e}")))?;
        if header.nal_unit_type != 7 {
            return Err(H264Error::InvalidData(format!(
                "sps: nal_unit_type {} != 7",
                header.nal_unit_type
            )));
        }
        let rbsp = unescape_rbsp(payload);
        let mut r = BitReader::new(&rbsp);

        let profile_idc = r
            .u(8)
            .map_err(|e| H264Error::InvalidData(format!("sps.profile_idc: {e}")))?
            as u8;
        let constraint_flags = r
            .u(8)
            .map_err(|e| H264Error::InvalidData(format!("sps.constraint_flags: {e}")))?
            as u8;
        let level_idc = r
            .u(8)
            .map_err(|e| H264Error::InvalidData(format!("sps.level_idc: {e}")))?
            as u8;
        let seq_parameter_set_id = narrow(
            uef(&mut r, "sps.seq_parameter_set_id")?,
            31,
            "sps.seq_parameter_set_id",
        )? as u8;

        let mut chroma_format_idc = 1u8;
        let mut separate_colour_plane_flag = false;
        let mut bit_depth_luma_minus8 = 0u8;
        let mut bit_depth_chroma_minus8 = 0u8;
        let mut scaling4: Option<[[u8; 16]; 6]> = None;
        let mut scaling8: Option<[[u8; 64]; 2]> = None;
        if is_high_profile(profile_idc) {
            chroma_format_idc = narrow(
                uef(&mut r, "sps.chroma_format_idc")?,
                3,
                "sps.chroma_format_idc",
            )? as u8;
            if chroma_format_idc == 3 {
                separate_colour_plane_flag = u1f(&mut r, "sps.separate_colour_plane_flag")?;
            }
            bit_depth_luma_minus8 = narrow(
                uef(&mut r, "sps.bit_depth_luma_minus8")?,
                6,
                "sps.bit_depth_luma_minus8",
            )? as u8;
            bit_depth_chroma_minus8 = narrow(
                uef(&mut r, "sps.bit_depth_chroma_minus8")?,
                6,
                "sps.bit_depth_chroma_minus8",
            )? as u8;
            let _ = u1f(&mut r, "sps.qpprime_y_zero_transform_bypass_flag")?;
            // 布局（spec 7.3.2.1.1.1，FFmpeg 同）：chroma != 3 →
            // 6 条 4x4 + 2 条 8x8；chroma == 3 → 6 条 4x4 + 6 条 8x8
            //（本解码器 4:2:0 恒走前一档；== 3 的额外 4 条如实收紧）
            if u1f(&mut r, "sps.seq_scaling_matrix_present_flag")? {
                let (m4, m8) =
                    parse_scaling_matrices(&mut r, chroma_format_idc != 3, None, None)?;
                scaling4 = Some(m4);
                scaling8 = m8;
            }
        }

        let log2_max_frame_num_minus4 = narrow(
            uef(&mut r, "sps.log2_max_frame_num_minus4")?,
            12,
            "sps.log2_max_frame_num_minus4",
        )? as u8;

        let pic_order_cnt_type = narrow(
            uef(&mut r, "sps.pic_order_cnt_type")?,
            2,
            "sps.pic_order_cnt_type",
        )? as u8;
        let mut log2_max_pic_order_cnt_lsb_minus4 = 0u8;
        let mut delta_pic_order_always_zero_flag = false;
        let mut offset_for_non_ref_pic = 0i32;
        let mut offset_for_top_to_bottom_field = 0i32;
        let mut offsets_for_ref_frames = Vec::new();
        match pic_order_cnt_type {
            0 => {
                log2_max_pic_order_cnt_lsb_minus4 = narrow(
                    uef(&mut r, "sps.log2_max_pic_order_cnt_lsb_minus4")?,
                    12,
                    "sps.log2_max_pic_order_cnt_lsb_minus4",
                )? as u8;
            }
            1 => {
                delta_pic_order_always_zero_flag =
                    u1f(&mut r, "sps.delta_pic_order_always_zero_flag")?;
                offset_for_non_ref_pic = narrow_i32(
                    sef(&mut r, "sps.offset_for_non_ref_pic")?,
                    "sps.offset_for_non_ref_pic",
                )?;
                offset_for_top_to_bottom_field = narrow_i32(
                    sef(&mut r, "sps.offset_for_top_to_bottom_field")?,
                    "sps.offset_for_top_to_bottom_field",
                )?;
                let cycles = narrow(
                    uef(&mut r, "sps.num_ref_frames_in_pic_order_cnt_cycle")?,
                    255,
                    "sps.num_ref_frames_in_pic_order_cnt_cycle",
                )? as usize;
                offsets_for_ref_frames = Vec::with_capacity(cycles);
                for _ in 0..cycles {
                    offsets_for_ref_frames.push(narrow_i32(
                        sef(&mut r, "sps.offset_for_ref_frame")?,
                        "sps.offset_for_ref_frame",
                    )?);
                }
            }
            _ => {}
        }

        let max_num_ref_frames = narrow(
            uef(&mut r, "sps.max_num_ref_frames")?,
            65535,
            "sps.max_num_ref_frames",
        )? as u16;
        let gaps_in_frame_num_value_allowed_flag =
            u1f(&mut r, "sps.gaps_in_frame_num_value_allowed_flag")?;
        let pic_width_in_mbs = (narrow(
            uef(&mut r, "sps.pic_width_in_mbs_minus1")?,
            65534,
            "sps.pic_width_in_mbs_minus1",
        )? + 1) as u16;
        let pic_height_in_map_units = (narrow(
            uef(&mut r, "sps.pic_height_in_map_units_minus1")?,
            65534,
            "sps.pic_height_in_map_units_minus1",
        )? + 1) as u16;
        let frame_mbs_only_flag = u1f(&mut r, "sps.frame_mbs_only_flag")?;
        let mut mb_adaptive_frame_field_flag = false;
        if !frame_mbs_only_flag {
            mb_adaptive_frame_field_flag = u1f(&mut r, "sps.mb_adaptive_frame_field_flag")?;
        }
        let direct_8x8_inference_flag = u1f(&mut r, "sps.direct_8x8_inference_flag")?;
        let mut frame_crop = [0u16; 4];
        if u1f(&mut r, "sps.frame_cropping_flag")? {
            const CROP_FIELDS: [&str; 4] = [
                "sps.frame_crop_left",
                "sps.frame_crop_right",
                "sps.frame_crop_top",
                "sps.frame_crop_bottom",
            ];
            for (slot, field) in CROP_FIELDS.iter().enumerate() {
                frame_crop[slot] = narrow(uef(&mut r, field)?, 65534, "sps.frame_crop")? as u16;
            }
        }
        let vui_parameters_present_flag = u1f(&mut r, "sps.vui_parameters_present_flag")?;

        let sps = Sps {
            profile_idc,
            constraint_flags,
            level_idc,
            seq_parameter_set_id,
            chroma_format_idc,
            separate_colour_plane_flag,
            bit_depth_luma_minus8,
            bit_depth_chroma_minus8,
            log2_max_frame_num_minus4,
            pic_order_cnt_type,
            log2_max_pic_order_cnt_lsb_minus4,
            delta_pic_order_always_zero_flag,
            offset_for_non_ref_pic,
            offset_for_top_to_bottom_field,
            offsets_for_ref_frames,
            max_num_ref_frames,
            gaps_in_frame_num_value_allowed_flag,
            pic_width_in_mbs,
            pic_height_in_map_units,
            frame_mbs_only_flag,
            mb_adaptive_frame_field_flag,
            direct_8x8_inference_flag,
            frame_crop_left: frame_crop[0],
            frame_crop_right: frame_crop[1],
            frame_crop_top: frame_crop[2],
            frame_crop_bottom: frame_crop[3],
            vui_parameters_present_flag,
            scaling4,
            scaling8,
        };
        // 裁剪量越界在解析末尾校验——此后 width()/height() 免失败
        let crop_x = u32::from(sps.frame_crop_left) + u32::from(sps.frame_crop_right);
        let crop_y = u32::from(sps.frame_crop_top) + u32::from(sps.frame_crop_bottom);
        let (unit_x, unit_y) = sps.crop_units();
        if crop_x * unit_x >= u32::from(sps.pic_width_in_mbs) * 16
            || crop_y * unit_y >= sps.frame_height_in_pixels()
        {
            return Err(H264Error::InvalidData(format!(
                "sps: cropping ({crop_x}x{crop_y} chroma units) exceeds picture ({}x{})",
                sps.pic_width_in_mbs, sps.pic_height_in_map_units
            )));
        }
        Ok(sps)
    }

    /// ChromaArrayType（§7-11 的重命名规则：separate_colour_plane 置位则 0）。
    fn chroma_array_type(&self) -> u8 {
        if self.separate_colour_plane_flag {
            0
        } else {
            self.chroma_format_idc
        }
    }

    /// 裁换单位 (CropUnitX, CropUnitY)（§7-12，chroma 采样→像素换算）。
    fn crop_units(&self) -> (u32, u32) {
        let two_minus_frame_only = 2 - u32::from(self.frame_mbs_only_flag);
        if self.chroma_array_type() == 0 {
            (1, two_minus_frame_only)
        } else {
            let (sub_w, sub_h) = match self.chroma_array_type() {
                1 => (2u32, 2u32),
                2 => (2, 1),
                _ => (1, 1),
            };
            (sub_w, sub_h * two_minus_frame_only)
        }
    }

    /// 未裁剪帧高（像素）：(2 − frame_mbs_only) × 宏块行 × 16。
    fn frame_height_in_pixels(&self) -> u32 {
        (2 - u32::from(self.frame_mbs_only_flag)) * u32::from(self.pic_height_in_map_units) * 16
    }

    /// 显示宽度（像素，含裁剪换算；与 ffprobe `width` 同口径）。
    pub fn width(&self) -> u32 {
        let (unit_x, _) = self.crop_units();
        u32::from(self.pic_width_in_mbs) * 16
            - (u32::from(self.frame_crop_left) + u32::from(self.frame_crop_right)) * unit_x
    }

    /// 显示高度（像素，含裁剪换算；与 ffprobe `height` 同口径）。
    pub fn height(&self) -> u32 {
        let (_, unit_y) = self.crop_units();
        self.frame_height_in_pixels()
            - (u32::from(self.frame_crop_top) + u32::from(self.frame_crop_bottom)) * unit_y
    }

    /// profile 档位（见 [`Profile`]）。
    pub fn profile(&self) -> Profile {
        Profile::from_idc(self.profile_idc)
    }

    /// Baseline 边界判定（④/⑨ 的 profile 门槛消费点）：
    /// profile_idc == 66（Baseline 家族，含 Constrained Baseline 子集）。
    pub fn is_baseline(&self) -> bool {
        self.profile_idc == 66
    }

    /// constraint_setN_flag（N ∈ 0..=5，越界 N 恒 false）。
    /// N=1 置位且 profile_idc=66 即 Constrained Baseline（ffprobe 同口径）。
    pub fn constraint_set(&self, n: u8) -> bool {
        if n > 5 {
            false
        } else {
            self.constraint_flags & (1 << (7 - n)) != 0
        }
    }
}

// —— PPS ——

/// 图像参数集（解码器要用的字段面；FMO 多片组被拒，
/// transform_8x8 扩展段不解析）。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pps {
    /// PPS 编号（0..=255）。
    pub pic_parameter_set_id: u8,
    /// 引用的 SPS 编号（0..=31）。
    pub seq_parameter_set_id: u8,
    /// 熵编码模式（false=CAVLC；Baseline 必须 CAVLC）。
    pub entropy_coding_mode_flag: bool,
    /// 底场 poc 在片头出现（bottom_field_pic_order_in_frame_present）。
    pub bottom_field_pic_order_in_frame_present_flag: bool,
    /// l0 缺省参考帧数 − 1。
    pub num_ref_idx_l0_default_active_minus1: u8,
    /// l1 缺省参考帧数 − 1。
    pub num_ref_idx_l1_default_active_minus1: u8,
    /// 加权预测（P 片）。
    pub weighted_pred_flag: bool,
    /// 加权双预测（B 片，u(2)）。
    pub weighted_bipred_idc: u8,
    /// 初始 QP − 26。
    pub pic_init_qp_minus26: i32,
    /// 初始 QS − 26。
    pub pic_init_qs_minus26: i32,
    /// chroma QP 偏移。
    pub chroma_qp_index_offset: i32,
    /// 去块滤波控制字段在片头出现。
    pub deblocking_filter_control_present_flag: bool,
    /// 受限帧内预测。
    pub constrained_intra_pred_flag: bool,
    /// 冗余片计数出现。
    pub redundant_pic_cnt_present_flag: bool,
    /// transform_8x8 允许（High 档；PPS 扩展段）。
    pub transform_8x8_mode_flag: bool,
    /// 第二 chroma QP 偏移（Cr；High 档；未出现 = 第一偏移）。
    pub second_chroma_qp_index_offset: i32,
    /// PPS scaling matrix 4x4 有效值（含 SPS/jvt 回退解析；None = 平表）。
    pub scaling4: Option<[[u8; 16]; 6]>,
    /// PPS scaling matrix 8x8 有效值（[IntraY, InterY]；None = 平表）。
    pub scaling8: Option<[[u8; 64]; 2]>,
}

/// 8x8 反量化基表（FFmpeg ff_h264_dequant8_coeff_init 逐字）。
const DEQUANT8_V: [[u8; 6]; 6] = [
    [20, 18, 32, 19, 25, 24],
    [22, 19, 35, 21, 28, 26],
    [26, 23, 42, 24, 33, 31],
    [28, 25, 45, 26, 35, 33],
    [32, 28, 51, 30, 40, 38],
    [36, 32, 58, 34, 46, 43],
];

/// 8x8 反量化类别扫描（FFmpeg ff_h264_dequant8_coeff_init_scan 逐字）。
const DEQUANT8_SCAN: [u8; 16] = [0, 3, 4, 3, 3, 1, 5, 1, 4, 5, 2, 5, 3, 1, 5, 1];

impl Pps {
    /// 解析完整 PPS NAL（含头字节）。类型非 8 → `InvalidData`；
    /// FMO 多片组（num_slice_groups_minus1 > 0）→ `Unsupported`
    /// （FFmpeg 同样拒绝；Baseline 码流不受影响）。
    /// transform_8x8 扩展段（High 档 PPS）随本里程碑解析；
    /// scaling 回退不带 SPS 上下文（按 jvt 缺省链）。
    pub fn parse(nal: &[u8]) -> Result<Self, H264Error> {
        Self::parse_with_sps(nal, None)
    }

    /// 带 SPS 上下文解析（PPS 扩展段的 scaling 回退需要 SPS 有效矩阵；
    /// `sps` 传 None 时 scaling 按无 SPS 回退链解析）。
    pub(crate) fn parse_with_sps(nal: &[u8], sps: Option<&Sps>) -> Result<Self, H264Error> {
        let (header, payload) =
            nal_header_and_payload(nal).map_err(|e| H264Error::InvalidData(format!("pps: {e}")))?;
        if header.nal_unit_type != 8 {
            return Err(H264Error::InvalidData(format!(
                "pps: nal_unit_type {} != 8",
                header.nal_unit_type
            )));
        }
        let rbsp = unescape_rbsp(payload);
        let mut r = BitReader::new(&rbsp);
        let pic_parameter_set_id = narrow(
            uef(&mut r, "pps.pic_parameter_set_id")?,
            255,
            "pps.pic_parameter_set_id",
        )? as u8;
        let seq_parameter_set_id = narrow(
            uef(&mut r, "pps.seq_parameter_set_id")?,
            31,
            "pps.seq_parameter_set_id",
        )? as u8;
        let entropy_coding_mode_flag = u1f(&mut r, "pps.entropy_coding_mode_flag")?;
        let bottom_field_pic_order_in_frame_present_flag =
            u1f(&mut r, "pps.bottom_field_pic_order_in_frame_present_flag")?;
        let num_slice_groups_minus1 = narrow(
            uef(&mut r, "pps.num_slice_groups_minus1")?,
            65535,
            "pps.num_slice_groups_minus1",
        )?;
        if num_slice_groups_minus1 > 0 {
            return Err(H264Error::Unsupported(
                "FMO slice groups in PPS (num_slice_groups_minus1 > 0)",
            ));
        }
        let num_ref_idx_l0_default_active_minus1 = narrow(
            uef(&mut r, "pps.num_ref_idx_l0_default_active_minus1")?,
            255,
            "pps.num_ref_idx_l0_default_active_minus1",
        )? as u8;
        let num_ref_idx_l1_default_active_minus1 = narrow(
            uef(&mut r, "pps.num_ref_idx_l1_default_active_minus1")?,
            255,
            "pps.num_ref_idx_l1_default_active_minus1",
        )? as u8;
        let weighted_pred_flag = u1f(&mut r, "pps.weighted_pred_flag")?;
        let weighted_bipred_idc = r
            .u(2)
            .map_err(|e| H264Error::InvalidData(format!("pps.weighted_bipred_idc: {e}")))?
            as u8;
        let pic_init_qp_minus26 = narrow_i32(
            sef(&mut r, "pps.pic_init_qp_minus26")?,
            "pps.pic_init_qp_minus26",
        )?;
        let pic_init_qs_minus26 = narrow_i32(
            sef(&mut r, "pps.pic_init_qs_minus26")?,
            "pps.pic_init_qs_minus26",
        )?;
        let chroma_qp_index_offset = narrow_i32(
            sef(&mut r, "pps.chroma_qp_index_offset")?,
            "pps.chroma_qp_index_offset",
        )?;
        let deblocking_filter_control_present_flag =
            u1f(&mut r, "pps.deblocking_filter_control_present_flag")?;
        let constrained_intra_pred_flag = u1f(&mut r, "pps.constrained_intra_pred_flag")?;
        let redundant_pic_cnt_present_flag = u1f(&mut r, "pps.redundant_pic_cnt_present_flag")?;

        // ---- 扩展段（FFmpeg h264_ps.c 逐字）----
        // 门槛①：有效数据位（剥 rbsp 尾随位）有余量；
        // 门槛②：more_rbsp_data_in_pps——Baseline/Main/Extended 且
        // constraint_set0..2 任一置位（Constrained 族）则无扩展段。
        let data_bits = crate::slice::rbsp_data_bits(&rbsp);
        let more_rbsp_data = match sps {
            Some(s)
                if matches!(s.profile_idc, 66 | 77 | 88)
                    && (s.constraint_set(0) || s.constraint_set(1) || s.constraint_set(2)) =>
            {
                false
            }
            _ => true,
        };
        let mut transform_8x8_mode_flag = false;
        let mut second_chroma_qp_index_offset = chroma_qp_index_offset;
        let mut scaling4: Option<[[u8; 16]; 6]> = None;
        let mut scaling8: Option<[[u8; 64]; 2]> = None;
        if data_bits > r.bit_position() && more_rbsp_data {
            transform_8x8_mode_flag = u1f(&mut r, "pps.transform_8x8_mode_flag")?;
            if u1f(&mut r, "pps.pic_scaling_matrix_present_flag")? {
                let (m4, m8) = parse_scaling_matrices(
                    &mut r,
                    transform_8x8_mode_flag,
                    sps.and_then(|s| s.scaling4.as_ref()),
                    sps.and_then(|s| s.scaling8.as_ref()),
                )?;
                scaling4 = Some(m4);
                if transform_8x8_mode_flag {
                    scaling8 = m8;
                }
            } else if let Some(s) = sps {
                // PPS 未发 scaling → 沿用 SPS 有效矩阵
                scaling4 = s.scaling4.clone();
                if transform_8x8_mode_flag {
                    scaling8 = s.scaling8.clone();
                }
            }
            second_chroma_qp_index_offset = narrow_i32(
                sef(&mut r, "pps.second_chroma_qp_index_offset")?,
                "pps.second_chroma_qp_index_offset",
            )?;
        }
        if !(-12..=12).contains(&chroma_qp_index_offset)
            || !(-12..=12).contains(&second_chroma_qp_index_offset)
        {
            return Err(H264Error::InvalidData(
                "chroma_qp_index_offset out of range (-12..=12)".into(),
            ));
        }
        Ok(Pps {
            pic_parameter_set_id,
            seq_parameter_set_id,
            entropy_coding_mode_flag,
            bottom_field_pic_order_in_frame_present_flag,
            num_ref_idx_l0_default_active_minus1,
            num_ref_idx_l1_default_active_minus1,
            weighted_pred_flag,
            weighted_bipred_idc,
            pic_init_qp_minus26,
            pic_init_qs_minus26,
            chroma_qp_index_offset,
            deblocking_filter_control_present_flag,
            constrained_intra_pred_flag,
            redundant_pic_cnt_present_flag,
            transform_8x8_mode_flag,
            second_chroma_qp_index_offset,
            scaling4,
            scaling8,
        })
    }

    /// 4x4 反量化乘子表（FFmpeg init_dequant4_coeff_table 逐字）：
    /// `qmul[转置槽] = V[qp%6][class(x)] · scaling[x] << (qp/6+2)`。
    /// `list`：0=IntraY 1=IntraCr 2=IntraCb 3=InterY 4=InterCr 5=InterCb。
    pub(crate) fn dequant4_coeff(&self, qp: u8, list: usize) -> [u32; 16] {
        let scaling: [u8; 16] = self
            .scaling4
            .as_ref()
            .map(|s| s[list])
            .unwrap_or([16; 16]);
        let mut qmul = [0u32; 16];
        for x in 0..16usize {
            let class = (x & 1) + ((x >> 2) & 1);
            let v = u32::from(crate::macroblock::DEQUANT_V[usize::from(qp % 6)][class])
                * u32::from(scaling[x]);
            // 槽位转置（FFmpeg (x>>2)|((x<<2)&0xF)）——平坦/对称矩阵下
            // 与现行光栅布局等价，带自定义矩阵时按 FFmpeg 转置口径
            qmul[(x >> 2) | ((x << 2) & 0xF)] = v << (u32::from(qp / 6) + 2);
        }
        qmul
    }

    /// 8x8 反量化乘子表（FFmpeg init_dequant8_coeff_table 逐字）：
    /// `qmul8[转置槽] = V8[qp%6][scanclass(x)] · scaling8[x] << (qp/6)`。
    /// `list`：0=IntraY 1=InterY。
    pub(crate) fn dequant8_coeff(&self, qp: u8, list: usize) -> [u32; 64] {
        let scaling: [u8; 64] = self
            .scaling8
            .as_ref()
            .map(|s| s[list])
            .unwrap_or([16; 64]);
        let mut qmul = [0u32; 64];
        for x in 0..64usize {
            let cls = usize::from(DEQUANT8_SCAN[((x >> 1) & 12) | (x & 3)]);
            let v = u32::from(DEQUANT8_V[usize::from(qp % 6)][cls]) * u32::from(scaling[x]);
            qmul[(x >> 3) | ((x & 7) << 3)] = v << u32::from(qp / 6);
        }
        qmul
    }

    /// chroma DC 反量化乘子（4x4 平表 DC 类，chroma DC 无独立矩阵）。
    pub(crate) fn chroma_dc_qmul(&self, qpc: u8) -> u32 {
        u32::from(crate::macroblock::DEQUANT_V[usize::from(qpc % 6)][0])
            << (u32::from(qpc / 6) + 6)
    }

    /// chroma QP（按平面：0=Cb 用第一偏移、1=Cr 用第二偏移；
    /// FFmpeg get_chroma_qp 的 4:2:0 形态：clip 后查 CHROMA_QP 表——
    /// 表在 macroblock.rs，经此只提供偏移选择）。
    pub(crate) fn chroma_qp_offset(&self, plane: usize) -> i32 {
        if plane == 1 {
            self.second_chroma_qp_index_offset
        } else {
            self.chroma_qp_index_offset
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手工构造的 SPS 黄金字节（推导过程见各用例注释）：
    /// Baseline(constrained)/level 3.0、16×16、poc type 2。
    const SPS_BASELINE_16X16: &[u8] = &[
        0x67, 0x42, 0xC0, 0x1E, // 头 + profile 66 + constraints C0 + level 30
        0xDA,
        0x79, // 语法位：sps_id=0 lmf=0 poc=2 mnr=1 gaps=0 w=0 h=0
              // fmo=1 d8=1 crop=0 vui=0 stop
    ];

    /// Main/level 3.0、poc type 0、32×16 裁剪成 30×14
    /// （4:2:0：CropUnitX=Y=2；left=1、top=1）。
    const SPS_MAIN_CROPPED: &[u8] = &[0x67, 0x4D, 0x00, 0x1E, 0xF9, 0x7A, 0xAA];

    /// High/level 4.0、chroma 4:4:4（chroma_format_idc=3、separate=0、
    /// 位深 8）、16×16、poc type 2（高档位字段链 + scaling 缺省路径）。
    const SPS_HIGH_444: &[u8] = &[0x67, 0x64, 0x00, 0x28, 0x91, 0x96, 0xBC, 0x80];

    /// Baseline/level 1.0、poc type 1（含 Delta 偏移链）：
    /// cycles=[-1,1]、offset_top_to_bottom=1、max_num_ref=3、16×16。
    const SPS_POC1: &[u8] = &[0x67, 0x42, 0xC0, 0x0A, 0xD2, 0x9B, 0x44, 0x79];

    /// 手工构造的 PPS 黄金字节：CAVLC、单片组、QP 偏移 0、
    /// deblocking 控制出现、constrained/redundant 关。
    const PPS_GOLDEN: &[u8] = &[0x68, 0xCE, 0x3C, 0x80];

    #[test]
    fn sps_golden_baseline_16x16() {
        let sps = Sps::parse(SPS_BASELINE_16X16).unwrap();
        assert_eq!(sps.profile_idc, 66);
        assert_eq!(sps.constraint_flags, 0xC0);
        assert_eq!(sps.level_idc, 30);
        assert_eq!(sps.seq_parameter_set_id, 0);
        // 非 High profile：chroma 走缺省 4:2:0、位深 8
        assert_eq!(sps.chroma_format_idc, 1);
        assert!(!sps.separate_colour_plane_flag);
        assert_eq!(sps.bit_depth_luma_minus8, 0);
        assert_eq!(sps.bit_depth_chroma_minus8, 0);
        assert_eq!(sps.log2_max_frame_num_minus4, 0);
        assert_eq!(sps.pic_order_cnt_type, 2);
        assert_eq!(sps.max_num_ref_frames, 1);
        assert!(!sps.gaps_in_frame_num_value_allowed_flag);
        assert_eq!(sps.pic_width_in_mbs, 1);
        assert_eq!(sps.pic_height_in_map_units, 1);
        assert!(sps.frame_mbs_only_flag);
        assert!(sps.direct_8x8_inference_flag);
        assert_eq!((sps.width(), sps.height()), (16, 16));
        assert!(!sps.vui_parameters_present_flag);
        // Baseline 边界判定与档位/约束访问器
        assert!(sps.is_baseline());
        assert_eq!(sps.profile(), Profile::Baseline);
        assert!(sps.constraint_set(0));
        assert!(sps.constraint_set(1)); // Constrained Baseline
        assert!(!sps.constraint_set(2));
        assert!(!sps.constraint_set(5));
        assert!(!sps.constraint_set(6)); // 越界恒 false
    }

    #[test]
    fn sps_golden_main_cropped_30x14() {
        let sps = Sps::parse(SPS_MAIN_CROPPED).unwrap();
        assert_eq!(sps.profile_idc, 77);
        assert_eq!(sps.level_idc, 30);
        assert_eq!(sps.pic_order_cnt_type, 0);
        assert_eq!(sps.log2_max_pic_order_cnt_lsb_minus4, 0);
        assert_eq!(sps.pic_width_in_mbs, 2); // 32 px
        assert_eq!(sps.pic_height_in_map_units, 1); // 16 px
        assert_eq!(sps.frame_crop_left, 1);
        assert_eq!(sps.frame_crop_right, 0);
        assert_eq!(sps.frame_crop_top, 1);
        assert_eq!(sps.frame_crop_bottom, 0);
        // 4:2:0 + frame_mbs_only：CropUnitX=CropUnitY=2 → 32−2 / 16−2
        assert_eq!((sps.width(), sps.height()), (30, 14));
        assert!(!sps.is_baseline());
        assert_eq!(sps.profile(), Profile::Main);
    }

    #[test]
    fn sps_golden_high_444() {
        let sps = Sps::parse(SPS_HIGH_444).unwrap();
        assert_eq!(sps.profile_idc, 100);
        assert_eq!(sps.level_idc, 40);
        assert_eq!(sps.chroma_format_idc, 3);
        assert!(!sps.separate_colour_plane_flag);
        assert_eq!(sps.bit_depth_luma_minus8, 0);
        assert_eq!(sps.bit_depth_chroma_minus8, 0);
        assert_eq!(sps.pic_order_cnt_type, 2);
        // 4:4:4 无裁剪：16×16
        assert_eq!((sps.width(), sps.height()), (16, 16));
        assert_eq!(sps.profile(), Profile::High);
        assert!(!sps.is_baseline());
    }

    #[test]
    fn sps_golden_poc_type1_offsets() {
        let sps = Sps::parse(SPS_POC1).unwrap();
        assert_eq!(sps.pic_order_cnt_type, 1);
        assert!(!sps.delta_pic_order_always_zero_flag);
        assert_eq!(sps.offset_for_non_ref_pic, 0);
        assert_eq!(sps.offset_for_top_to_bottom_field, 1);
        assert_eq!(sps.offsets_for_ref_frames, vec![-1, 1]);
        assert_eq!(sps.max_num_ref_frames, 3);
        assert_eq!((sps.width(), sps.height()), (16, 16));
    }

    /// 防竞争字节在 Sps::parse 内部被去除：黄金 SPS 尾部追加线上转义
    /// 序列 `00 00 03 00`（RBSP `00 00 00`），解析结果不受影响——
    /// 去转义覆盖整个 NAL 载荷，且解析止于 vui 标志，不读尾随字节。
    /// （说明：小字段值的有效 SPS 语法位流本身造不出 `00 00 0x` 字节
    /// 模式——那需要 16+ 连续零位，ue 码长上限 32 位内的大值会被字段
    /// 范围校验先拒。真实码流的防竞争出现在 slice 数据；故本用例以
    /// 尾随字节验证组合链路，纯函数级的去除语义见 nal.rs 用例。）
    #[test]
    fn sps_tolerates_trailing_emulation_prevention() {
        let mut nal = SPS_BASELINE_16X16.to_vec();
        nal.extend_from_slice(&[0x00, 0x00, 0x03, 0x00]);
        let sps = Sps::parse(&nal).unwrap();
        assert_eq!(sps.profile_idc, 66);
        assert_eq!((sps.width(), sps.height()), (16, 16));
        assert_eq!(sps.pic_order_cnt_type, 2);
    }

    #[test]
    fn sps_rejects_wrong_nal_type_and_garbage() {
        // PPS NAL 不是 SPS
        assert!(matches!(
            Sps::parse(PPS_GOLDEN),
            Err(H264Error::InvalidData(msg)) if msg.contains("!= 7")
        ));
        // 位耗尽：只有头 4 字节（profile/constraints/level 之后无位）
        assert!(matches!(
            Sps::parse(&[0x67, 0x42, 0xC0, 0x1E]),
            Err(H264Error::InvalidData(msg)) if msg.contains("seq_parameter_set_id")
        ));
        // 裁剪量超出尺寸被拒：1 宏块宽（16px）+ crop_left=8
        // （4:2:0：CropUnitX=2 → 裁 16px ≥ 16px 全宽）。
        // 位串推导：sps0=1 lmf0=1 poc2=011 mnr0=1 gaps=0 w0=1 h0=1
        // fmo=1 d8=1 crop=1 | crop_left=ue(8)=0001001 | R=T=B=ue(0)=1,1,1
        // vui=0 stop=1 → 11011101 11110001 00111101
        let bytes = [0x67u8, 0x42, 0xC0, 0x1E, 0xDD, 0xF1, 0x3D];
        match Sps::parse(&bytes) {
            Err(H264Error::InvalidData(msg)) => assert!(msg.contains("cropping"), "{msg}"),
            other => panic!("应拒裁剪超界，实为 {other:?}"),
        }
    }

    #[test]
    fn sps_rejects_out_of_range_poc_type() {
        // poc_type=3：ue(3)="00100" → 越界（0..=2）
        // 位串：1(sps0) 1(lmf0) 00100(poc=3) …
        let bytes = [0x67u8, 0x42, 0xC0, 0x1E, 0b1100_0100, 0x00];
        match Sps::parse(&bytes) {
            Err(H264Error::InvalidData(msg)) => {
                assert!(msg.contains("pic_order_cnt_type"), "{msg}")
            }
            other => panic!("应拒 poc_type=3，实为 {other:?}"),
        }
    }

    #[test]
    fn pps_golden() {
        let pps = Pps::parse(PPS_GOLDEN).unwrap();
        assert_eq!(pps.pic_parameter_set_id, 0);
        assert_eq!(pps.seq_parameter_set_id, 0);
        assert!(!pps.entropy_coding_mode_flag); // CAVLC
        assert!(!pps.bottom_field_pic_order_in_frame_present_flag);
        assert_eq!(pps.num_ref_idx_l0_default_active_minus1, 0);
        assert_eq!(pps.num_ref_idx_l1_default_active_minus1, 0);
        assert!(!pps.weighted_pred_flag);
        assert_eq!(pps.weighted_bipred_idc, 0);
        assert_eq!(pps.pic_init_qp_minus26, 0);
        assert_eq!(pps.pic_init_qs_minus26, 0);
        assert_eq!(pps.chroma_qp_index_offset, 0);
        assert!(pps.deblocking_filter_control_present_flag);
        assert!(!pps.constrained_intra_pred_flag);
        assert!(!pps.redundant_pic_cnt_present_flag);
    }

    #[test]
    fn pps_rejects_wrong_type_and_fmo() {
        // SPS NAL 不是 PPS
        assert!(matches!(
            Pps::parse(SPS_BASELINE_16X16),
            Err(H264Error::InvalidData(msg)) if msg.contains("!= 8")
        ));
        // FMO：num_slice_groups_minus1 = 1 → ue(1)="010"
        // 位串：1(pps0) 1(sps0) 0(entropy) 0(bottom) 010(nsg=1) …
        let bytes = [0x68u8, 0b1100_0010, 0x00];
        match Pps::parse(&bytes) {
            Err(H264Error::Unsupported(msg)) => assert!(msg.contains("FMO"), "{msg}"),
            other => panic!("应拒 FMO，实为 {other:?}"),
        }
    }

    #[test]
    fn sps_pps_types_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Sps>();
        assert_send_sync::<Pps>();
        assert_send_sync::<Profile>();
    }
}

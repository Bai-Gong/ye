# 临时清理补丁（跑完即删）：移除调试钩子与死字段，修复全部告警。
import re

# ---------- syntax.rs ----------
p = 'src/syntax.rs'
s = open(p, encoding='utf-8').read()

# 1) LONG_STOP 常量
old = "pub(crate) const LONG_STOP: u8 = 3;\n"
assert s.count(old) == 1
s = s.replace(old, "")

# 2) IcsInfo 两个 prev 字段
old = """    /// TNS 最大频带数。
    pub tns_max_bands: u8,
    /// 上一帧 window_sequence（滤波器组交界）。
    pub prev_window_sequence: u8,
    /// 上一帧 window_shape。
    pub prev_window_shape: bool,
}"""
new = """    /// TNS 最大频带数。
    pub tns_max_bands: u8,
}"""
assert s.count(old) == 1
s = s.replace(old, new)

# 3) parse_ics_info 签名与构造（去掉两个 prev 参数）
old = """fn parse_ics_info(
    r: &mut BitReader,
    asc: &AudioSpecificConfig,
    prev_window_sequence: u8,
    prev_window_shape: bool,
) -> Result<IcsInfo, AacError> {"""
new = """fn parse_ics_info(r: &mut BitReader, asc: &AudioSpecificConfig) -> Result<IcsInfo, AacError> {"""
assert s.count(old) == 1, "parse_ics_info sig"
s = s.replace(old, new)
# 构造体里两行
old = """        prev_window_sequence,
        prev_window_shape,
    })
}"""
if s.count(old) == 1:
    s = s.replace(old, """    })
}""")
else:
    # 找 IcsInfo { ... } 构造尾部
    m = re.search(r"prev_window_sequence,\n\s*prev_window_shape,\n", s)
    assert m, "ics construct"
    s = s[:m.start()] + s[m.end():]

# 4) parse_raw_data_block 包装函数删除 + _at 变体签名收窄
old = """/// 解析 raw data block（SCE/CPE/LFE/DSE/FIL/END；CCE/PCE 拒绝）。
///
/// # Errors
/// 见 [`parse_channel`] 与各元素语法。
pub(crate) fn parse_raw_data_block<'a>(
    data: &'a [u8],
    asc: &AudioSpecificConfig,
    prev_window_sequence: u8,
    prev_window_shape: bool,
) -> Result<RawDataBlock<'a>, AacError> {
    parse_raw_data_block_at(data, 0, asc, prev_window_sequence, prev_window_shape)
}

/// 同 [`parse_raw_data_block`]，但自 `bit_offset` 位起解析
///（LOAS 载荷非字节对齐）。
pub(crate) fn parse_raw_data_block_at<'a>(
    data: &'a [u8],
    bit_offset: usize,
    asc: &AudioSpecificConfig,
    prev_window_sequence: u8,
    prev_window_shape: bool,
) -> Result<RawDataBlock<'a>, AacError> {"""
new = """/// 解析 raw data block（SCE/CPE/LFE/DSE/FIL/END；CCE/PCE 拒绝），
/// 自 `bit_offset` 位起（LOAS 载荷非字节对齐；ADTS/raw 传 0）。
///
/// # Errors
/// 见 [`parse_channel`] 与各元素语法。
pub(crate) fn parse_raw_data_block_at<'a>(
    data: &'a [u8],
    bit_offset: usize,
    asc: &AudioSpecificConfig,
) -> Result<RawDataBlock<'a>, AacError> {"""
assert s.count(old) == 1, "raw block fns"
s = s.replace(old, new)

# 5) 内部两处 parse_ics_info 调用（共窗分支 + 各元素）
s = s.replace("""                        let ics = parse_ics_info(
                            &mut reader,
                            asc,
                            prev_window_sequence,
                            prev_window_shape,
                        )?;""",
"""                        let ics = parse_ics_info(&mut reader, asc)?;""")
s = s.replace("""                let ics =
                    parse_ics_info(&mut reader, asc, prev_window_sequence, prev_window_shape)?;""",
"""                let ics = parse_ics_info(&mut reader, asc)?;""")
s = s.replace("""                let ics = parse_ics_info(
                    &mut reader,
                    asc,
                    prev_window_sequence,
                    prev_window_shape,
                )?;""",
"""                let ics = parse_ics_info(&mut reader, asc)?;""")

# 6) RawDataBlock.reader 字段删除
old = """/// raw data block 解析结果。
pub(crate) struct RawDataBlock<'a> {
    /// 残余位流（END 之后，供对齐校验）。
    pub reader: BitReader<'a>,
    /// 元素序列。
    pub elements: Vec<RawElement>,
}"""
new = """/// raw data block 解析结果。
pub(crate) struct RawDataBlock<'a> {
    /// 元素序列。
    pub elements: Vec<RawElement>,
    marker: std::marker::PhantomData<&'a [u8]>,
}"""
assert s.count(old) == 1
s = s.replace(old, new)
old = "    Ok(RawDataBlock { reader, elements })"
new = """    Ok(RawDataBlock {
        elements,
        marker: std::marker::PhantomData,
    })"""
assert s.count(old) == 1
s = s.replace(old, new)

# 7) debug_trace_raw_block 整函数删除（从 doc 注释起到文件尾的下一个顶层项）
start = s.index("/// 诊断（#[doc(hidden)]，施工期对拍定位用）：逐元素解析并回报推进。")
# 函数体到下一个 "}" 顶层收尾：找 "\n}\n" 出现在 start 之后的第一处（函数级缩进为 0）
end = s.index("\n}\n", start) + len("\n}\n")
s = s[:start] + s[end:]

# 8) 无用比较
old = """                sect_end += incr;
                if r.remaining() < 0 {
                    return Err(AacError::InvalidData("section_data 越界".into()));
                }
                if sect_end > ics.max_sfb as usize {"""
new = """                sect_end += incr;
                if sect_end > ics.max_sfb as usize {"""
assert s.count(old) == 1
s = s.replace(old, new)

# 9) 损坏注释修复
old = s[s.index("    // gain_control_data_present"):]
head = old[: old.index("\n", old.index("SSR"))]
s = s.replace(
    head,
    "    // gain_control_data_present：SSR 工具——按 FFmpeg 语义跳过语法（LC 不出现）",
)

# 10) ChannelSyntax.pulse 字段删除
old = """    /// 重建 scalefactor 链（同排布；强度位 = is_pos，ZERO 位 = 0）。
    pub scalefactors: Vec<i16>,
    /// pulse_data。
    pub pulse: Option<PulseData>,
    /// tns_data。"""
new = """    /// 重建 scalefactor 链（同排布；强度位 = is_pos，ZERO 位 = 0）。
    pub scalefactors: Vec<i16>,
    /// tns_data。"""
assert s.count(old) == 1
s = s.replace(old, new)
old = """    Ok(ChannelSyntax {
        ics: ics.clone(),
        global_gain,
        codebooks,
        scalefactors,
        pulse,
        tns,
        coeffs,
    })"""
new = """    Ok(ChannelSyntax {
        ics: ics.clone(),
        global_gain,
        codebooks,
        scalefactors,
        tns,
        coeffs,
    })"""
assert s.count(old) == 1
s = s.replace(old, new)

# 11) 测试构造体去 prev 字段 + 未用变量
s = s.replace("""            prev_window_sequence: ONLY_LONG,
            prev_window_shape: false,
""", "")
old = "        let asc = test_asc();"
assert s.count(old) >= 1
s = s.replace(old, "        let _asc = test_asc();")

open(p, 'w', encoding='utf-8', newline='\n').write(s)
print('syntax.rs cleaned')

# ---------- decoder.rs ----------
p = 'src/decoder.rs'
s = open(p, encoding='utf-8').read()

s = s.replace("use std::sync::Arc;\n", "")
old = """use crate::syntax::{
    self, AudioSpecificConfig, BitReader, LoasState, PairCommon, RawElement, EIGHT_SHORT,
    LONG_START, LONG_STOP, ONLY_LONG,
};"""
new = """use crate::syntax::{
    self, AudioSpecificConfig, BitReader, LoasState, PairCommon, RawElement, EIGHT_SHORT,
    LONG_START, ONLY_LONG,
};"""
assert s.count(old) == 1
s = s.replace(old, new)

old = """        // 逐声道 prev 窗状态快照（首声道 = 元素 0 / CPE ch0，次 = CPE ch1）
        let prev_seq = [ONLY_LONG; 2]; // 滤波器组不消费 prev 序列，仅 shape
        let prev_shapes: Vec<bool> = self.prev_shape.clone();
        let rdb = syntax::parse_raw_data_block_at(
            block,
            bit_offset,
            &asc,
            prev_seq[0],
            prev_shapes.first().copied().unwrap_or(false),
        )?;"""
new = """        // 逐声道 prev 窗形快照（首声道 = 元素 0 / CPE ch0，次 = CPE ch1）
        let prev_shapes: Vec<bool> = self.prev_shape.clone();
        let rdb = syntax::parse_raw_data_block_at(block, bit_offset, &asc)?;"""
assert s.count(old) == 1
s = s.replace(old, new)

# debug 字段与访问器、debug_first_frame 删除
old = """    /// 诊断槽（#[doc(hidden)]，对拍期用）：最近一帧逐声道（谱、sf 链、
    /// codebooks、global_gain、窗序）。"""
i = s.index(old)
j = s.index("debug_coeffs: None,\n", i)
s = s[:i] + s[j + len("debug_coeffs: None,\n"):]
# 结构体里的字段声明与 stash、访问器逐个删
for frag in [
    """    /// 诊断：最近一帧声道 0 的 sf 链。
    debug_sf: Option<(Vec<i16>, Vec<u8>, u8, u8)>,
    /// 诊断：最近一帧声道 0 的频域谱（TNS 前）。
    debug_spectrum: Option<Vec<Vec<f32>>>,
    /// 诊断：最近一帧声道 0 的系数谱。
    debug_coeffs: Option<Vec<f32>>,
""",
]:
    assert s.count(frag) == 1, frag[:40]
    s = s.replace(frag, "")
s = s.replace("""                    if ci == 0 {
                        self.debug_coeffs = Some(ch.coeffs.clone());
                    }
""", "")
s = s.replace("""                    if ci == 0 {
                        self.debug_sf = Some((
                            ch.scalefactors.clone(),
                            ch.codebooks.clone(),
                            ch.global_gain,
                            ch.ics.window_sequence,
                        ));
                    }
""", "")
# 诊断访问器（两个 doc(hidden) 方法）
i = s.index("    /// 诊断访问器（#[doc(hidden)]，对拍期用）：最近一帧逐声道谱。")
j = s.index("\n}\n", i) + len("\n}\n")
s = s[:i] + s[j:]
# debug_first_frame 顶层函数
i = s.index("/// 诊断（#[doc(hidden)] 施工对拍用）：解首帧并回传（谱、窗信息、时域）。")
j = s.index("\n}\n", s.index("pub fn debug_first_frame", i)) + len("\n}\n")
s = s[:i] + s[j:]

open(p, 'w', encoding='utf-8', newline='\n').write(s)
print('decoder.rs cleaned')

# ---------- lib.rs ----------
p = 'src/lib.rs'
s = open(p, encoding='utf-8').read()
s = s.replace("pub use syntax::debug_trace_raw_block;\n", "")
s = s.replace("debug_trace_raw_block,\n", "")
open(p, 'w', encoding='utf-8', newline='\n').write(s)
print('lib.rs cleaned')

# ---------- stereo.rs / spectral.rs / tns.rs 测试构造 ----------
for p, extra in [('src/stereo.rs', None), ('src/spectral.rs', None), ('src/tns.rs', None)]:
    s = open(p, encoding='utf-8').read()
    frag = """            prev_window_sequence: crate::syntax::ONLY_LONG,
            prev_window_shape: false,
"""
    if frag in s:
        s = s.replace(frag, "")
    if p == 'src/stereo.rs':
        s = s.replace(
            "    ChannelSyntax, PairCommon, CB_INTENSITY_HI, CB_INTENSITY_LO, CB_NOISE,\n",
            "    ChannelSyntax, PairCommon, CB_INTENSITY_HI, CB_INTENSITY_LO, CB_NOISE,\n",
        )
        old = """            pulse: None,
            tns: None,"""
        assert s.count(old) == 1
        s = s.replace(old, """            tns: None,""")
    open(p, 'w', encoding='utf-8', newline='\n').write(s)
    print(p, 'cleaned')
print('ALL DONE')

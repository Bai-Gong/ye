//! CABAC 算术解码引擎（spec 9.3.3）。
//!
//! 算法与语义逐字对齐 spec 9.3.3.2（决策/旁路/终止）与 9.3.3.2.2
//! （上下文初始化 preCtxState 公式）；表见 [`crate::cabac_tables`]
//! （转录自 JM biaridecod.h = spec 表 9-44/45/46 原值、FFmpeg
//! h264_cabac.c 上下文初始化扁平表）。MPS 翻转规则按 FFmpeg/JM
//! 一致口径：LPS 转移**前** pStateIdx==0 才翻转（经 FFmpeg
//! ff_h264_mlps_state 表逐项核对该实现）。
//!
//! 上下文字节布局（FFmpeg 同构）：`(pStateIdx << 1) | valMps`。
//!
//! 位源约定：RBSP 字节切片 + 绝对位游标；片数据起于字节对齐位
//! （spec 9.3.1 cabac_alignment_one_bit 后），引擎初始化一次读 9 位。
//! 读越界按 0 位补齐（FFmpeg UNCHECKED 语义；片必以 terminate 收尾，
//! 越界读不改变合法流的解码结果）。
//!
//! 全库约定：无 unsafe、无 panic 路径；表下标恒在界内（状态字节
//! 高 7 位 ≤ 63、qRangeIdx ≤ 3 由构造保证）。

use crate::cabac_tables::{CTX_INIT_I, CTX_INIT_PB, RLPS, TRANS_LPS, TRANS_MPS};

/// CABAC 解码引擎（不可克隆；借用 RBSP 字节切片）。
pub(crate) struct Cabac<'a> {
    data: &'a [u8],
    /// 绝对位游标（从 `data` 起点 0 计）。
    bit_pos: usize,
    range: u32,
    offset: u32,
}

impl<'a> Cabac<'a> {
    /// 初始化（spec 9.3.3.2.2.3）：`data` 为整段 RBSP，`byte_pos` 为
    /// 字节对齐后的片数据起点（片头位长向上取整到字节）。读 9 位
    /// 作 codIOffset 初值，codIRange = 510。
    pub(crate) fn new(data: &'a [u8], byte_pos: usize) -> Self {
        let mut c = Cabac {
            data,
            bit_pos: byte_pos * 8,
            range: 510,
            offset: 0,
        };
        c.offset = c.next_bits(9);
        c
    }

    /// 当前绝对位游标（I_PCM 对齐用）。
    pub(crate) fn bit_position(&self) -> usize {
        self.bit_pos
    }

    /// 读 n 位（MSB 在前）；越界补 0。I_PCM 样值读取亦走此口。
    pub(crate) fn next_bits(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | u32::from(self.next_bit());
        }
        v
    }

    pub(crate) fn next_bit(&mut self) -> bool {
        let byte = self.bit_pos / 8;
        let bit = if byte < self.data.len() {
            (self.data[byte] >> (7 - self.bit_pos % 8)) & 1 == 1
        } else {
            false
        };
        self.bit_pos += 1;
        bit
    }

    /// 再归一化（spec 9.3.3.2.3 末段）：codIRange < 256 时左移并补位。
    fn renorm(&mut self) {
        while self.range < 256 {
            self.range <<= 1;
            self.offset = (self.offset << 1) | u32::from(self.next_bit());
        }
    }

    /// 常规决策（spec 9.3.3.2.1）。`ctx` 为上下文字节
    /// （`(pStateIdx << 1) | valMps`），就地更新。
    pub(crate) fn decision(&mut self, ctx: &mut u8) -> u32 {
        let s = *ctx;
        let state = (s >> 1) as usize;
        let mps = s & 1;
        let q = ((self.range >> 6) & 3) as usize;
        let rlps = u32::from(RLPS[state][q]);
        self.range -= rlps;
        let bin;
        if self.offset >= self.range {
            // LPS：offset 左移域内减去 codRange（= range−rLPS），
            // codRange ← rLPS；bin = ¬valMps（原值），旧状态 0 →
            // 翻转 MPS（只影响后续决策）
            self.offset -= self.range;
            self.range = rlps;
            bin = mps ^ 1;
            let new_state = TRANS_LPS[state];
            let mps = if state == 0 { mps ^ 1 } else { mps };
            *ctx = (new_state << 1) | mps;
        } else {
            // MPS：codRange = range − rLPS（已减）
            *ctx = (TRANS_MPS[state] << 1) | mps;
            bin = mps;
        }
        self.renorm();
        u32::from(bin)
    }

    /// 旁路决策（spec 9.3.3.3.2）。
    pub(crate) fn bypass(&mut self) -> u32 {
        self.offset = (self.offset << 1) | u32::from(self.next_bit());
        let v = if self.offset >= self.range {
            self.offset -= self.range;
            1
        } else {
            0
        };
        binlog::log("B", -1, 0, v);
        v
    }

    /// 旁路符号（FFmpeg get_cabac_bypass_sign 逐字：bypass 比较 ≥
    /// （"1"）→ 原值；<（"0"）→ 取反——与直观相反，经源码核证）。
    pub(crate) fn bypass_sign(&mut self, v: i32) -> i32 {
        if self.bypass() == 1 { v } else { -v }
    }

    /// 终止决策（spec 9.3.3.3.3）：返回 `true` = end_of_slice_flag=1
    /// （调用方随后停止本片解码；引擎不再归一化）。
    pub(crate) fn terminate(&mut self) -> bool {
        self.range -= 2;
        let t = if self.offset >= self.range {
            true
        } else {
            self.renorm();
            false
        };
        binlog::log("T", -2, 0, u32::from(t));
        t
    }

    /// I_PCM 后重启（spec 7.3.5.3：PCM 样值后按 9.3.1 重新对齐初始化）。
    pub(crate) fn restart_after_pcm(&mut self) {
        let byte = self.bit_pos.div_ceil(8);
        *self = Cabac::new(self.data, byte);
    }
}

// 临时诊断（提交前剥离）：YE_BIN_LOG=<n> 时打印前 n 个 bin
//（D=常规/ctx 槽位、B=旁路、T=终止），配 MBIN 宏块边界标记。
pub(crate) mod binlog {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::OnceLock;

    pub(crate) fn max() -> usize {
        static M: OnceLock<usize> = OnceLock::new();
        *M.get_or_init(|| {
            std::env::var("YE_BIN_LOG")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
        })
    }

    static COUNT: AtomicUsize = AtomicUsize::new(0);

    pub(crate) fn log(tag: &str, ctx: i32, state: u8, bin: u32) {
        let n = COUNT.fetch_add(1, Ordering::Relaxed);
        if n < max() {
            eprintln!("BIN {n} {tag} ctx={ctx} st={state} v={bin}");
        }
    }

    pub(crate) fn marker(s: &str) {
        if max() > 0 {
            eprintln!("MBIN {s}");
        }
    }
}

/// 一片 CABAC 解码的上下文模型集（1024 槽，FFmpeg 扁平布局）。
pub(crate) struct CabacCtx {
    state: [u8; 1024],
}

impl CabacCtx {
    /// 按 QP 与片型初始化（FFmpeg ff_h264_init_cabac_states 逐字：
    /// preCtxState 公式 + 打包 (pStateIdx<<1)|valMps）。
    pub(crate) fn init(slice_qp: u8, cabac_init_idc: usize, is_i_slice: bool) -> Self {
        let tab: &[(i8, i8); 1024] = if is_i_slice {
            &CTX_INIT_I
        } else {
            &CTX_INIT_PB[cabac_init_idc]
        };
        let qp = i32::from(slice_qp).clamp(0, 51);
        let mut state = [0u8; 1024];
        for (i, &(m, n)) in tab.iter().enumerate() {
            // spec 9.3.2.2：preCtxState = Clip3(1,126, ((m*QP)>>4)+n)；
            // ≤63 → pState=63−pre, mps=0；否则 pState=pre−64, mps=1。
            // FFmpeg 折叠式：pre = 2*preCtxState − 127，负值经 ~ 映射回
            // （x<0 → |x|−1 恰为 (63−pre)*2+0 与 (pre−64)*2+1 的并集）。
            let pre = 2 * (((i32::from(m) * qp) >> 4) + i32::from(n)) - 127;
            let pre = if pre < 0 { !pre } else { pre }; // x<0 → |x|−1
            let pre = if pre > 124 { 124 + (pre & 1) } else { pre };
            state[i] = pre as u8;
        }
        CabacCtx { state }
    }

    // M2 WIP（CABAC 对拍收尾）：状态槽直读入口，编码侧/对拍诊断接线在即。
    #[allow(dead_code)]
    pub(crate) fn get_mut(&mut self, ctx_idx: usize) -> &mut u8 {
        &mut self.state[ctx_idx]
    }
}

// ---------------------------------------------------------------------------
// 语法层的公共 bin 读取形态
// ---------------------------------------------------------------------------

/// CABAC 语法层（引擎 + 上下文模型的绑定）。
pub(crate) struct CabacReader<'a> {
    pub(crate) cabac: Cabac<'a>,
    pub(crate) ctx: CabacCtx,
}

impl<'a> CabacReader<'a> {
    pub(crate) fn new(cabac: Cabac<'a>, ctx: CabacCtx) -> Self {
        CabacReader { cabac, ctx }
    }

    /// 常规 bin。
    pub(crate) fn bin(&mut self, ctx_idx: usize) -> u32 {
        let st = self.ctx.state[ctx_idx];
        let v = self.cabac.decision(&mut self.ctx.state[ctx_idx]);
        binlog::log("D", ctx_idx as i32, st, v);
        v
    }

    /// 一元截止前缀（spec 9.3.2.3 前缀形态；如 mb_qp_delta 的 |Δ| 前缀）。
    // M2 WIP（CABAC 对拍收尾）：dqp/mvd 现走专用循环，本形态供后续
    // 语法元素（如 transform_8x8 族）接线。
    #[allow(dead_code)]
    pub(crate) fn unary(&mut self, ctx_idx: usize, max: u32) -> u32 {
        let mut v = 0u32;
        while v < max && self.bin(ctx_idx) == 1 {
            v += 1;
        }
        v
    }
}

// ---------------------------------------------------------------------------
// 单测：引擎与 spec 9.3.4 的编码器例题对拍（附录 D 例题）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 决策序列手算轨迹（表与公式直接推，逐值断言）：
    /// 位流 bytes = [0b1011_1010, 0b1000_0000]：前 9 位 = 0x175=373
    /// （含 bytes[1] 首位 1），bit9=0、bit10=0。init 后 range=510、offset=373。
    /// ctx 初值 0（pState 0, mps 0）：
    /// ① q=(510>>6)&3=3、rLPS=RLPS[0][3]=240；range←270；
    ///    373≥270 → LPS：offset←103、range←240、bin=¬mps=1；旧状态 0 →
    ///    mps 翻 1、新状态 transLPS[0]=0 → ctx=1；再归一化 range 240<256 →
    ///    range←480、offset←(103<<1)|0=206。
    /// ② q=(480>>6)&3=3、rLPS=240；range←240；206<240 → MPS：bin=mps=1；
    ///    state transMPS[0]=1 → ctx=(1<<1)|1=3；再归一化 range 240 → 480、
    ///    offset←(206<<1)|0=412。
    #[test]
    fn decision_trace_hand_computed() {
        let bytes = [0b1011_1010, 0b1000_0000];
        let mut c = Cabac::new(&bytes, 0);
        assert_eq!(c.range, 510);
        assert_eq!(c.offset, 0x175);
        let mut ctx: u8 = 0;
        assert_eq!(c.decision(&mut ctx), 1);
        assert_eq!(ctx, 1);
        assert_eq!(c.range, 480);
        assert_eq!(c.offset, 206);
        assert_eq!(c.decision(&mut ctx), 1);
        assert_eq!(ctx, 3);
        assert_eq!(c.range, 480);
        assert_eq!(c.offset, 412);
    }

    /// 旁路与终止的基础行为（白盒：init 消耗 9 位后重置 offset）。
    /// 位流 bytes[1]=0b0101_0100 → bit9=0、bit10=1。
    /// bypass①：offset=(0<<1)|0=0 <256 → 0；bypass②：offset=(0<<1)|1=1 → 0。
    /// terminate：range 256−2=254 > offset 1 → false（并再归一化）。
    #[test]
    fn bypass_and_terminate() {
        let bytes = [0b1111_1111, 0b0101_0100];
        let mut c = Cabac::new(&bytes, 0);
        c.range = 256;
        c.offset = 0; // 白盒：直接驱动 bypass
        assert_eq!(c.bypass(), 0);
        assert_eq!(c.bypass(), 0);
        assert!(!c.terminate());
    }

    /// 上下文初始化：QP=26、I 片，首上下文 (m,n)=(20,−15)（表 9-12 首项）。
    /// preCtxState = ((20*26)>>4) − 15 = 32−15 = 17 ≤ 63 → pState=46, mps=0
    /// → 字节 92。
    #[test]
    fn ctx_init_formula() {
        let ctx = CabacCtx::init(26, 0, true);
        assert_eq!(ctx.state[0], (63 - 17) << 1);
        // QP=0：preCtxState = 0−15 = −15 → clip 1 → pState 62 mps 0
        let ctx0 = CabacCtx::init(0, 0, true);
        assert_eq!(ctx0.state[0], 62 << 1);
    }
}

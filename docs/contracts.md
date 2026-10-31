# 接口契约

按 [architecture.md](architecture.md) 演进规则维护：每一层落地时，把其对外
接口契约补记到本文件；改契约必须同步改这里，本文与代码不一致以最近一次
合入的代码为准并须立刻修正其一。

本文记录已落地的各层：门面/feature 接线、`ye-time`、`ye-pixfmt`、
`ye-container-mp4`（读取端 + 封装端 `Mp4Writer`）、`ye-cli`
（`probe` / `remux` / `convert` 真转码），外加合流后定形的
「锻的最小 API 面」与「M2+ 接口桩契约」。M3 首轮（2026-10-26）新增
「H.264 编码器输出契约」与「处理图最小集契约」两节，并同步收窄
桩契约（H.264 编码桩删除、AAC 编码桩文案改后续批次）。M3 集成阶段
（2026-10-27）convert 编码槽从桩换真实现（见「锻的最小 API 面」）。
**M3 补齐（2026-10-26 晚）**：真码控（`EncoderConfig.bit_rate` 由
H.264 编码器消费，CBR/A-VBR）+ AAC-LC 编码器（`register_aac_encoder`，
AAC 编码桩随交付删除）+ convert 音频三态与剥视频（见「锻的最小
API 面」签名块）。**M4 首轮（2026-10-26 深夜）**：新增「合成导出
最小集契约」——`ye::compose` 顺序拼接泵，剪导出接管的纯剪辑子集。

> 2026-10-24 合流后：本文 m1 侧各节（`ye-time` / `ye-pixfmt` /
> `ye-container-mp4` / `ye-cli` / `Mp4Writer` 及 feature 链）描述的是
> 并入的 m1-mp4 实现。其中**类型与时戳层**已被「合流裁决记录」第
> 1–4 条与「移植裁决记录」取代——`ye-time` / `ye-pixfmt` 两节的
> 签名块是 m1 方案（全局 tick 域、`pub i64` 字段、无错误枚举），
> 现行签名以 [architecture.md](architecture.md)「接口契约」四节与
> 源码为准；**box 解析与 fMP4/writer 硬契约**（解析规则 1–13、
> 写入契约、`Layout::Fragmented` 语义）在移植后原样存活，仍以
> 这些节为准。

## 合流裁决记录（2026-10-24，B 案）

m1-mp4（MP4 全功能实现）合入 develop（契约层）。裁决：**以 origin/develop
的契约层为主体，m1-mp4 的实现重写挂到 trait 后再恢复编译**。文件级：
`ye-time` / `ye-pixfmt` / `ye-container` 的 `lib.rs` 与 `Cargo.toml`
develop 版胜出；`ye-cli` 以 m1 版为底；`Cargo.lock` 重新生成；
`docs/roadmap.md` 双方内容都保留；`ye-container-mp4` 整目录并入、
暂从 workspace members 排除（根 `Cargo.toml` `exclude`）。
（「暂从 workspace 排除」与「`ye-cli` 以 m1 版为底」为合流时点的
过渡态，已分别由移植与接线阶段终结——见下两节。）

语义级四处裁决（重写 `ye-container-mp4` 时必须遵守，以 develop 版代码为准）：

1. **时戳 = 流内时基 + 自描述 `Timestamp`**：`ye_time::Timestamp` 为
   `ticks: i64 + time_base: Rational`（秒/tick），自带所属时基、跨函数
   边界绝不裸传 `i64`；无哨兵值，缺时戳一律 `Option<Timestamp>`；
   跨时基比较走 `Timestamp::compare`，不实现 `Ord`
   （`crates/ye-time/src/timestamp.rs`）。**取代** m1 的全局
   `TICKS_PER_SECOND` 单一 tick 域方案。
2. **`Rational` = `i32` 私有字段**：分子分母为私有 `i32`、构造即约分、
   经 `num()`/`den()` 访问；i128 中间量归约后落不回 `i32` → `Overflow`
   （`crates/ye-time/src/rational.rs`）。**取代** m1 的 `pub num: i64,
   pub den: i64` 公有字段方案。
3. **seek = 前一关键帧、经只读查询实现**：`Demuxer::seek` 定位到指定流
   时基下 `target` 的**前一个关键帧**，返回实际落地时刻（同流时基）
   （`crates/ye-container/src/demuxer.rs`）；重写 mp4 读取端时以其
   样例表/索引的**只读查询**实现，不靠可变游标状态。
4. **包按流时基打戳**：`Packet` 的 `pts`/`dts`/`duration` 为
   `Option<Timestamp>`，解封装器负责以对应 `Stream` 的 `time_base`
   打戳并填 `stream_index`，重标度语义落在 demuxer 不在调用方；封装器
   写包时向目标流时基重标度（`crates/ye-codec/src/packet.rs`、
   `crates/ye-container/src/demuxer.rs`）。

## 移植裁决记录（2026-10-24，ye-container-mp4 重写挂 develop 类型）

按上节四条语义裁决完成 `ye-container-mp4` 整 crate 移植（m1 实现 →
develop 契约类型），恢复为 workspace 成员（根 `Cargo.toml` 的
`exclude` 移除）。逐项裁决：

1. **读取端时戳 = 流时基原生单位**：`read_packet` 产出的
   `pts/dts/duration` 为 `Some(Timestamp::new(轨道单位, 1/timescale))`
   ——单位即 tick，**全程无换算无舍入**（m1 的全局 tick 换算及其
   half-away 舍入路径在读端消失，任何 timescale 下逐包精确）。
   `Stream.duration = Some(mdhd/末 fragment 尾的轨道单位值)`，
   `nb_frames = Some(样例数)`，`start_time = None`（无 elst 解析）。
   `pts` 恒 `Some(dts+cts)`（ctts 缺省 ⇒ cts=0）——比 m1 的
   `None 表示等于 dts` 更显式。
2. **写入端重标度 = NearInf**：`write_packet` 收到任意时基的自描述
   包后，`Timestamp::rescale`（默认 [`Rounding::NearInf`]，对齐 FFmpeg
   `av_rescale_q` 默认）到轨道时基单位域，衔接校验与入表均在单位域；
   mvhd/tkhd 的 movie 时基（1/1000）换算同取 NearInf；
   `fragment_duration` 阈值比较走 `Timestamp::compare` 精确交叉相乘
   （无舍入）。新增契约：包缺 `dts`/`duration` ⇒ `Misuse`（M1 封装
   不推断时戳）；时基不符先重标度后校验。
3. **类型映射**：`Stream` → `ye_container::Stream`（本 crate 不再
   自定义，经门面 re-export）；`Packet` → `ye_codec::Packet`
   （`keyframe` → `PacketFlags::KEYFRAME`、`data` → `Arc<[u8]>`、
   `stream_index` 为 u32、`sample_index` 字段删除——随机访问 API 的
   序号由调用方自带）；`MediaKind` → `MediaType`；`VideoInfo`/
   `AudioInfo` → `ye_codec::VideoParams`/`AudioParams`（stsd 表达不了
   的 `pixel_format`/`frame_rate`/`sample_format` 置 None）。
4. **四字符码保留，codec 处映射 [`CodecId`]**：box 类型（moov/stts/…）
   与 stsd entry 类型不是 codec，`FourCc` 留在本 crate（box_tree 公开
   面）。映射：读取向 `avc1|avc3→H264、hvc1|hev1→Hevc、mp4a→Aac、
   其余 Unknown`；写入向仅 `H264→avc1、Aac→mp4a`（其余
   `Unsupported`）。原始 fourcc 与 tkhd `track_id` 经新增只读查询
   `Mp4Reader::track_info(stream_index)` 暴露（develop `Stream` 无处
   安放这两个 MP4 特有字段）。
5. **非 vide/soun 轨的媒体类别**：develop `MediaType` 无 Other——
   内部仍按三值 TrackKind 处理（entry 布局取 8 字节基座、关键帧走
   stss 链），公开面映射 `Subtitle` + `CodecId::Unknown`。
6. **声道**：stsd 只有 channelcount；develop `AudioParams` 无
   channel_count 字段——按 FFmpeg `av_get_default_channel_layout`
   表映射 1..=8 声道为默认布局（声道数经 `layout.channels()` 保全），
   其余 `None`。
7. **trait 面（本阶段零改动）**：`Demuxer`/`Muxer` 及注册表未动。
   trait 面放不下的 mp4 专属面保留在本 crate 具体类型上，接线阶段
   再挂 trait：`Mp4WriterOptions`/`Layout`/`flush_fragment`（封装
   选项与 fMP4 切片，`MuxerFactory::open` 无参承载不了）、
   `read_packet(stream_index, sample_index)` 随机访问与 `sample_count`/
   `box_tree`（`Demuxer::read_packet` 是顺序游标语义）、`TrackConfig`
   （`StreamTemplate` 之外的 stsd 级配置）。
8. **新增只读查询（接线阶段 `Demuxer::seek` 的依据，不添行为）**：
   `Mp4Reader::keyframe_index_before(stream_index, sample_index)`
   （≤ 该序号的最近关键帧序号，stss/fragment flags 既有判定回溯）与
   `Mp4Reader::sample_timing(stream_index, sample_index)`
   （dts/pts/duration 三元组，stts/ctts/tfdt 既有计算，不读数据）；
   两者错误语义与 `read_packet` 一致。
9. **oracle 对拍口径更新**：ffprobe 逐包对拍与时戳 roundtrip 一律
   流时基原生单位严格相等（取代 m1 的「单位域相等、tick 域须整除」
   分组——新时戳模型下整除条件不存在）；m1 的「ye probe 冒烟」用例
   （构建 ye-cli 真二进制）随合流移出 oracle_demux——ye-cli 为 m1 版、
   develop 上暂不编译（超出本 crate 移植范围），待其按新契约重写后
   回归。

测试基线（本次移植后）：`cargo test -p ye-container-mp4` 84 项全绿
（手工字节单元层 + ffmpeg/ffprobe oracle 对拍，oracle 组在 PATH 无
工具时自动跳过）；develop 既有 113 项（ye-time 27 / ye-pixfmt 28 /
ye-codec 34 / ye-container 24，合流时基线）经 `cargo test --workspace
--exclude ye-cli` 验证无回退；ye-cli 在 develop 上维持合流时的既有编译
失败（引用已删除的 m1 ye-time API），待接线阶段处理。

## 接线与桩裁决记录（2026-10-24，合流第三阶段）

移植裁决第 7/8 条预留的工作随后完成：trait 挂接、注册探测、ye-cli
重写、门面上提、convert 占位与 M2+ 桩。逐项裁决（实现见
`crates/ye-container-mp4/src/adapter.rs`、`crates/ye-cli/src/`、
`crates/ye/src/`、`crates/ye-codec/src/stubs.rs`、`crates/ye-filter/src/`）：

1. **trait 面零改动**：`Demuxer`/`Muxer`/`ContainerRegistry`/
   `CodecRegistry` 均未动——接线只做包装与注册，不添 trait 行为；
   mp4 专属面（移植裁决第 7 条）保持具体类型直达，不经 trait。
2. **`Mp4Demuxer`**（adapter.rs，只加新文件、未改 reader/writer）包
   `Mp4Reader<Box<dyn ReadSeek>>`：`read_packet` 把随机访问折成顺序
   游标——跨轨按 `Timestamp::compare` 精确交叉相乘比较 dts 归并，
   并列取流号小者，EOF 幂等；`seek` 二分找到 ≤ target 的最大 dts
   样例后，经只读查询 `keyframe_index_before` 回溯关键帧（合流裁决
   第 3 条「不靠可变游标」的落实），其余轨前向恢复到首个
   dts ≥ 落地时刻——契约未约束其余轨的恢复策略，该选择在 adapter
   头注声明。包打戳契约（合流裁决第 4 条）由 `Mp4Reader::read_packet`
   原生满足（流时基），本层零换算透传。
3. **`Mp4Muxer`** 包 `Mp4Writer<Box<dyn WriteSeek>>`（progressive
   布局）：trait 状态机（未写头/写入中/已收尾）映射
   `ContainerError::InvalidState`；fMP4 布局选项走具体类型（移植裁决
   第 7 条）。错误映射：`Mp4Error`/`WriterError` → `ContainerError`
   （Io 原样透传、流索引 → `StreamNotFound`、`WriterError::Misuse` →
   `InvalidState`、`WriterError::Time` → `Time`（2026-10-24 评审统一：
   数据驱动的时戳重标度溢出按 Muxer 契约走 Time 变体）、其余 →
   `InvalidData` 带原文案）。
4. **探测评分**：`DemuxerFactory::probe` 做 ftyp/styp box 嗅探——
   size ≥ 8 + box 类型四字符命中 + major brand 在常见集（12 项，
   2026-10-24 评审移除 heic——HEIF 静态图片无 moov/trak，读取端另有
   「至少一条 trak」校验兜底；同日施工图①再移除 `qt  `——qt 族 MOV
   降入拒绝表：major brand 为 `qt  ` 时直接 `NONE`（probe 即拒），
   `open_demuxer`/`probe`/`detect` 明确 `UnknownFormat`，不假成功，
   剪对 MOV 过渡期回退 ffprobe，M4 落地 MOV 支持后移除；对齐
   FFmpeg mov probe 常用子集）→ `ProbeScore::MAXIMUM`；brand 在
   拒绝表（现仅 `qt  `，只拦 major brand）→ `NONE`；brand 可读但
   不在集内 → `EXTENSION`；乱数据 → `NONE`。`register_mp4` 以格式名
   `"mp4"` 注册 demuxer/muxer 双工厂，auto-probe 由
   `ContainerRegistry::open_demuxer` 驱动；`ye-container` 本体零改动。
5. **门面 feature 上提**（取代 m1 设想的 `ye-container/mp4` 链）：
   合流后 `ye-container-mp4` 依赖 `ye-container`（契约类型），抽象层
   反向 optional 转发会成环——汇总点上提至门面
   `ye/mp4 = ["dep:ye-container-mp4"]`，re-export 在
   `ye::container::mp4`。`ye::container` 从纯 re-export 改为包装模块
   （`pub use ye_container::*` + `mp4` 子模块），`ye::container::<Item>`
   路径全部不变。
6. **remux 上提门面**：`ye::container::remux`（约 80 行归并泵，自 m1
   ye-cli 上提，时戳域换成流内自描述 `Timestamp`；跨轨归并已内化到
   `Demuxer::read_packet`，本层不重排）——auto-probe 打开 →
   `StreamTemplate` 建流 → 每轨整体平移到首包 dts = 0 → 逐包直通。
   回归 `crates/ye/tests/remux.rs`（fMP4 非零 tfdt 首 dts →
   progressive 平移验证）。
7. **convert 占位定形**：ye-cli 子命令 + lib 层 `convert`，参数形态即
   锻的进程内接入契约（进度 `(processed_us, total_us)` 微秒 +
   `&AtomicBool` 取消）；调用即返回含「M3 落地」的桩错误、不产出
   文件——完整契约见下文「锻的最小 API 面」。
8. **ye-codec 桩**：新增 `CodecError::UnsupportedStub(&'static str)`
   变体（不动既有变体）与 `stubs` 模块（H.264/AAC × 解码/编码四个
   桩工厂 + `register_stubs`）——语义见下文「M2+ 接口桩契约」。
9. **ye-filter 最小契约**：`PadDescriptor`/`FilterDescriptor`
   （纯数据）、`FilterError`、`FilterGraph` + `PadRef` + `NodeId`
   （`add_filter`/`connect` 真实生效并校验节点/垫索引，不去重不成环
   检测，`topological_order` 调用即 `Unsupported`）。垫的媒体类型复用
   `ye_codec::MediaType`（path 依赖，与 lavfi 依赖 lavcodec 的方向
   对称）。
## 门面与 feature 接线

依赖方向是硬规则，防止抽象层倒挂：

1. 创作工具（jian/duan/yue）只依赖门面 `ye`，不直接依赖任何 `ye-*` 实现 crate。
2. 具体实现 crate（如 `ye-container-mp4`）挂在 optional 依赖 + 同名
   feature 下，由门面 re-export。**例外（2026-10-24 合流裁决）**：实现
   crate 反向依赖抽象层取契约类型时（`ye-container-mp4` →
   `ye-container`），抽象层转发会成环——此类汇总点一律上提到门面
   （接线裁决第 5 条）。
3. `ye-cli` 显式声明所需 feature（`features = ["mp4"]`），不依赖
   default 的默认值。

当前 feature 表（2026-10-27 集成阶段后，`crates/ye/Cargo.toml`）：

```
ye/mp4          →  dep:ye-container-mp4  →  ye::container::mp4     （默认开启）
ye/codec-stubs  →  （桩本体在 ye-codec 恒编译）→  ye::stubs::codec  （默认关闭）
ye/filter-stubs →  （处理图本体在 ye-filter 恒编译）→  ye::stubs::filter （默认关闭）
ye/h264         →  dep:ye-codec-h264     →  ye::codec_h264        （默认关闭）
ye/aac          →  dep:ye-codec-aac      →  ye::codec_aac         （默认关闭）
ye/png          →  dep:ye-png            →  ye::png               （默认关闭）
```

关闭 `mp4`（`ye = { default-features = false }`）时 `ye::container::mp4`
不存在；`ye-cli` 因显式声明 feature 而不受门面 default 变更影响。
`codec-stubs`/`filter-stubs` 是接口桩/处理图的门面聚合入口（语义见
「M2+ 接口桩契约」）：显式开启即把「本构建依赖桩」记进依赖声明。
`h264` feature 同时汇总**编码器**
（`register_h264_encoder`/`H264Encoder`/`encoder_factory`）与解码器
（`register_h264`）；`aac` 同构汇总**编码器**
（`register_aac_encoder`，2026-10-26 M3 补齐）与解码器
（`register_aac`）；`png` 语义不变。**ye-cli 的 feature 声明 2026-10-26
M3 补齐起为 `["mp4", "h264", "aac"]`**（`--audio aac` 重编码需要
AAC 解码 + 编码真工厂；直通路径用不到）。

## ye-time（基础类型层）

> **m1 侧描述，签名块已被取代**：全局 tick 域、`pub i64` 字段、
> 无错误枚举均为 m1 方案，现行 `Rational`（私有 `i32` 字段）/
> `Timestamp`（`ticks + time_base` 自描述）/ `TimeError` 以
> [architecture.md](architecture.md)「ye-time 接口契约」与源码为准
> （合流裁决第 1/2 条）。

对标 `AVRational` + `av_rescale_q`。**无错误枚举**：所有可失败操作返回
`Option`（契约评审时定下"错误枚举必为死代码"）。

```rust
pub const TICKS_PER_SECOND: i64 = 7_056_000;

pub struct Rational { pub num: i64, pub den: i64 }   // den>0，已约分
impl Rational {
    pub const ZERO / ONE;
    pub fn new(num: i64, den: i64) -> Option<Rational>;   // 构造即约分
    pub fn reduce(self) -> Rational;                      // den==0/不可表示 → panic
    pub fn inverse(self) -> Option<Rational>;
    pub fn mul(self, rhs: Rational) -> Option<Rational>;  // 固有方法名，非 std::ops
    pub fn div(self, rhs: Rational) -> Option<Rational>;
    pub fn to_f64(self) -> f64;                           // 仅显示，禁止参与运算
}

pub struct Timestamp(i64);    // 全局 tick 整数，可为负（encoder delay）
impl Timestamp {
    pub const ZERO;
    pub fn from_ticks(ticks: i64) -> Timestamp;
    pub fn as_ticks(self) -> i64;
    pub fn from_seconds(seconds: i64) -> Option<Timestamp>;
    pub fn from_units(units: i64, timebase: Rational) -> Option<Timestamp>;
    pub fn to_units(self, timebase: Rational) -> Option<i64>;
    pub fn checked_add / checked_sub(self, rhs: Timestamp) -> Option<Timestamp>;
}

pub fn frame_duration_ticks(frame_rate: Rational) -> Option<Timestamp>;
pub fn sample_duration_ticks(sample_rate: i64) -> Option<Timestamp>;
```

语义要点：

- `TICKS_PER_SECOND = LCM(常见帧率, 44100, 48000) = 2^7·3^2·5^3·7^2`，
  对 24/25/30/50/60/120 与 44100/48000 整除。
- 单位换算（`from_units`/`to_units`）舍入 **half-away-from-zero**；
  `from_units` 溢出返回 `None`；非正帧率（`num <= 0`）、零/坏时基
  （`den == 0`、`num == 0`）防御性返回 `None`。
- `Rational` 的 `Ord` 按数值比较（i128 交叉相乘），与 `Eq`/`Hash` 一致。
- 已知边界（文档化的例外，非 bug）：`i64::MIN` 组合的不可表示结果返回
  `None`（如 `new(i64::MIN, -1)`、`new(1, i64::MIN)`）；可表示的都正确。

## ye-pixfmt（基础类型层）

> **m1 侧描述，签名块已被 develop 版取代**：如 `PixfmtError` 现为
> `BadAlignment(usize)` 等带载荷变体（m1 写作 `BadStrideAlign`）——
> 现行签名以 [architecture.md](architecture.md)「ye-pixfmt 接口契约」
> 与源码为准。

对标 `AVPixelFormat` 家族，只做描述与布局，转换内核归 `ye-filter`。

```rust
#[non_exhaustive]
pub enum PixelFormat { Gray8, Rgb24, Bgr24, Rgba8, Bgra8,
                       Yuv420P, Yuv422P, Yuv444P, Nv12, Nv21, Yuv420P10Le }
impl PixelFormat {
    pub const ALL: [PixelFormat; 11];
    pub fn plane_count(self) -> usize;
    pub fn bytes_per_sample(self, plane: usize) -> usize;      // 越界 panic
    pub fn subsampling_factors(self, plane: usize) -> (usize, usize); // 越界 panic
    pub fn bit_depth(self) -> u32;
    pub fn name(self) -> &'static str;
    pub fn from_name(name: &str) -> Option<PixelFormat>;
}

pub struct PlaneLayout { pub offset: usize, pub stride: usize,
                         pub rows: usize, pub size_bytes: usize }
pub struct FrameLayout { pub format, width, height, planes: [PlaneLayout; 4],
                         used_planes, total_bytes }
impl FrameLayout {
    pub fn new(format: PixelFormat, width: usize, height: usize,
               stride_align: usize) -> Result<FrameLayout, PixfmtError>;
    pub fn plane(&self, index: usize) -> Result<&PlaneLayout, PixfmtError>;
}

pub struct FrameBuffer { /* layout + Box<[u8]> */ }
impl FrameBuffer {
    pub fn alloc(layout: FrameLayout) -> FrameBuffer;         // 零初始化
    pub fn layout(&self) -> &FrameLayout;
    pub fn plane(&self, index: usize) -> Result<&[u8], PixfmtError>;      // 紧密切片
    pub fn plane_mut(&mut self, index: usize) -> Result<&mut [u8], PixfmtError>;
    pub fn as_bytes(&self) -> &[u8];                          // 含 padding，= total_bytes
}

pub enum PixfmtError { ZeroDimension, BadStrideAlign, PlaneIndexOutOfRange }
```

布局为**规范公式**（不是逐格式查表）：

- 每平面 `stride = align_up(ceil(width/宽除数) × bytes_per_sample, stride_align)`，
  `rows = ceil(height/高除数)`；`offset` 为前序平面 `size_bytes` 累加（紧凑排布）。
- `stride_align == 1` 表示无对齐；0 或非 2 的幂 → `BadStrideAlign`；
  宽或高为 0 → `ZeroDimension`。
- `plane()` 切片是紧的：`stride × (rows-1) + 末行有效宽`（末行不含 padding）；
  需要整块（含行尾 padding）用 `as_bytes()`。
- 注记：Nv12/Nv21 色度平面 stride 按同一公式得 `ceil(w/2)×2`（偶数宽即等于 w），
  与 FFmpeg linesize 一致；`Gray8` 的 `name()` 为 `"gray8"`（FFmpeg 接受的
  别名，规范名 `"gray"`）。

## ye-container-mp4（容器层，feature `mp4`）

> **类型面已被移植裁决第 1–6 条取代**：`Stream`/`Packet`/
> `MediaKind` 现为 `ye_container::Stream` / `ye_codec::Packet` /
> `ye_codec::MediaType`（时戳 `Option<Timestamp>` 流内自描述、
> `keyframe` → `PacketFlags::KEYFRAME`、`data` → `Arc<[u8]>` 等）。
> 下文 **box 解析硬契约 1–13 存活有效**；签名块以源码为准。

对标 libavformat mov demuxer 的只读子集（progressive 与 fMP4 均支持）。
封装（mux）由 `Mp4Writer` 提供——契约见文末
「ye-container-mp4 Mp4Writer」一节。

```rust
pub use ye_time::{Rational, Timestamp};

pub struct FourCc(bytes: [u8; 4]);       // new/from_bytes/as_bytes/as_str（\xNN 转义）
pub enum MediaKind { Video, Audio, Other }
pub struct VideoInfo { width: u32, height: u32 }              // stsd 像素尺寸
pub struct AudioInfo { channel_count: u32, sample_rate: u32 } // 16.16 取高 16 位
pub struct Stream { index, track_id, kind, codec: FourCc, timebase: Rational,
                    duration: Timestamp, video: Option<_>, audio: Option<_>,
                    extra_data: Vec<u8> }
pub struct Packet { stream_index, sample_index, dts: Timestamp,
                    pts: Option<Timestamp>, duration: Timestamp,
                    keyframe: bool, data: Vec<u8> }
pub struct BoxInfo / BoxNode;            // box 树（type/范围/子节点；含 moof/traf 子树）
pub enum Mp4Error { Io(std::io::Error),
                   Malformed { path: String, reason: String },   // 带 box 定位路径
                   Unsupported(&'static str),                    // 混合轨、多 sample description 等
                   InvalidStreamIndex { index, count },
                   SampleIndexOutOfRange { stream, sample, count } } // From<io::Error>

pub struct Mp4Reader<R: Read + Seek>;
impl Mp4Reader<R> {
    pub fn open(path) / from_vec(Vec<u8>) / from_reader(R) -> Result<Self, Mp4Error>;
    pub fn streams(&self) -> &[Stream];
    pub fn stream(&self, index: usize) -> Option<&Stream>;
    pub fn sample_count(&self, stream_index: usize) -> Result<usize, Mp4Error>;
    pub fn read_packet(&mut self, stream_index: usize, sample: usize)
        -> Result<Packet, Mp4Error>;   // 样例序号跨 fragment 统一编址
    pub fn box_tree(&self) -> &[BoxNode];
}
```

解析硬契约（摘要，全文见 crate `reader.rs` 头注）：

1. 整数一律大端；box 头 size u32 + type；size==0 → 延伸到父容器末，
   size==1 → largesize(u64)；size < 头长或越出父容器 → `Malformed`
   （错误信息带 `moov.trak[i].mdia.minf.stbl.<box>` / `moof[j].traf[k].<box>`
   定位路径）。
2. 递归容器 moov/trak/mdia/minf/stbl 与 moof/traf/mvex；未知 box
   （udta/edts/free/wide/mfra/tfra/mfro/sidx/styp/skip/…，顶层与容器内
   同样）按大小跳过；容器嵌套深度上限 64 层，超限 → `Malformed`。
3. stsd entry_count 必须为 1；`extra_data` 为第一个子 box 的载荷，
   按其声明 size 截取（size==0 ⇒ 到 entry 末；越出 entry → `Malformed`；
   avc1→avcC 即 AVCDecoderConfigurationRecord；mp4a→esds 含 4 字节
   版本/标志），**原样透传给解码器，本 crate 不解释**。
4. mdhd/tkhd 按 v0/v1 字段宽度解析；时基 `1/timescale`，
   时长经 `Timestamp::from_units` 换算到全局 tick。
5. 样例表：stts 段内 dts 加段内偏移×delta；ctts 缺省 ⇒ pts==dts；
   stss 缺省 ⇒ 视频全关键帧、音频恒关键帧；stco/co64 任一；
   stsc 跑段定位 chunk。读样例前校验 offset+size ≤ 文件长度，
   越界 → `Malformed`（防损坏 stsz 触发巨型分配）。定长 stsz 的
   locate 先按文件长度整体界校验（chunk 偏移 + chunk 内前序跨度
   O(1) 可算），防纯声明的巨型样例序号把逐样例求和拖成 CPU 挂起。
6. `Packet.data` 为容器存储原样——H.264 是 AVCC（长度前缀 NAL），
   **不转 Annex B**。

fMP4（fragment）硬契约（样例统一编址，`read_packet` 的序号跨
fragment 连续，`sample_count`=Σ(stbl+fragment) 样例数）：

7. mvex 按 track_id 关联 trak：mehd 可选（跳过）；trex 记录
   default_sample_description_index/duration/size/flags。moof 递归
   mfhd（跳过）/traf；traf 递归 tfhd/tfdt/trun，tfhd 须先于 tfdt/trun。
8. 基值解析：tfhd `0x000001` 置位 → 显式 base_data_offset；否则
   `0x020000` default-base-is-moof → moof 起点；否则 → 前一 trun 数据
   末尾（解析期按轨维护游标）；首个 traf 无任何基值信息 → 兜底取
   moof 起点。trun data-offset-present==0 → 样例数据接当前游标。
9. 样例字段优先链：trun 逐样例显式（duration `0x100`/size `0x200`/
   flags `0x400`/cts `0x800` 存在位）> tfhd 默认 > trex 默认；
   duration/size 全缺省按 0 容忍。长度界：解析期按「样例数据必须装进
   文件」校验——缺省 size 已知时 trun 数据总跨度（起点+n×size）须
   ≤ 文件长度；size 全缺省 0 无字节可依时按样例数 ≤ 文件长度兜底；
   越界 → `Malformed`（防无逐样例字段的 trun 声明巨量样例触发
   巨型分配 abort）。flags 另有首样例特例——首样例且
   trun 置 first-sample-flags-present（`0x004`）⇒ first_sample_flags
   覆盖该样例整个 flags；全缺省 ⇒ flags 未知。实测 ffmpeg 形态即
   first_sample_flags + tfhd 默认链（trun flags=0x00000a05、tfhd
   flags=0x00020038、tfdt v1、空样例表 moov + 顶层 mfra）。
10. keyframe = !(有效 flags & 0x00010000)（sample_is_non_sync_sample
    位；实测 ffmpeg default flags 0x01010000 被 ffprobe 标为非 K、
    first_sample_flags 0x02000000 标为 K，判定位即 0x00010000）；
    flags 全缺省 ⇒ true（与无 stss 表语义对齐）；音频轨恒 true。
11. tfdt v0(u32)/v1(u64) 均解析，为 fragment 首 dts（轨道单位，缺
    tfdt 则接前一 fragment 尾 dts）；trun v0 cts 无符号、v1 有符号。
12. 形态判定：无 moof → 纯 progressive 语义（mvex/trex 仅记录不参与，
    mvex+非空 stbl+无 moof 的文件正常打开读样例）；有 moof →
    fragment 样例接在轨样例序之后，`Stream.duration`=末 fragment 尾
    dts+dur（mdhd/mehd 为 0 时亦然）；同轨 stbl 非空样例又遇 moof
    样例 → `Unsupported("混合 progressive/fragmented 轨道")`。moov 内
    空样例表（entry_count=0，ffmpeg empty_moov 实测形态）在 mvex
    存在时合法。
13. hdlr 兼容两种布局：handler 紧随 version/flags（本 crate 早期手工
    测试字节、writer 的 pre_defined 双写）与 ISO 布局（pre_defined
    之后才是 handler_type，ffmpeg 实测产物）；mp4a entry 兼容 20 字节
    基础布局与 28 字节扩展布局（声道/采样率前多 8 字节，ffmpeg 产物
    与 writer 输出均为后者），按采样率/声道合理区间判别，extra_data
    从对应布局的固定字段之后取。

已知偏离（对原始任务书字面）：

- 任务书写 `Rational::new(1, timescale)`，ye-time 实际返回 `Option`，
  故 timescale 非法时归为 `Mp4Error::Malformed`。
- `Stream`/`Packet` 数据类型本轮定义在本 crate（与 architecture.md
  数据模型同名同义），M2 解码器消费时上提到 `ye-container` 共享定义。
- fMP4 任务书的关键帧判定位写作 `0x00100000`；实测 ISO/ffmpeg 的
  sample_is_non_sync_sample 位为 `0x00010000`（ffmpeg default flags
  0x01010000 被 ffprobe 标为非 K，其 0x00100000 位为 0），按
  `0x00010000` 实现并以 ffmpeg oracle 对拍验证。

## ye-cli（命令行层）

命令面（2026-10-27 集成阶段后）：`ye probe <FILE>`、`ye remux <IN>
<OUT>`、`ye convert <IN> <OUT> [--size 宽x高]`、`ye --help|-h`、
`ye --version|-V`。退出码约定：**0** 成功；**1** 运行失败（stderr 带
`ye: <错误>`）；**2** 用法错误（stderr 带用法全文）。参数解析手写零
第三方依赖；选项成体系后再评估 clap。

probe/remux/convert 全部走 `ContainerRegistry`（`ye_cli::registry()`
注册 MP4 工厂，输入 auto-probe ftyp/styp 嗅探）——命令面与未来容器
（MOV/MKV…）解耦；convert 另经 `ye_cli::codec_registry()` 注册 H.264
解码 + 编码真工厂。逻辑在 `ye_cli` lib（`parse_args`/`probe`/`remux`/
`convert`），`main.rs` 只做 argv 分发与输出，集成测试直接调 lib 并以
`CARGO_BIN_EXE_ye` 走真二进制端到端。

`probe` 输出契约（机器可 grep，勿随意改字段名与顺序）：

```
<路径>: mp4, <n> 路流
流 #<i> [视频] codec=<id> 时基=<num/den> 时长=<x.xxx>s (<t> tick) 样例数=<n> 帧率≈<f>（样例数÷时长近似，VFR 文件有偏差） 尺寸=<w>x<h> 码率=<bps>bps
流 #<i> [音频] codec=<id> 时基=<num/den> 时长=<x.xxx>s (<t> tick) 样例数=<n> 声道=<c> 采样率=<hz>Hz 码率=<bps>bps
流 #<i> [字幕] codec=<id> 时基=<num/den> 时长=<x.xxx>s (<t> tick) 样例数=<n> 码率=<bps>bps
```

- `codec` 为 `CodecId` 的 Debug 形式转小写（`h264`/`aac`，对齐 ffprobe
  命名；m1 期的 stsd 四字符码显示废止——原始 fourcc 经
  `Mp4Reader::track_info` 可查）。非 vide/soun 轨标签为 `[字幕]`
  （公开面映射 `MediaType::Subtitle` + `CodecId::Unknown`，移植裁决
  第 5 条）。
- `时长` 的 `tick` 为**流时基原生单位数**（精确值以它为准，秒数为
  三位小数显示值；m1 期的全局 tick 基数 7,056,000 废止）。
- 帧率/码率两列的口径与取舍见「锻的最小 API 面」。

`remux` 的行为契约上提到门面 `ye::container::remux` 文档（接线裁决
第 6 条），CLI 层是薄包装：输出布局 progressive、流序一一对应、
每轨整体平移到首包 dts = 0、包数据/关键帧/时长与 pts−dts 偏移原样
透传（H.264 保持 AVCC，不转 Annex B），成功输出单行
`<IN> → <OUT>: MP4, <n> 路流, <m> 样例直通（不转码）`。回归：
`ye-cli/tests/remux_e2e.rs`——PATH 上 ffmpeg 造双轨源 → 真二进制
`ye remux` → ffprobe 可读且流参数一致 → ye 两次解封装逐包对拍；
PATH 无 ffmpeg/ffprobe 即整组跳过，临时文件全落
`CARGO_TARGET_TMPDIR`（target/ 下，不入库）。

`convert`（2026-10-27 集成阶段真实现）：调门面 `ye::convert::convert`
（参数形态与轨道策略见上节「锻的最小 API 面」）——视频重编码 H.264
（`--size 宽x高` 控制缩放，值须非零偶数），音频包直通不重编码，输出
MP4；失败即删半成品输出文件；成功输出单行
`<IN> → <OUT>: MP4, <n> 路流, 视频 <m> 帧重编码（H.264 [WxH]）,
音频 <k> 包直通`。回归 `ye-cli/tests/convert_e2e.rs`——PATH 上
ffmpeg 造 3s 176x144 H.264(Baseline)+AAC 双轨源 → 真二进制
`ye convert --size 320x240` → ffmpeg 无错解码 + ffprobe 流参数
（视频 320x240、音频与源一致）+ 时长（±0.1s）与帧数（逐帧相等）
对拍；PATH 无工具即整组跳过，临时文件全落 `CARGO_TARGET_TMPDIR`。

## 锻的最小 API 面（M3 接入契约，2026-10-24 定形；convert 2026-10-27 集成阶段真实现，2026-10-26 M3 补齐增音频三态/剥视频/码率）

锻（duan）以 Tauri 后端 cargo 依赖接入，最小面 = `probe`（M1 已
可用）/ `remux`（M1 已可用）/ `convert`（**M3 集成阶段已真实现**：
视频重编码 H.264 + MP4 封装；音频三态——直通 / AAC-LC 重编码 /
剥轨；视频可剥〔`video = None`，纯音频输出〕）。调用形态三层：`probe`
在 `ye_cli` lib（`ye-cli/src/lib.rs`）；`remux` 在门面
`ye::container::remux`；convert 泵在门面 `ye::convert`
（`crates/ye/src/convert.rs`）——CLI 子命令即同一契约的命令行形态
（`ye convert <IN> <OUT> [--size 宽x高] [--vb kbps]
[--audio copy|aac:kbps] [--no-video]`）。

```rust
// ye_cli lib（ye-cli/src/lib.rs）
pub fn probe(path: &Path) -> Result<String, String>;    // 报告文本（含末尾换行）
pub struct ConvertOptions { pub size: Option<(u32, u32)>,
                            pub video_bitrate_kbps: Option<u32>,
                            pub audio: ConvertAudio, pub no_video: bool }
pub enum ConvertAudio { Copy, Aac { kbps: u32 } }        // --audio 的值域
pub fn convert(input: &Path, output: &Path, options: &ConvertOptions,
               progress: ConvertProgress<'_>, cancel: &AtomicBool)
    -> Result<String, String>;   // 失败即删半成品输出文件
pub type ConvertProgress<'a> = &'a dyn Fn(i64, i64);    // (processed_us, total_us)

// 门面（crates/ye/src/container/remux.rs）
pub fn remux(registry: &ContainerRegistry, input: Box<dyn ReadSeek>,
             output_format: &str, output: Box<dyn WriteSeek>)
    -> Result<RemuxStats, ContainerError>;              // RemuxStats { streams, packets }

// 门面 convert 泵（crates/ye/src/convert.rs，2026-10-27 集成定形——
// 无 feature 门：解码/编码器与容器均经注入注册表取得，泵本体不依赖
// 具体实现 crate；像素核同样注入，缺省核桥接 filter_ops()）
pub type ConvertProgress<'a> = &'a dyn Fn(i64, i64);    // 同上，µs
pub type PixelConvertFn = fn(&VideoFrame, PixelFormat) -> Result<VideoFrame, ConvertError>;
pub type ScaleFn = fn(&VideoFrame, u32, u32) -> Result<VideoFrame, ConvertError>;
pub struct PixelOps { pub convert: PixelConvertFn, pub scale: ScaleFn }  // 像素核槽（注入）
pub struct VideoChain { pub format: Option<PixelFormat>, pub size: Option<(u32, u32)> }
pub struct VideoEncodeSettings { pub chain: VideoChain, pub gop: u32,
                                 pub quality: QualityMode,
                                 pub bit_rate: Option<u64> }
    // Default：no-op 链 + GOP 250 + Crf(23) + 无码控；编码槽固定 H.264。
    // bit_rate = Some → 真码控（M3 补齐）：Qp 基准 → CBR 风味、
    // Crf 基准 → A-VBR（-crf c -maxrate b 对标），见 H.264 编码器
    // 输出契约
pub enum AudioEncodeSettings { Copy, Aac { bitrate_bps: u32 }, Drop }
    // 音频三态（M3 补齐）：直通（Default）/ AAC-LC 重编码（采样率/
    // 声道沿用源流，无重采样器）/ 剥轨
pub fn convert_frames(containers: &ContainerRegistry, codecs: &CodecRegistry,
                      input: Box<dyn ReadSeek>, chain: VideoChain, pixels: PixelOps,
                      sink: &mut dyn FnMut(Frame) -> Result<(), ConvertError>,
                      progress: ConvertProgress<'_>, cancel: &AtomicBool)
    -> Result<ConvertStats, ConvertError>;
    // 帧消费模式（M2 主路径）：固定链 解码→像素转换→scale→帧回调
pub fn convert(containers: &ContainerRegistry, codecs: &CodecRegistry,
               input: Box<dyn ReadSeek>, output_format: &str,
               output: Box<dyn WriteSeek>, video: Option<&VideoEncodeSettings>,
               audio: &AudioEncodeSettings,
               pixels: PixelOps, progress: ConvertProgress<'_>, cancel: &AtomicBool)
    -> Result<ConvertStats, ConvertError>;
    // 编码模式（M3 集成落地，M3 补齐扩展）：视频 解码→处理链→强制
    // yuv420p→H.264 编码→`output_format` 点名 muxer 封装；audio 三态
    // （Copy 每轨平移到首包 dts=0 / Aac 解码→凑 1024→编码→esds 模板
    // / Drop）；video = None 剥视频（纯音频输出，多视频流不再拒收）；
    // 剥光（无输出轨）显式 InvalidArguments；字幕轨丢弃；单视频流
    // （video 在场时多路显式 InvalidArguments）
pub struct ConvertStats { pub streams: usize, pub video_frames: u64,
                          pub audio_frames: u64, pub processed_us: i64, pub total_us: i64 }
pub enum ConvertError { Cancelled, Sink(String), InvalidArguments(String),
                        InvalidData(String), Container(ContainerError),
                        Codec(CodecError), Time(TimeError) }
pub fn filter_ops() -> PixelOps;
    // 像素核缺省桥接：ye-filter 核（yuv420p↔rgba8/rgb24 转换、
    // yuv420p/rgba8 双线性 scale）组装成 PixelOps
```

**固定链序（帧模式与编码模式共用前段）**：视频帧出解码器后依次过
「像素转换槽 → scale 槽」（目标与现状相同的环跳过、不调核），音频帧
帧模式解码后直通；编码模式在链尾再**强制 yuv420p**（H.264 编码器的
输入面——处理后仍非 yuv420p 的帧再过一次转换槽）后编码。**对拍口径
注意**：链序为 转换在前 scale 在后，与 ffmpeg 惯用
`-vf scale=W:H,format=rgba`（scale 在前）相反——E2E 对拍（剪抽帧）
的容差须覆盖该序差异，或参考侧改用 `-vf format=rgba,scale=W:H`。
通用图调度（`FilterGraph::topological_order`）已随 M3 处理图最小集
交付真实现（见「处理图最小集契约」），泵的图编排接入归后续批次。

**编码模式的轨道策略与时间线**：

- 视频轨（至多一路，多路 → `InvalidArguments`）：源帧率取流标称
  `VideoParams::frame_rate`，缺则按 `样例数 ÷ 时长` 精确有理数推导
  （i64 + gcd 归约，不用 f64），再缺省 25fps；输出时基 =
  `1/帧率分子`（帧时长 = 帧率分母 tick，整数无舍入——NTSC 等分数
  帧率下编码器打戳与 muxer 的 dts 衔接校验均精确）；输出包时戳由
  编码器按帧号重排（输入帧 pts 被忽略，「H.264 编码器输出契约」），
  首包恒 0、无重排（dts = pts）；
- avcC 取得：`Encoder` trait 无配置回读面，视频轨模板的 extradata
  由**首个 IDR 包的带内 SPS/PPS** 构造（消费编码器输出契约：IDR 带内
  SPS+PPS、AVCC 4 字节前缀；与 `ye-codec-h264` 的 `build_avcc`
  同构）——为此封装头**延迟到首个视频包产出**才落，此前的音频包先
  缓冲、落头后原序冲出；无视频帧的输入（纯音频）在 EOF 落头；
- 音频轨（任意路）：包数据/关键帧/时戳/pts−dts 偏移原样直通，每轨
  整体平移到首包 dts = 0（与 `remux` 同法；音频包缺 dts →
  `InvalidData`）；
- 字幕等轨道：丢弃（不进输出容器）；零帧视频轨不落输出轨。

**像素核槽（注入消费）**：转换/scale 的核是注入的纯函数
（`PixelOps`），核本体在 `ye-filter/src/video/`——泵只认函数签名、
不依赖核的模块路径；门面提供缺省桥接 `ye::convert::filter_ops()`
（`filter_convert`/`filter_scale` 两个公开桥接函数：yuv420p ↔
rgba8/rgb24 转换、yuv420p/rgba8 域双线性 scale，色彩范围取帧
`ColorDesc.range`、缺省 Limited）。剪（jian）的 `extract_frame`/
波形与单测（`crates/ye/tests/convert.rs`，conformance 向量驱动）
均走 `convert_frames`。

**进度约定**：`(processed_us, total_us)` —— 已处理/总时长，**微秒**；
与锻现有 `transcode-progress` 事件负载 `{jobId, timeUs, totalUs}` 同源
（`timeUs`/`totalUs` 直传回调值）。jobId 路由与 ≥100ms 节流归锻侧
（Tauri 事件层），ye 只提供进程内的每任务回调。帧模式**逐帧上报**
（帧过完链后）；编码模式**逐输出包上报**（视频包落盘后、音频包直通
后）；`processed_us` 单调不减（跨流取各流已处理时长的最大值，缺时戳
不推进）；`total_us` 取流时长（µs，缺时长 → 0）。CLI 形态即示范：
回调打 stderr（首报后每推进 ≥1s 一行），命令行无交互取消入口时
旗标恒 false。

**取消约定**：`cancel: &AtomicBool` 进程内取消旗标——锻侧置位即请求
取消，泵**每包检查点**（读出一包后立即检查，入口亦查），置位即返回
`ConvertError::Cancelled`；进程内等价于旧方案的 kill 子进程。编码
模式的**半成品清理**：泵持有 `WriteSeek` 抽象、无路径概念，失败时
封装器随所有权丢弃释放输出，输出文件的删除归调用方（ye-cli 已实现：
失败即 `remove_file`）。

**probe 的帧率口径**（仅视频轨）：`样例数 ÷ 时长（秒）` 近似——
恒帧率（CFR）文件准确，可变帧率（VFR）文件有偏差（逐帧时长不均时
平均帧率 ≠ 瞬时帧率），输出列自带注明「（样例数÷时长近似，VFR 文件
有偏差）」。仅显示用（f64）；标称帧率语义（`VideoParams::frame_rate`）
归容器/码流参数，不经此列。

**probe 的码率口径**：该流**样例字节总和 × 8 ÷ 时长（秒）**，四舍五入
到整数 bps（m1 期无此列）；一律由样例字节计算，不依赖容器声明字段。
为拿精确样例字节数，probe 会把解封装**排空一遍**（读全文件一次）——
「取精确值优先于快」的取舍已在 ye-cli lib 文档注明，M2 若证明太慢
再议增量口径。

## H.264 编码器输出契约（M3 T5，2026-10-26）

实现 crate：`ye-codec-h264`（`encoder` 目录，公共面 `H264Encoder`/
`encoder_factory`/`ReconFrame`/`level_for`）；门面注册
`ye::codec_h264::register_h264_encoder`（`h264` feature，与解码侧
`register_h264` 对称，替换语义）。范围 = Baseline/CAVLC、I/P 帧、
无 B 帧、单参考帧（m3-plan §5 裁决 4）。

- **配置面**（`ye_codec::VideoEncoderConfig`，公有字段结构）：M3 起
  增质量字段——`gop: u32`（IDR 间隔，帧数；1 = 全 I 帧，0 非法）与
  `quality: QualityMode`（`Qp(u8)` 恒定 QP / `Crf(u8)` 简单 CRF，值域
  均 **1..=51**——0 显式拒绝，形态收紧一律 `Err` 的库风）。真工厂
  交付即校验：codec 为 H264、`video` 在场、格式 yuv420p、宽高非零
  偶数、`gop ≥ 1`、质量值 1..=51、帧率与时基为正、尺寸×帧率不超
  level 5.2 限额——非法 → `CodecError::InvalidArguments`。
- **参数集写侧（定形）**：SPS = Baseline（profile_idc=66 +
  constraint_set0）+ `level_idc` 按 帧尺寸×帧率 查表取满足的最小值
  （`level_for`：H.264 表 A-1/A-3 子集，MaxFS/单边宏块数/MaxMBPS 三
  约束全整数交叉相乘；**不含 1b**——其 idc 与 1.1 共用且需
  constraint_set3 表达，限额本身相同，跳过不损失选择）；PPS 恒
  `pic_init_qp_minus26 = 0`（**QP 中立**：恒定 QP 与逐帧 QP 统一走片头
  `slice_qp_delta = qp − 26`，avcC 因此可在首帧编码前构造）；poc_type
  = 2（片头无 POC 语法）、单参考帧、CAVLC、无去块控制字段（解码器
  缺省全滤）。写出的参数集经自家 `Sps`/`Pps` 解析回环校验。
- **avcC 写侧**：`H264Encoder::extradata()` 构造即可用——avcC 记录
  （profile/compat/level 取自 SPS 头三字节、lengthSizeMinusOne=3 即
  4 字节前缀、SPS/PPS 各一条），`AvcConfigRecord::parse` 回环。
  `Encoder` trait 不携带配置回读面，带外参数经具体类型访问器取得。
- **包体形态**：`Packet.data` = AVCC（4 字节大端长度前缀 + NAL）；
  IDR 包带内含 SPS+PPS+IDR 片三个 NAL（裸流/换容器自足，与 avcC
  冗余但合法），P 包仅含片 NAL。IDR 包置 `PacketFlags::KEYFRAME`。
- **时戳**：输出包按 `EncoderConfig.time_base` 打戳——pts = 帧号 ×
  帧率倒数 重标度（`Timestamp::from_frame_index`），**输入帧 pts 被
  忽略**；B 帧缺席故 dts = pts（无重排）；duration = 一帧时长；
  `stream_index` 恒 0（流号归封装侧改写）。GOP：帧号 0, N, 2N, … 为
  IDR。
- **质量模式语义（M3 补齐后）**：`Qp(q)` 恒定 QP 直取；`Crf(c)` 为
  **启发式**简单 CRF——基 QP = c，按帧内活动度（luma 相邻差均值）加
  0..=4 的 ΔQP（复杂帧更粗，方向对齐 x264）。**真码控**
  （`EncoderConfig.bit_rate = Some(bps)`，2026-10-26 M3 补齐）：
  虚拟缓冲单程反馈（`encoder/rate.rs`，纯整数 i64——容量 2×bps 对齐
  锻 ffmpeg 引擎 `-bufsize 2b` 口径、目标水位半满、ΔQP 单帧 ±4 钳
  制、QP 1..=51）；`Qp` 基准 → CBR 风味（QP 自基值积分游走，平均
  码率趋近目标），`Crf` 基准 → A-VBR（游走仅在与活动度基 QP 取大时
  生效，对标 `-crf c -maxrate b`）；`bit_rate = None` 逐字节恒等
  于纯质量模式（回归锁定）；`Some(0)` 工厂拒收。实测：CBR 300k
  +9.3% / 800k −0.7%（±25% 容差带内不贴边），A-VBR Crf(18)+300k
  封顶落到无封顶的 35.6%。2-pass/完整 CRF 归后续批次。运动估计
  搜索范围固定 ±16px（可配归后续批次）。
- **重建快照**：本地解码环逐帧重建经 `H264Encoder::recon_frames()`
  暴露（`clear_recon` 释放）——位级回环验证与消费方预览共用，
  长编码内存代价由调用方自理。
- **状态机**：`flush` 后排空即 `Finished`（幂等），此后 `feed` →
  `Err(InvalidState)`；未 flush 队列空 → `NeedInput`（编码逐帧同步，
  flush 无滞留冲刷）。
- **错误映射**：编码侧 `H264Error::InvalidArguments` **保序透传**为
  `CodecError::InvalidArguments`（输入来自调用方——与解码侧把它归
  `InvalidData` 的选择不同，见 `decoder.rs` `codec_err` 对照）。
- **对拍口径（m3-plan §3 冻结，容差写死只准收紧）**：①回环位级一致
  ——本地重建 vs 自家 `H264Decoder`（avcC extradata + AVCC 包输入）
  逐像素 0 容差；②合法性双解码——自家解码帧数一致 + `avcc_to_annexb`
  转 Annex B 后 ffmpeg `-v error` 解码无错且 ffprobe 帧数一致
  （PATH 无工具打印跳过信息不 fail）；③Y-PSNR 逐帧下限——平滑三类
  向量族（纯色/渐变/运动方块）QP20 ≥ 36 dB、QP32 ≥ 28 dB；LCG 噪声
  只走①②不设门槛。回归 `crates/ye/tests/encode_h264.rs`（默认
  feature 集即运行，dev 依赖直连实现 crate）；门面注册路径回归在该
  文件 `#[cfg(feature = "h264")]` 组。
- **缺陷修复登记（2026-10-26）**：对拍暴露前向 DCT 蝶形行 2 笔误
  （`5·(t01−t23)` ≠ spec `[1,−1,−1,1]` 行）——任何含偶频率-2 能量的
  块系数量错、重建大幅偏离；位级回环因编码/解码两侧同错而不可见，
  PSNR 对拍（运动方块族）暴露。修复后以
  `fwd_dct4x4_matches_n_rows_matrix`（蝶形 == N·r·Nᵀ 逐元素）锁死，
  `satd_4x4` 同源笔误一并修正。

## AAC-LC 编码器输出契约（M3 补齐，2026-10-26）

实现 crate：`ye-codec-aac`（`encoder` 目录：bitwriter/psy/quant/
section/element，公共面 `encoder_factory` 与自由函数
`esds_extradata(sample_rate, channels)`）；门面注册
`ye::codec_aac::register_aac_encoder`（`aac` feature，替换桩语义）。
范围 = AAC-LC 长窗（ONLY_LONG、sine 窗、帧长 1024）、无 TNS/PNS/
M-S/短块（absence 位合法流）。

- **配置面**：`EncoderConfig` 的 `codec = Aac` + `audio` 在场 +
  `bit_rate = Some(bps)`（>0；**码率是唯一质量旋钮**，None 工厂
  拒收）；声道仅单/双（SCE / CPE〔common_window=1、ms_mask=0，两
  声道独立编码〕）；采样率限 ASC 标准 13 档；输入 packed f32
  （`SampleFormat::F32`）交错。1024 样本一包、逐帧同步（flush 收
  前视缓冲至多两包）；包 pts 按帧号 × 1024/采样率 重标度到
  `time_base`。
- **码流形态**：`Packet.data` = raw_data_block（裸访问单元，MP4
  esds 直填形态）；码率环从细侧向预算收敛、**FIL 填充元素按字节
  补齐到恒定码率**（低熵内容也满足码率门）；TDAC 前视 framing
  （交付帧 ∥ 下一帧），包 pts 与交付样本严格对齐。
- **带外参数**：`esds_extradata` 构造完整 esds 载荷（4 字节
  version/flags + ES_Descriptor 链，内含 ASC），MP4 `mp4a` stsd 的
  extradata 直填；自家解码器双形态（含/不含版本前缀）均可消费。
- **对拍口径**：①自家解码 PCM == 编码侧量化谱 IMDCT 逆变换（逐位
  一致）；②ffmpeg 解码我们的 ADTS 退出码 0 且帧数 = ceil(样本/
  1024)；③自家解码 vs ffmpeg 解码同一 ADTS 逐样本 100% |Δ|≤1e-5
  （实测 8.9e-8，按实测收紧）；④SNR ≥ 18dB（128k 立体声 AM 正弦，
  实测 28.2dB，跳过首 1024 priming 样本）；⑤码率命中目标 ±20%
  （实测 +0.5%/+0.2%）。工具缺席分级 skip（`tests/common` 三档
  定位器），在场失败不许 skip。回归
  `crates/ye-codec-aac/tests/encode_oracle.rs`。

## 合成导出最小集契约（M4 首轮，2026-10-26）

门面 `ye::compose`（`crates/ye/src/compose.rs`）：剪（jian）导出
接管的第一轨——**顺序拼接**。消费方接线：jian
`run_export_ye`/`export_ye_impl`（`src-tauri/src/export_ye.rs`，
双引擎之一；纯函数翻译层 `buildYeExportParams` 把「纯剪辑」时间线
翻成片段列表，带任何变换的时间线明确拒绝并指路 ffmpeg 引擎）。

```rust
pub struct ComposeSegment { pub input: Box<dyn ReadSeek>,
                            pub trim_in_us: i64, pub trim_out_us: i64 }
    // 源内裁剪窗口（µs，含头不含尾）
pub struct ComposeSettings { pub width: u32, pub height: u32,
                             pub frame_rate: Rational,       // 输出 CFR
                             pub gop: u32, pub quality: QualityMode,
                             pub bit_rate: Option<u64>,
                             pub audio: AudioEncodeSettings } // Aac/Drop（Copy 拒绝）
pub fn compose(containers: &ContainerRegistry, codecs: &CodecRegistry,
               segments: Vec<ComposeSegment>, output_format: &str,
               output: Box<dyn WriteSeek>, settings: &ComposeSettings,
               pixels: PixelOps, progress: ConvertProgress<'_>, cancel: &AtomicBool)
    -> Result<ConvertStats, ConvertError>;
```

行为契约（细节以 `compose.rs` 模块文档为正本）：

- **逐段校验**：恰一路视频流；`Aac` 时恰一路音频流且各段采样率/声道
  一致、在 AAC 能力面内；`Copy` 拒绝；违者 `InvalidArguments` 指明
  1 起段号（不做重采样/混音——归后续批次）。
- **视频 CFR 桶采样**（`CfrSampler`）：tick 数 = ⌈窗口×帧率⌉（i128
  精确有理数），每 tick 取 pts ≤ tick 时刻的最近帧（hold-last、段首
  无帧取首帧），全局唯一 H.264 编码器、输出 pts 全局连续从 0 起；
  窗口内零帧显式拒绝。
- **音频跨段连续累积** f32（窗口逐采样过滤），凑 1024 出帧喂全局
  唯一 AAC 编码器，末段补零；esds 复用 `convert::aac_esds`。
- **avcC 封装头延迟**：复用 convert 的 `HeaderState` 套路
  （`convert::avcc_from_idr` 回收，输出流号恒定 视频 0/音频 1）。
- **进度/取消/统计**：与 convert 完全同契约（逐输出包上报 µs 单调
  不减、total = Σ窗口；每输入包取消检查点；`ConvertStats` 口径）。
- **首轮取舍（登记归后续）**：无 seek 快进（窗口外帧解码后丢弃）、
  发射帧一次克隆、无段级静音填充、段间无转场。

对拍：`crates/ye/tests/compose.rs` 9 项（conformance 向量，缺席即
失败）+ jian `tests/export_ye_e2e.rs`（两段 3s 源各裁 2s →
320x240@15：ffmpeg 无错解码、时长/帧数形态、与 ffmpeg concat filter
对照五点位灰度帧平均 |Δ| 实测 0.75–1.62、容差 3.0 只准收紧）。

## 处理图最小集契约（M3 T6，2026-10-26）

实现 crate：`ye-filter`（`graph.rs`/`descriptor.rs`/`video/` 与错误
枚举），门面直达 `ye::filter`（`filter-stubs` feature 仅保留聚合入口
标记）。M3 处理图最小集交付：**拓扑排序真实现 + 滤镜参数形态定形 +
crop/fps/volume 三核**；scale 与像素格式转换复用 M2 既有核不重做。

- **`FilterGraph::topological_order`**：Kahn 算法真实现——入度 0 的
  节点按句柄升序入队、就绪即入队（FIFO），同图构造序下结果**唯一**；
  孤立节点按句柄序包含，空图返回空序；平行边按边计数。成环（含
  自环）→ `FilterError::Cycle(&'static str)`——原 `Unsupported` 变体
  随桩删除（无生产者一并移除，`FilterError` 现为
  `InvalidArguments`/`Cycle`/`Time` 三变体）。
- **图组装**（M2 既有，语义不变）：`add_filter`/`connect` 真实记边，
  节点/垫索引与两侧垫媒体类型一致性校验（失配 → `InvalidArguments`）；
  不去重（平行边合法）、不检测重复连接——环检测归拓扑排序。
- **滤镜参数 `FilterParams`**（`FilterDescriptor::params` 载荷，纯数据
  无不变式；不 derive `Eq`——`Volume` 的 f32 增益是数学参数非权威量，
  不属「f64 只用于显示」禁区）：
  - `Crop { x, y, width, height }`：yuv420p 平面域裁剪，四值须全偶
    （平面几何），违者执行核 `Err`；
  - `Fps { num, den }`：帧率重采样（每秒帧数），两者非零，违者 `Err`；
  - `Volume { gain }`：线性增益（1.0 恒等；dB 换算 `gain = 10^(dB/20)`，
    便捷函数 `volume_gain_from_db`），乘法 + [−1,1] 削波；
  - `None`：直通/参数承载归后续轮次（如 scale 目标尺寸）。
  参数校验归属执行核（消费时 `Result`），描述符层不校验。
- **旧 fps API 改造（裁决 9）**：`fps_nearest_frame_indices` 的
  `pts_us: &[i64]` 裸 µs 签名改为 `Timestamp` 入参并**删除旧签名**
  （跨时基比较走 `Timestamp::compare`；门面未导出、仓内零消费者，
  库未发版不留兼容包袱）。
- **对拍口径（m3-plan §3.5）**：crop（yuv420p 域纯字节搬运 0 容差）、
  fps（最近帧选择，对齐 ffmpeg `fps` 滤镜 round=near）、volume（定点
  口径容差 1 LSB 级）；PATH 无 ffmpeg 自动跳过。回归 `ye-filter` crate
  内测试组。

## M2+ 接口桩契约（2026-10-24 定形，2026-10-26 M3 收口更新）

M2+ 才交付的能力（解码/编码/处理图）先以**接口桩**占位：契约层
（trait、类型、注册表）现已定形并编译，实现缺席。桩让消费方现在就
能对接线与调用面做集成试验，真实现落地时只换注册的工厂、不动
调用方。

**三条语义边界**（ye-codec 与 ye-filter 一致，模块文档同源）：

1. **可构造**：桩工厂一律成功、不校验参数（尚无真实的初始化语义可
   校验）；构造出的对象自身属性可查（`codec_id()` 等）。真实现工厂
   **交付即校验**（对照面见「H.264 编码器输出契约」）。
2. **可查询**：注册后注册表可查——`supports_decoder`/
   `supports_encoder`/`decoder_ids`/`encoder_ids`；图结构
   （`node_count`/`edge_count`/`filter`/`edges`）可完整读回。
3. **调用即 Unsupported**：能力调用立即失败——`feed`/`pull` →
   `CodecError::UnsupportedStub(&'static str)`，文案**含交付去向标注**
   （供调用方与测试识别）；`flush` 无操作（实现交付前状态机无意义）。

**覆盖清单（2026-10-26 M3 补齐后）**：

- `ye-codec` 桩（`stubs` 模块，根级 re-export `register_stubs`）收窄为
  **两项**：H.264 解码 / AAC 解码（真实现已随 M2 交付，桩文案维持
  「桩：M2 交付」、仅接口占位用途）。**H.264 编码桩已删除**
  （`h264_encoder_factory` 消失）：注册位由 `ye-codec-h264` 的
  `encoder_factory` 接管（门面 `register_h264_encoder`）；**AAC 编码
  桩已随 M3 补齐删除**（`aac_encoder_factory` 消失）：注册位由
  `ye-codec-aac` 的 `encoder_factory` 接管（门面
  `register_aac_encoder`）——桩注册表上 `supports_encoder` 对两者
  均为假、`create_encoder` → `NotRegistered`，经真工厂注册后恢复。
- `ye-filter`：**处理图桩已随 M3 处理图最小集删除**——
  `topological_order` 为 Kahn 真实现、`FilterError::Unsupported` 变体
  移除（见「处理图最小集契约」）。

**门面 feature**：`ye` 的 `codec-stubs` / `filter-stubs`（默认关闭）
开启 `ye::stubs::codec` / `ye::stubs::filter` 聚合入口。桩本体随
`ye-codec` 无条件编译；`ye-filter` 的处理图本体同样无条件编译且**已
无桩**——两 feature 的作用是把「本构建经聚合入口依赖桩面/处理图」
显式记进依赖声明（防桩悄悄进产品路径，替换时移除 feature 即得全仓
检索点）。回归 `crates/ye/tests/stubs.rs`（`cargo test -p ye
--features codec-stubs,filter-stubs`；默认 feature 集不含，不进
workspace 门槛）——断言随收窄更新：H.264 编码在桩注册表上不可查、
经真工厂（dev 依赖直连）注册后可查且校验参数；处理图组装 + 拓扑
排序真行为经聚合入口成立。

**替换路径（更新）**：H.264 解码/编码、处理图调度、AAC-LC 编码均
已完成真实现替换（M2/M3/M3 补齐）；桩面仅剩 H.264/AAC 解码占位
（真实现早已在场，桩只为接口试验保留），`register_stubs` 收窄为
解码×2（2026-10-26 M3 补齐）。

## 测试基线（M3 补齐后，2026-10-26）

`cargo test --workspace`（默认 feature 集，ffmpeg/ffprobe 在 PATH 的
环境实跑全绿），共 **677** 项：

| crate | 测试 | 说明 |
| --- | --- | --- |
| ye-time | 28 | 整除性/往返/舍入/溢出/`i64::MIN` 边界 |
| ye-pixfmt | 28 | 元数据表/布局公式/紧密切片/错误 |
| ye-codec | 40 | trait/注册表/参数/包/帧 + 桩语义（M3 补齐收窄为解码×2，编码桩删除） |
| ye-codec-h264 | 188 | 码流层 + 解码主链（M2）+ 编码写侧 T2–T5：BitWriter/CAVLC 写侧位级回环、I/P 帧黑盒回环 0 容差、level 查表/avcC 回环、`fwd_dct4x4 == N·r·Nᵀ` 锁死、Encoder 配置校验/状态机/打戳 GOP、conformance 2 项；**M3 补齐 + 码控 12 项**（rate.rs 单元 + CBR 收敛 ±25%〔实测 +9.3%/−0.7%〕/单调性/A-VBR 封顶/None 逐字节回归，`tests/encode_h264.rs`） |
| ye-codec-aac | 69 | AAC-LC 解码主链（M2）+ conformance 单元；**M3 补齐 + 编码 25 项**（码表逐符号回环、ASC/esds 回环、编码→自家解码位级一致、码率环 + FIL 补齐、ffmpeg oracle：双解码一致 100% |Δ|≤1e-5〔实测 8.9e-8〕/SNR 28.2dB/码率 ±20%，`tests/encode_oracle.rs`） |
| ye-conformance（辅助 crate） | 26 | 向量清单/校验和/获取脚本 |
| ye-container | 26 | 契约 trait/注册表/探测 |
| ye-container-mp4 | 111 | 读取端黄金/变体/错误/回归 + fMP4 读全矩阵 + Mp4Writer progressive/fMP4 + adapter 接线 + ffmpeg/ffprobe oracle 组（PATH 无工具跳过） |
| ye-filter | 70 | 像素核（M2 scale/转换）+ 处理图最小集（M3：拓扑排序 DAG/环/孤立节点、Crop/Fps/Volume 参数与核、`Timestamp` 化 fps API、oracle 对拍组） |
| ye-png | 22 | PNG 编码器（M2）+ oracle 组 |
| ye（门面） | 36 | remux 回归 3 + convert 泵 12（帧模式 7 + 编码模式 4：重编码+缩放全链回读/音频逐包直通对拍/入口取消/编码器未注册）+ convert 单元 6（设置缺省含音频三态缺省/帧率推导含 NTSC/avcC 构造与失败面/scale 桥接恒等）+ 编码收口对拍 `encode_h264.rs` 11 项（第 11 项为 `h264` feature 门面路径——workspace 运行时经 ye-cli 的 feature 声明统一开启）+ **M3 补齐 `convert_audio.rs` 4 项**（纯音频重编码自家回环/双流剥视频丢包回归/剥光守卫/码率接线冒烟）（workspace 统一口径；`-p ye` 单独运行时 feature 门组不计） |
| ye-cli | 17 | probe 报告/参数解析（含 M3 补齐旗标面）/二进制端到端 9 + remux e2e 4 + convert e2e 2（M3：`--size` 全链；**M3 补齐：`--no-video --audio aac:96` 纯音频**——ffmpeg 无错解码/单 aac 轨/时长对拍/码率容差；PATH 无工具跳过） |

另有非默认 feature 组：`cargo test -p ye --features
codec-stubs,filter-stubs`（桩聚合回归，含收窄断言）。单元层全为手工
构造字节/确定性合成向量（LCG）；oracle 组用 PATH 上的 ffmpeg/ffprobe
造源对拍，找不到工具即打印跳过信息返回（不 fail；ffmpeg 在而
conformance 向量缓存缺席按失败——门禁不许假绿），临时文件全落
`CARGO_TARGET_TMPDIR`（target/ 下，不入库），零网络、零第三方依赖。
PSNR/对拍门槛写死为测试常量、只准收紧（m3-plan §3 冻结口径）。

## ye-container-mp4 Mp4Writer（封装，模块二 + 模块三 fMP4）

> **时戳域已被移植裁决第 2 条取代**：写入契约的时戳换算（全局 tick →
> `Timestamp::to_units`）现为自描述包 `Timestamp::rescale`（默认
> `Rounding::NearInf`，对齐 `av_rescale_q`）；mvhd/tkhd 换算同
> NearInf，`fragment_duration` 阈值走 `Timestamp::compare`。其余写入
> 契约（布局/样例表/校验链/fMP4 语义）存活有效，签名块以源码为准。

对标 libavformat mov muxer 的 progressive 子集；模块三在其上落地
`Layout::Fragmented`（fMP4 写入，语义见本节末尾）。

```rust
pub enum Layout { Progressive, Fragmented }
pub struct Mp4WriterOptions { pub layout: Layout, pub free_size: u64,
                              pub force_co64: bool,
                              pub fragment_duration: Option<Timestamp> }
                              // Default: Progressive / 0 / false / None
pub struct TrackConfig { pub kind: MediaKind, pub codec: FourCc,
                         pub timebase: Rational, pub video: Option<VideoInfo>,
                         pub audio: Option<AudioInfo>, pub extra_data: Vec<u8> }
impl TrackConfig { pub fn from_stream(s: &Stream) -> TrackConfig; }
pub enum WriterError { Io(std::io::Error), Unsupported(&'static str),
                       InvalidStreamIndex { index, count },
                       Misuse(&'static str) }        // 含 Display/Error/From<io::Error>

impl Mp4Writer<BufWriter<File>> {
    pub fn create(path, options) -> Result<Self, WriterError>;
}
impl<W: Write + Seek> Mp4Writer<W> {
    pub fn new(w: W, options: Mp4WriterOptions) -> Mp4Writer<W>;
    pub fn add_stream(&mut self, cfg: TrackConfig) -> Result<usize, WriterError>;
    pub fn write_packet(&mut self, pkt: &Packet) -> Result<(), WriterError>;
    pub fn flush_fragment(&mut self) -> Result<(), WriterError>;  // 仅 Fragmented
    pub fn finalize(self) -> Result<W, WriterError>;   // progressive 写 moov、回填 mdat 头；
                                                       // fragmented 冲尾 + 回填前置 moov 时长
    pub fn stream_count(&self) -> usize;
}
```

- **构造约定（硬性）**：任何代码（含全部测试）构造 `Mp4WriterOptions`
  一律 `Mp4WriterOptions { layout: .., ..Default::default() }` 结构更新
  语法，禁止全字段字面量——后续加字段不破编译。形状本模块一次性定形
  （含 `fragment_duration`），模块三不再改 struct 形状。
- 布局：`ftyp → free(free_size>0 时才有；上限 u64::MAX-8，8+free_size
  不得溢出 u64，超出在首个写出点报 Misuse) → mdat(恒 16 字节 size==1+
  largesize 头，封装期占位、finalize 回填真实总长) → moov`。mdat 头长
  恒定 ⇒ chunk 偏移写入即定、免回填位移，>4 GiB 天然正确。
- 样例表：单 stsd entry（avc1→avcC、mp4a→esds，`extra_data` 原样回写
  不解释）；stts run-length 合并（delta 一律取单位域 dts 差，尾包取
  duration）；ctts v1 仅当存在 pts≠dts（负偏移合法）；stss 仅视频轨且
  非全关键帧；stsz 全等且非 0 定长否则变长（sample_size=0 是变长表
  标记，全空包轨必须走变长表逐样例写 0）；同轨文件内连续样例自动合一
  chunk（stsc 段最小）；任一 chunk offset 超 u32::MAX 或 force_co64 ⇒
  全文件 co64。
- 时戳换算：packet 的全局 tick 时戳入表前一律换到轨道时基单位域
  （`Timestamp::to_units`），校验与 delta 都在单位域进行；timescale ≤
  7,056,000 时 tick→单位→tick 往返在单位域严格无损（half-away 舍入
  互相抵消），tick 域严格相等仅当 timescale 整除 7,056,000。
- write_packet 写入契约（违反即拒，不落半字节数据）：
  1. `pkt.stream_index` 越界 → `InvalidStreamIndex`；
  2. 首包已写后再 `add_stream` → `Misuse`；
  3. 同轨 dts 单调不减（单位域）；`dts < 0` → `Misuse`（固定文案
     “负 dts 不可表达（stts 0 起/tfdt u64），请调用方整体平移”）；
  4. 单位域 `dts[i+1] != dts[i] + duration[i]` → `Misuse`（固定文案
     “dts 间隙/重叠与 duration 不一致，M1 不自动补样例”）；
  5. `duration < 0` → `Misuse`；`duration == 0` 合法（delta 0）；
  6. 首包 dts 须为 0（模块二补充规则：stts 隐式从 0 起算，非零首偏移
     无 elst 不可表达，请调用方整体平移）；
  7. timebase 须 `num == 1`（den 即 mdhd timescale）；kind 仅
     Video/Audio 且须带对应 video/audio 信息（缺/错配 → Misuse）；
     codec 仅 avc1（Video）/mp4a（Audio），其余 → `Unsupported`；
  8. finalize 时 0 流 → Misuse（0 包轨合法，产出空表兼容文件）。
- hdlr 与 AudioSampleEntry 兼容性注记（2026-10-24 评审更新：读取端
  已按硬契约 13 双布局兼容，下述「缺陷/失真」为历史状态，现为兼容
  保留）：①hdlr 的 handler 四字符双写在 pre_defined 与 handler_type
  两个槽位——读取端 `parse_hdlr` 现已跳过 version/flags 与
  pre_defined（reader.rs 双布局兼容），规范槽位直接命中；双写仅作
  历史兼容保留，读取端两代布局都读到正确 handler。②mp4a entry 按
  ISO 写 28 字节固定字段（SampleEntry 基座 8 + reserved u32[2] 8 +
  声道/样宽/pre_defined/reserved 8 + 采样率 16.16 在 +24）；读取端
  `extract_entry_fields` 现按 20/28 字节双布局判别（本文件 box 硬契约
  13，ffmpeg 28 字节产物实测可读），声道/采样率/extra_data 读回无
  失真（tests/writer.rs 的 mp4a 用例与 oracle_mux 即其回归验证）。
- pub(crate) 组装件（模块三 fMP4 组装的编译契约，签名不得偏离）：
  `BoxBuf`（大端追加 + `into_box`/`into_box64`；注意两者取 `self`
  按值，不能接在返回 `&mut Self` 的追加链尾）、`build_ftyp`、
  `validate_timebase`、`build_stsd_entry`、`build_mvhd`、
  `build_trak_skeleton`（duration_units 写 mdhd 轨道时基域，
  tkhd_duration_units 写 tkhd movie 时基域（1/1000，ISO 14496-12 §8.3.2，
  2026-10-24 评审拆分两参数——此前同值直写轨道单位，timescale≠1000
  的轨错报时长）；模块三传空样例表 + >u32::MAX 占位 duration 复用
  （两参数同传占位值，强制 v1 定宽，见 Layout::Fragmented 节）。
- 测试：`tests/writer.rs`（自包含单元层：布局黄金/extra_data 回写/
  交错分 chunk/ctts/stss/co64/Misuse 全清单/0 包轨/时基换算）；
  `tests/oracle_mux.rs`（ffmpeg lavfi 造 avc1+mp4a 源 → reader 读 →
  writer 重封装 → ffprobe 字段一致 + 逐包 roundtrip（单位域严格相等，
  整除时基另作 tick 域断言）+ flags=K 对拍 + force_co64 可读 + 解码
  冒烟；PATH 无 ffmpeg/ffprobe 时打印跳过信息返回）。
  模块二原 `unsupported_fragmented_layout` 用例（Fragmented 一律
  Unsupported）已随模块三行为落地删除，fMP4 行为由 `tests/writer_fmp4.rs`
  与 `tests/oracle_fmp4.rs` 接管。

### Layout::Fragmented（fMP4 写入，模块三）

布局：`ftyp → 前置 moov（mvhd + trak×N（build_trak_skeleton 传完整
stsd + 空样例表 stts/stsc/stsz/stco）+ mvex（trex×N，track_id 按
add_stream 顺序，缺省字段全 0））→ (moof + mdat)*`，free 不写
（`free_size` 仅 Progressive 生效）。

- **flush_fragment**：把已攒样例切出为一个 moof+mdat 对；无待切样例 ⇒
  no-op（不产空 moof）。`finalize` 自动冲尾；Progressive 布局调用 ⇒
  `Misuse("flush_fragment 仅 Fragmented 布局可用")`。样例数据攒在内存
  （moof 前置 ⇒ 数据须等 moof 定长后才可落盘），mdat 载荷按轨序拼接
  （轨内连续 ⇒ 每 traf 单 trun 的 data_offset 走读成立）。
- 每 fragment：`mfhd`（sequence_number 从 1 递增）+ 每有 pending 数据的
  轨一个 `traf`：
  - **tfhd** 恒置 default-base-is-moof（`0x020000`，与读取端/ffmpeg 的
    实现位一致）+ sample-description-index-present（`0x000002`，值 1），
  不写 base_data_offset；trex 已兜底全 0 默认，不再写 tfhd 级默认值。
  - **tfdt** 恒 v1（u64）= 本 traf 首样例 dts（媒体单位；write_packet
    契约保证 ≥0——Fragmented 下首包 dts 允许非 0，基值由 tfdt 承载，
    progressive 的「首包 dts 须为 0」规则不适用）。
  - **trun** 恒 v1（flags `0x00000F01`）且 per-sample 显式
    duration/size/flags/cts：duration 取 dts 差（尾包取 pkt.duration，
    衔接校验下两者同值）；flags 关键帧 `0x02000000` / 非关键帧
    `0x01010000`（读取端判定位 `0x00010000` 清零即关键帧）；cts =
    pts−dts（单位域，可负，v1 有符号）。first_sample_flags 不用——首样例
    也写 per-sample flags，与读取端优先链兼容。
- **data_offset**（i32）= 该轨数据起点 − moof 起点；moof 在内存两遍拼装
  （字段定宽）后落盘，第二遍回填。mdat 恒 16 字节 size==1 + largesize 头
  （flush 时总长已知直接写实值；延续 progressive 策略）。moof 用 u32 size。
- **fragment_duration**（`Option<Timestamp>`）：Some(t) 时新到的
  keyframe 包在本轨待切跨度（tick 域）≥ t ⇒ 自动先 flush 再攒（片段
  起点落在关键帧上，音频轨 keyframe 恒真即按跨度切）；None ⇒ 仅手动
  flush / finalize 冲尾。
- **finalize（Fragmented）**：冲尾 fragment 后回填前置 moov 的
  mvhd/tkhd/mdhd duration——moov 写出时以 >u32::MAX 占位值强制 v1
  （u64 定宽 ⇒ 偏移恒定），从 box size 逐层定位字段后原位改写；
  mdhd = 轨道时基单位跨度（末样例尾 dts+dur − 首 dts），tkhd/mvhd =
  tick 跨度换算 movie 时基（1000）。0 包轨合法：产出仅 ftyp+moov 的
  可读文件，时长回填 0。
- 写入契约：模块二 write_packet 契约全部沿用（负 dts / duration < 0 /
  同轨单位域单调不减 / dts 间隙与重叠 / 首包后 add_stream / 流索引越界 /
  timebase 与 codec 校验一致）；跨 fragment 的衔接由 flush 后的单位域
  游标继续校验（回退 ⇒「同轨 dts 须单调不减」、跳变 ⇒「dts 间隙/重叠
  与 duration 不一致」）；trun 字段宽度另有限定（duration ≤ u32、
  |pts−dts| ≤ i32，越界 Misuse）。
- 测试：`tests/writer_fmp4.rs`（单元层：黄金字节/多 fragment
  tfdt·mfhd/fragment_duration 自动切/冲尾与空 flush no-op/音视频交错
  roundtrip 含负 cts/Misuse/校验链）；`tests/oracle_fmp4.rs`（ffmpeg 造
  default_base_moof 与显式 base_data_offset 两种 fMP4 源 → Fragmented
  重封装 → ffprobe streams/packets 单位域严格对拍 + flags=K + 解码冒烟
  + moof≥2；skip 规则同前）。


## CABAC 解码收口（2026-10-31）

- 范围：H.264 progressive、8-bit、4:2:0 的 High/CABAC，全 I、I/P、B 金字塔；包含 8×8 变换、双列表参考、空间直接预测、隐式双向加权与 B 帧去块。
- `cabac_oracle` 自动生成五组 45 帧素材（全 I、I/P、B、B 禁用去块、B 禁用加权），Y/Cb/Cr 三平面逐帧 **bit-exact** 对比系统 FFmpeg；工具在场时生成、解码或比对失败均报错。
- 临时 bin 注入、变异开关、残帧放行与全局落盘钩子已删除，产品解码不得绕过不完整图片检查。
- 这是库内上述回归范围的收口，不代表所有 High 形态均支持；10-bit、4:2:2/4:4:4、隔行与显式 B 加权仍不在本次交付范围。剪/锻的导入放行和调用侧验证另行推进。

## AAC PNS（联动 P1）

AAC-LC 解码支持码本 13：独立 noise scalefactor 差分链、逐窗逐带能量归一、解码器私有可复现随机状态、CPE 共窗 M/S 标记下相关噪声。非 PNS 容差不变。PNS 用长/短窗频带能量、立体声相关性和真实 44.1/48kHz 素材总能量（与 FFmpeg 相差不超过 1dB）验收；不要求随机样本与其他解码器一致。编码器仍不生成 PNS。


## 联动 P1：色彩与显示时间

- SPS VUI 的范围、原色、传递函数和矩阵传至输出 VideoFrame；RGBA 转换显式支持 BT.601/709 与有限/全范围。BT.601 原舍入口径保留；未知矩阵不得冒充 601。
- MP4 原始读取器仍返回样本表时间；Demuxer 对 packet PTS/DTS 及 seek 同步应用常见 elst 映射（一个 rate=1 媒体 edit，前面可有一个空 edit），复杂重排或非 1 播放率明确拒绝。编辑列表写入 stream 的 mp4.edit.* 元数据；音频 priming 样本尚不裁掉，波形原始采样口径不变。
- High/B 帧、30000/1001 帧率的五个抽帧时点与 FFmpeg 比对通过；BT.709 两种范围的 RGBA 最大通道差不超过 2。仍须推进音频对齐和编辑时长消费，不能据此宣布整个 P1 完成。
# 联动输出时钟（P2）

`convert_with_options` 保留 `convert` 的原有默认入口，增加可选有理数目标帧率及 `AudioOutput`（44.1/48kHz 单/双声道）。改帧率按窗口内显示时刻 hold-last，帧数为窗口时长乘目标帧率向上取整。音频输出启用时以时间零为共同原点，负时间 priming 丢弃、正时间空隙分块填零；32-tap sinc 重采样，输出有效样本数按输入样本数与采样率比向上取整，编码末块仍补到 AAC 的1024样本。MP4 呈现时长通过下述 elst 裁尾记录，原始包仍含编码填充。

MP4 Demuxer 头的 duration/start_time 现应用常见编辑列表；原始 Mp4Reader 保留原样本时钟。编码注册工厂关闭重建历史缓存；具体 H264Encoder 的测试访问器保留原行为。
# 静态时间线渲染（联动 P3）

`timeline::render_frames` 输出编码前 RGBA 和 48kHz 双声道 PCM；`timeline::render` 复用既有编码封装泵。每个素材出场有独立游标；视频取显示时刻不晚于输出 tick 的最近帧，keyframe seek 后解码并重排。窗口含头不含尾，源区间与时间线区间等长。没有视频层的 tick 输出黑场，没有音频的采样输出静音。

变换为画布中心偏移 x/y、源中心锚定、等比 scale、顺时针角度及 source-over opacity；z 增大向上。音频 gain 0–2，pan -1–1 采用左右平衡（中心两侧不变，极端关闭对侧），相加后削波，不做归一化。mute/solo 在消费方唯一求值层解决。每路仅保留相邻视频帧或少量 PCM 块；源音频支持44.1/48kHz单/双声道，32-tap sinc 插值并以源时戳对齐。

输出 RGBA 经 BT.709 limited 矩阵转换，编码首个 SPS 携带色彩 VUI。输入 BT.601/709 与 full/limited 按帧色彩转换；HDR 与未知矩阵明确拒绝。首版不处理关键帧、变速、效果、转场、图片；消费方 DTO 后端须列出这些阻塞项。

MP4 mux 模板可带 `mp4.presentation.duration_ticks`，写 v1 elst，并采用882MHz movie时钟精确表达微秒和44.1/48kHz样本。底层样本表保留编码包时长，Demuxer 的呈现时长裁去填充；使用原始 Mp4Reader 的调用方仍须自行解释编辑列表。旧模板未设置该字段时不改变封装。

## 呈现波形与源帧率

`audio_peaks_aligned_with_count` 只解码所需音轨，依据编辑列表与帧 PTS 排除 priming、保留开头静音。按已知呈现时长固定分桶，仅保存当前 PCM 块及桶累计器，内存不随素材长度增长。旧 `audio_peaks_with_count` 保留原始样本口径供历史调用与对拍。带250ms音频起点偏移的素材与 FFmpeg `-copyts` 参考比对使用原非PNS容差1.5e-4。

Demuxer 在应用呈现裁尾前从原始样本表计算 frame_rate。禁止用裁过最后半帧的呈现时长反推编码帧率，否则24fps成片会被误识别成24.975fps。

`convert::check_convert` 在不打开输出的情况下检查实际流、目标时钟与编解码器工厂；任务执行仍重新建轨校验。`timeline::check_timeline` 聚合窗口与静态参数问题，解码能力由 `media::validate_decoding` 检查。预检不能替代逐包解码来证明素材没有损坏。

seek 的目标为显示时间，返回落地关键帧的 DTS。MP4 先二分 DTS 找候选，再检查关键帧 PTS；下一个 GOP 可能提前解码但尚未到显示时刻，此时必须回退到上一个 GOP。跨 GOP 的非关键帧裁剪与视频延迟200ms/音频延迟100ms的共同原点回归分别验收像素、PCM及转码进度总时长。


### 精确时钟与任务所需轨道

CFR 帧数与选帧使用原始有理数时间，不先舍入到微秒。时间线偏移通过精确和式比较处理，避免分数帧时基与微秒时基公分母超出 i32。DTO 的完整源区间允许末端微秒舍入；音频逐采样限于 edit list 的有效窗口，不能泄漏 AAC 填充样本。纯音频转码立即写出输出轨头，不等待不存在的视频包。

抽帧和时间线共用显示时间 seek 与所选流游标。抽帧只创建视频解码器，无关音轨不受支持不会阻塞抽帧；视频自身的失败原样返回。通用转码检查和音频任务仍校验其实际需要的轨道。本文早期抽帧“整体解码并拒绝不可解音轨”的契约由此替代。


### 显式源流选择

TimelineClip.stream_index 是容器的实际流索引，不是声道或同类型流的序号。Some(index) 必须存在且类型匹配图层；None 只接受该类型恰一条流，绝不静默选择第一条。select_stream 为调用侧预检和执行共用校验。音频波形的 audio_peaks_aligned_stream_with_count 同样接受显式流索引；原有无索引接口保持歧义拒绝行为。立体声一条源流对应一条编辑音轨，声道布局仍保留。

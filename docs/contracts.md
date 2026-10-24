# 接口契约

按 [architecture.md](architecture.md) 演进规则维护：每一层落地时，把其对外
接口契约补记到本文件；改契约必须同步改这里，本文与代码不一致以最近一次
合入的代码为准并须立刻修正其一。

本文记录 M1 已落地的各层：门面/feature 接线、`ye-time`、`ye-pixfmt`、
`ye-container-mp4`（读取端 + 封装端 `Mp4Writer`），外加 `ye-cli`
（`probe` 输出与 `remux` 直通）的契约。

> 2026-10-24 合流后：本文 m1 侧各节（`ye-time` / `ye-pixfmt` /
> `ye-container-mp4` / `ye-cli` 及 feature 链）描述的是并入待重写的
> m1-mp4 实现，其与 develop 契约层的冲突以下节裁决为准。

## 合流裁决记录（2026-10-24，B 案）

m1-mp4（MP4 全功能实现）合入 develop（契约层）。裁决：**以 origin/develop
的契约层为主体，m1-mp4 的实现重写挂到 trait 后再恢复编译**。文件级：
`ye-time` / `ye-pixfmt` / `ye-container` 的 `lib.rs` 与 `Cargo.toml`
develop 版胜出；`ye-cli` 以 m1 版为底；`Cargo.lock` 重新生成；
`docs/roadmap.md` 双方内容都保留；`ye-container-mp4` 整目录并入、
暂从 workspace members 排除（根 `Cargo.toml` `exclude`）。

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

## 门面与 feature 接线

依赖方向是硬规则，防止抽象层倒挂：

1. 创作工具（jian/duan/yue）只依赖门面 `ye`，不直接依赖任何 `ye-*` 实现 crate。
2. 具体实现 crate（如 `ye-container-mp4`）挂在对应抽象层
   （`ye-container`）的 optional 依赖 + **同名 feature** 下，由抽象层
   `pub use ... as <名>;` re-export。
3. 门面 `ye` 用同名 feature 转发开启，并在 `ye/src/lib.rs` 的 feature 表
   汇总；`ye-cli` 显式声明所需 feature（`features = ["mp4"]`），
   不依赖 default 的默认值。

当前 feature 链（M1 第一轮）：

```
ye/mp4  →  ye-container/mp4  →  dep:ye-container-mp4   →  ye::container::mp4
（默认开启）
```

关闭 `mp4`（`ye = { default-features = false }`）时 `ye::container::mp4`
不存在；`ye-cli` 因显式声明 feature 而不受门面 default 变更影响。

## ye-time（基础类型层）

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

命令面（M1）：`ye probe <FILE>`、`ye remux <IN> <OUT>`、
`ye --help|-h`、`ye --version|-V`。
退出码约定：**0** 成功；**1** 运行失败（I/O、解析错误，stderr 带
`ye: <路径>: <错误>`）；**2** 用法错误（stderr 带用法全文）。
参数解析手写零第三方依赖；M3 `convert` 选项成体系后再评估 clap。

`probe` 输出契约（机器可 grep，勿随意改字段名与顺序）：

```
<路径>: MP4, <n> 路流
流 #<i> [视频] codec=<四字符码> 时基=<num/den> 时长=<x.xxx>s (<tick> tick) 样例数=<n> 尺寸=<w>x<h>
流 #<i> [音频] codec=<四字符码> 时基=<num/den> 时长=<x.xxx>s (<tick> tick) 样例数=<n> 声道=<c> 采样率=<hz>Hz
流 #<i> [其他] codec=<四字符码> 时基=<num/den> 时长=<x.xxx>s (<tick> tick) 样例数=<n>
```

- `时基` 是 `mdhd:timescale` 的倒数（`1/1000` 形式）；`时长` 秒数为显示值
  （三位小数），精确值以 `tick` 数为准（tick 基数 7,056,000）。
- 逻辑在 `ye_cli` lib（`parse_args`/`probe`），`main.rs` 只做分发与输出，
  集成测试直接调 lib 并以 `CARGO_BIN_EXE_ye` 走真二进制端到端。

### `ye remux`（M1 收尾新增）

MP4 解封装 → 封装直通（对标 `ffmpeg -c copy`，不转码）：

- 输出布局 progressive（`ftyp → mdat → moov`）；包数据、关键帧、
  duration 与 pts−dts 偏移原样透传（H.264 保持 AVCC，不转 Annex B）。
- 流映射：按 reader 流序 `add_stream(TrackConfig::from_stream)`，流索引
  一一对应；kind 非 Video/Audio 或 codec 非 avc1/mp4a 的轨 → 运行失败
  （退出码 1，沿用 writer 的 `Unsupported`/`Misuse` 文案）。
- 交错：各轨按样例序游标推进，全局按 `(dts tick, 流号)` 归并写入
  （轨内序保持；同轨连续样例合 chunk 由 writer 处理）。
- 时戳：progressive 的 stts 从 0 起算（无 elst），每轨时间线整体平移到
  首包 dts=0（平移量 = 该轨首包 dts；常见文件为 0 即 no-op）。轨内相对
  时序与时长不变；跨轨起始差不保真（progressive 无法表达）。
- 成功输出（stdout，单行）：`<IN> → <OUT>: MP4, <n> 路流, <m> 样例直通（不转码）`。
- 回归：`ye-cli/tests/remux_e2e.rs`——PATH 上 ffmpeg 造双轨源 →
  真二进制 `ye remux` → ffprobe 可读且流参数与源一致 → ye 两次解封装
  逐包对拍（数据字节严格一致、时戳单位域严格相等）；PATH 无
  ffmpeg/ffprobe 即整组跳过。临时文件全落 `CARGO_TARGET_TMPDIR`
  （target/ 下，不入库）。

## 测试基线（本轮合入时）

| crate | 测试 | 说明 |
| --- | --- | --- |
| ye-time | 23 | 整除性/往返/舍入/溢出/`i64::MIN` 边界 |
| ye-pixfmt | 14 | 元数据表/布局公式/紧密切片/错误 |
| ye-container-mp4 | 78 | 读取端黄金/变体/错误/回归（手工字节）；fMP4 读全矩阵；Mp4Writer progressive/fMP4 单元层；ffmpeg/ffprobe oracle 对拍（解封装/封装/fMP4） |
| ye-cli | 10 | probe 报告/参数解析/二进制端到端；remux e2e（ffmpeg 造源→真二进制→ffprobe+逐包对拍）/参数解析 |

单元层全为手工构造字节；oracle 组（容器 crate 9 项 + remux e2e 1 项）
用 PATH 上的 ffmpeg/ffprobe 造源对拍，找不到工具即打印跳过信息返回
（不 fail），临时文件全落 `CARGO_TARGET_TMPDIR`（target/ 下，不入库），
零网络。

## ye-container-mp4 Mp4Writer（封装，模块二 + 模块三 fMP4）

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
- hdlr 与 AudioSampleEntry 兼容性注记（writer 已按规范写，集成时对齐
  读取端）：①hdlr 的 handler 四字符双写在 pre_defined 与 handler_type
  两个槽位——ISO 14496-12 的 handler_type 在 4 字节 pre_defined 之后，
  而读取端 `parse_hdlr` 只跳 version/flags 未跳 pre_defined（缺陷）；
  双写保证两代读取端都读到正确 handler，读取端修复后此写法依旧兼容，
  不必回改。②mp4a entry 按 ISO 写 28 字节固定字段（SampleEntry 基座 8
  + reserved u32[2] 8 + 声道/样宽/pre_defined/reserved 8 + 采样率 16.16
  在 +24）；读取端 `extract_entry_fields` 现按 20 字节旧布局解析（声道
  在 +8、采样率在 +16），而真实文件（ffmpeg 产物）为 28 字节布局——
  读取端须改为 skip 16 / need 28 / extra 从 +28 取，修复前 mp4a 轨的
  声道/采样率/extra_data 读回失真（tests/writer.rs 的 mp4a 用例与
  oracle_mux 即其回归验证）。
- pub(crate) 组装件（模块三 fMP4 组装的编译契约，签名不得偏离）：
  `BoxBuf`（大端追加 + `into_box`/`into_box64`；注意两者取 `self`
  按值，不能接在返回 `&mut Self` 的追加链尾）、`build_ftyp`、
  `validate_timebase`、`build_stsd_entry`、`build_mvhd`、
  `build_trak_skeleton`（duration_units 写 mdhd 轨道时基域，tkhd 同值；
  模块三传空样例表 + >u32::MAX 占位 duration 复用（强制 v1 定宽，见
  Layout::Fragmented 节）。
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

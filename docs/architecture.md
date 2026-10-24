# 架构

## 分层

```
┌───────────────────────────────────────────────────┐
│ ye-cli          命令行（探查 / 转码）               │
├───────────────────────────────────────────────────┤
│ ye              门面：统一 re-export + feature 汇总 │
├──────────────┬────────────────────────────────────┤
│ ye-filter    │ 处理图（格式转换、缩放、色彩、搬运）  │
├──────────────┴────────────────────────────────────┤
│ ye-codec（+ ye-codec-*）        Packet ⇄ Frame    │
├───────────────────────────────────────────────────┤
│ ye-container（+ ye-container-*） Stream ⇄ Packet  │
├───────────────────────────────────────────────────┤
│ ye-time / ye-pixfmt             基础类型           │
├───────────────────────────────────────────────────┤
│ ye-device       硬件加速（设备内存、硬解/硬编会话）  │
└───────────────────────────────────────────────────┘
```

## 数据模型（规划）

- `Packet`：压缩域数据 + 流索引 + 时戳（以 `ye-time` 的整数 tick 表示）。
- `Frame`：解码后的视频帧或音频块，缓冲由 `ye-pixfmt` 描述，
  可指向系统内存或 `ye-device` 的设备内存。
- `Stream`：容器内一路媒体流的元数据（编解码参数、时基、时长）。
- 时间统一用整数 tick，避免浮点累积漂移；tick 基数须可被常见
  帧率（24/25/30/50/60/120）与音频采样率（44100/48000）整除。

## 接口契约（已落地四层）

以下契约与 `crates/` 对应 src 源码一一对应，以源码为准。先列跨层
通用约定，再分四层各述其核心类型、trait 与约定。

跨层通用约定：

- **错误**：每 crate 自带错误枚举，禁 thiserror；`Display` 与
  `std::error::Error` 手工实现，文案为英文短句、doc 注释中文；
  下层错误以包装变体 + 成对 `From` 透出（`?` 直达），不许吞掉下层类型。
- **panic 只允许在全库五处文档写明的编程错误**（均为调用方索引/参数
  错误，来自文件/码流的数据驱动失败一律 `Result`）：
  ①② `Rational::new` 的 `den == 0` 与 `i32::MIN`×负奇数分母归一化
  溢出；③ `PixelFormat::subsample_log2` 平面索引越界；
  ④ `FrameLayout::plane` 平面索引越界；⑤ `VideoFrame::plane` /
  `plane_mut` 平面索引越界。
- **时戳自描述**：`Timestamp` 内含时基（秒/tick），跨函数边界不裸传
  `i64` ticks；全库无 `AV_NOPTS_VALUE` 等价哨兵，缺时戳一律
  `Option<Timestamp>`；跨时基比较只走 `Timestamp::compare`。
- **f64 只用于 UI 显示与探查估算**（浮点入口仅
  `Rational::to_f64` / `Rational::from_f64`、
  `Timestamp::to_seconds` / `Timestamp::from_seconds_f64`），
  不作权威值、不参与累积运算。
- **线程安全**：公共数据类型与全部抽象 trait 满足 `Send + Sync`
  （jian/duan 的 Tauri 多线程后端必需）。
- **依赖与组织**：四 crate 均零第三方依赖（仅 std + 内部 path 依赖：
  ye-codec → ye-time + ye-pixfmt；ye-container → ye-time + ye-codec，
  不直接依赖 ye-pixfmt）、不含 unsafe；私有模块 + 根 re-export，
  crate 间一律完整路径调用；注册表显式构造、显式传递，无全局单例。

### ye-time 接口契约（有理数时基与时间戳）

对标 libavutil 的 `AVRational` 与 `av_rescale_q`。全库时间统一以整数
tick 表示，避免浮点累积漂移。

| 类型 | 角色 | 关键约定 |
| --- | --- | --- |
| `Rational` | 时基/帧率/速率统一表示 | 字段私有；不变式：`den > 0`、分子分母既约、0 恒为 `0/1`；`Ord` 手工实现（交叉相乘的数值序，非字段字典序） |
| `Rounding` | 重标度舍入模式 | `Zero` / `Inf` / `Down` / `Up` / `NearInf`，对齐 `AV_ROUND_*`；全库默认 `NearInf` |
| `Timestamp` | `i64` ticks + 所属时基 | 任何 ticks 均合法、无哨兵值；不实现 `Ord`，跨时基排序必须走 `compare` |
| `TimeError` | 错误枚举（Copy） | `InvalidDenominator` / `DivideByZero` / `Overflow` / `NotFinite` |

- 构造双轨：`Rational::new`（const fn，字面量/已校验路径，即 ①② 两处
  panic 所在）与 `Rational::try_new`（数据驱动路径，对应 `Err`）。
  帧率常量 `RATE_23_976` … `RATE_48000` 表示「每秒次数」，
  `Timestamp::from_frame_index` 取其倒数作时基，与 `as_frame_index`
  任意时基下精确互逆。
- 算术全部显式 `Result`（`checked_add/sub/mul/div`、`checked_neg`、
  `invert`），i128 中间量先乘后约，归约后落不回 i32/i64 →
  `Err(Overflow)`；不实现 `std::ops`，杜绝隐藏 panic。
- `Timestamp::rescale(_with)`：`av_rescale_q(_rnd)` 等价，i128 全程，
  默认 `NearInf`；`rescale_saturating` 溢出按源 ticks 符号钳到
  i64 边界。
- `Timestamp::compare`：i128 精确交叉相乘，无舍入、永不失败；
  **禁止**经重标度实现比较（舍入会令不等值判等，MP4 mux 的跨流
  dts 排序依赖本方法）。
- `Timestamp::checked_add/sub`：取两时基的公共精确基
  `gcd(分子)/lcm(分母)`，换算系数为整数、无舍入
  （例：`1@1/2 + 1@1/3 = 5@1/6`）。
- `Rational::from_f64(value, max_den)`：`av_d2q` 等价管线（2 的幂
  缩放 + 连分式收敛子/半收敛子，误差用整数交叉相乘比较），只约束
  分母、不约束分子（分子以 i32 表示能力为界）。
- `CANONICAL_TIME_BASE = 1/7_056_000`（= LCM(24, 25, 30, 50, 60, 120,
  44100, 48000)）：跨流时长/起点的全局混流时钟；常见整数速率转入
  **无舍入**，NTSC（·/1001）只在流自身时基精确，转写有 ≤1 tick
  （≈0.14 ns）舍入。容器级时长/起点（见 ye-container）统一以它计。

### ye-pixfmt 接口契约（像素格式、色彩与采样布局）

对标 libavutil 的 `AVPixelFormat` / `AVColorSpace` 家族；只承载元数据
描述，实际转换内核归 `ye-filter`。

- `PixelFormat`（14 变体）：`Gray8`；`Yuv420p(/10Le)`、`Yuv422p(/10Le)`、
  `Yuv444p(/10Le)`、`Yuva420p`；`Nv12` / `Nv21`；`Rgb24` / `Bgr24`、
  `Rgba8` / `Bgra8`。`10Le` = 每样本 10 bit、小端存储。
  - `plane_count`（Nv12 = 2、Yuv420p = 3、Yuva420p = 4、packed = 1）
    与 `component_count`（Nv12 = 3、Rgba8 = 4、Gray8 = 1）区分平面数
    与分量数——交织平面里的各分量分别计数；
  - `subsample_log2(plane)` 语义对齐 `AVPixFmtDescriptor`（该平面每行
    样本 `ceil(w/2^ss_w)`、行数 `ceil(h/2^ss_h)`；越界 panic = ③）；
  - `bit_depth`（10Le 系列为 10，其余 8）；`name` / `from_name`
    FFmpeg 风格规范名精确往返（大小写敏感）。
- `PixelFormat::frame_layout(width, height, align)` → `FrameLayout`：
  舍入一律 ceil（奇数宽/高合法）；Nv12/Nv21 的 UV 平面每行
  `2·ceil(w/2)`；packed RGB(A) 每行 `w × component_count`；
  `stride = align_up(行字节, align)`；平面 `offset` 从 0 前向累加，
  `size()` = 末平面 offset + size。错误：宽或高为 0 →
  `ZeroDimension`；`align` 为 0 或非 2 的幂 → `BadAlignment(align)`。
  极端尺寸饱和处理，不 panic。`PlaneLayout` 为 stride/rows/offset/size
  公有字段；`FrameLayout` 字段私有、只能经本方法构造（`plane(i)`
  越界 panic = ④）。`DEFAULT_STRIDE_ALIGN = 64`（一条缓存行，覆盖
  AVX-512 整行访存，供 `VideoFrame::new` 作缺省对齐）。
- H.273 色彩族（码值对齐 FFmpeg `AVColor*`，MP4 `colr` 读写直接用
  `to_code`/`from_code`）：`ColorPrimaries` / `ColorTransfer` /
  `ColorSpace` 带 `Unknown(u8)` 变体——`from_code` 对未定义码返回
  `None`，需无损保存时由调用方构造 `Unknown(code)` 原样写回；
  `ColorRange` / `ChromaLocation` 无 Unknown 变体。`ColorDesc` 五字段
  聚合，`Default` = 全 Unspecified。
- `SampleFormat`：`U8/I16/I32/I64/F32/F64` + `P` 后缀平面变体
  （每声道一块连续缓冲，其余为 packed 交织）；`bytes_per_sample`、
  `is_planar`；`buffer_len(format, channels, frames)`：planar =
  `声道 × 帧 × bps`、packed = `帧 × 声道 × bps`（溢出饱和处理；
  0 声道 → 0）。
- `ChannelLayout`：u64 位掩码，位序对齐 FFmpeg `AV_CH_*`（bit0 前左
  … bit5 后右）；不变式 `mask != 0`（`try_from_mask` 拒绝空掩码 →
  `EmptyChannelLayout`）；常量 `MONO`（0x4）、`STEREO`（0x3）；
  `channels()` = 置位数；`has_channel(bit)`。
- `PixfmtError`：`ZeroDimension` / `BadAlignment(usize)` /
  `EmptyChannelLayout`。

### ye-codec 接口契约（Packet ⇄ Frame）

对标 libavcodec；具体 codec 以 `ye-codec-*` 独立 crate 落地，由 `ye`
门面按 feature 汇总。

- trait（均声明 `Send + Sync` 上界，否则 `Box<dyn Decoder>: Send`
  编不过 E0277）：
  - `Decoder`：`codec_id` / `feed(&Packet)` / `flush()` / `pull()`；
    `Encoder`：`codec_id` / `feed(&Frame)` / `flush()` / `pull()`，
    两者对称。
  - 状态机契约：flush 排空后再 `feed` → `Err(InvalidState(..))`
    （对齐 FFmpeg `AVERROR_EOF`）；`pull` 返回 `Finished` 后**幂等**
    终态；暂无输出且未排空 → `NeedInput`。推拉语义对标
    `avcodec_send_packet` / `avcodec_receive_frame`，以枚举分载
    （`Decoded = Frame | NeedInput | Finished`、
    `Encoded = Packet | NeedInput | Finished`）替代 EAGAIN/EOF
    哨兵返回值。
  - 工厂为普通 `fn` 指针：`DecoderFactory = fn(&CodecParameters) ->
    Result<Box<dyn Decoder>, CodecError>`，`EncoderFactory` 消费
    `&EncoderConfig`（零依赖、天然 Send + Sync）。
- 数据载体：
  - `Packet`（公有字段）：`data: Arc<[u8]>`（克隆零拷贝共享）、
    `stream_index`、`pts` / `dts` / `duration: Option<Timestamp>`、
    `flags`；`PacketFlags` 手写位标志（`KEYFRAME` / `CORRUPT` /
    `DISCARD`，`empty` / `contains` / `insert` + `BitOr`）；
    `Packet::empty(stream_index)` 空包构造。
  - `VideoFrame`：不变式 `data.len() == layout.size()`；
    `new` 内部按 `DEFAULT_STRIDE_ALIGN` 计算布局并零初始化；
    `from_parts` 包装外部缓冲（长度不符 →
    `BufferMismatch{expected, actual}`，布局与 format/width/height
    描述不符 → `InvalidArguments`）；`plane` / `plane_mut` 越界
    panic = ⑤。
  - `AudioFrame`：缓冲长度 = `SampleFormat::buffer_len(format,
    channels, frames)`；`sample_rate == 0` → `InvalidArguments`；
    `data` / `data_mut` 整块访问（声道级切片推迟到 M4）。
  - `Frame` 枚举（`Video` / `Audio`）统一入口：`media_type` / `pts` /
    `set_pts` 委托内层。
- 参数：`CodecParameters`（`codec` / `media_type` / `extradata`
  （SPS/PPS/ASC 等带外参数）/ `bit_rate` / `video` / `audio`，
  容器层填、解码工厂消费）；`EncoderConfig`（`time_base` = **输出包
  时戳的时基**、`bit_rate`、`video` / `audio` 按 codec 媒体类型二选一）。
- 打戳契约：解封装器以流时基给包打戳；编码器输出包以
  `EncoderConfig.time_base` 打戳；封装器写包时向目标流时基重标度。
- `CodecRegistry`（显式构造/传递，无全局单例）：`register_decoder` /
  `register_encoder` 同 `CodecId` 重复注册返回旧工厂（替换语义）；
  `create_decoder(&CodecParameters)` / `create_encoder(&EncoderConfig)`
  未注册 → `NotRegistered`，工厂自身拒绝原样透传（如
  `UnsupportedCodec` / `InvalidArguments`）；`supports_decoder` /
  `supports_encoder`；`decoder_ids` / `encoder_ids` 按 `CodecId` 的
  `Ord` 序（内部 BTreeMap）。
- `MediaType`（`Video` / `Audio` / `Subtitle`）与 `CodecId`
  （`H264` / `Hevc` / `Av1` / `Vp9` / `Aac` / `Flac` / `Opus` / `Mp3` /
  `Unknown`）；`CodecId::media_type()` 对 `Unknown` 返回 `None`。
- `CodecError`：`UnsupportedCodec` / `NotRegistered` /
  `InvalidData(String)` / `InvalidState(&'static str)` /
  `InvalidArguments(&'static str)` / `BufferMismatch{expected, actual}` /
  `Time(TimeError)` / `Pixfmt(PixfmtError)`（后两者配套 `From`，
  `source()` 透出下层）。

### ye-container 接口契约（Stream ⇄ Packet）

对标 libavformat；具体容器以 `ye-container-*` 独立 crate 落地。依赖
ye-time 与 ye-codec，不直接依赖 ye-pixfmt。

- IO 组合 trait：`ReadSeek`（`Read + Seek + Send + Sync`）与
  `WriteSeek`（`Write + Seek + Send + Sync`），各带空白实现——
  `dyn Read + Seek + Send` 别名形式违反 trait 对象「只允许一个非
  auto trait」规则（E0225），组合 trait 使 `Box<dyn ReadSeek>` 可跨线程。
- `Demuxer`（`Send + Sync`）：`format_name` / `read_header()`（返回
  `MediaHeader` 流表）/ `read_packet()`（EOF → `Ok(None)` 且此后幂等）/
  `seek(stream_index, target)`（`target` 与返回值都以该流时基计，返回
  实际落到的**前一个关键帧**时刻；流不存在 → `StreamNotFound`）。
  实现契约：`read_packet` 产出的包必须已按对应 `Stream::time_base`
  打戳并填好 `stream_index`（重标度语义落在 demuxer，不在调用方）。
- `DemuxerFactory`（`Send + Sync`）：`name() -> &'static str`（注册表
  按它点名/去重）/ `probe(&head) -> ProbeScore`（只看开头字节，
  **不得消费输入**——窗口由注册表准备并 seek 回 0）/
  `open(Box<dyn ReadSeek>)`。
- `Muxer`：调用顺序 `write_header(&[StreamTemplate])` →
  `write_packet`* → `write_trailer`（默认空实现 `Ok(())`，兼容无
  trailer 的格式）；顺序违约 → `Err(InvalidState(_))`；包时基与模板
  `time_base` 不符时 muxer **内部重标度**（失败以 `Err(Time(..))`
  透出，不静默丢弃）。`MuxerFactory`（`Send + Sync`）：`name()` /
  `open(Box<dyn WriteSeek>)`，无 probe（输出格式由调用方点名）。
- 流对象：`Stream`（`index`（容器保证从 0 连续递增）/ `media_type` /
  `codec: CodecParameters` / `time_base`（流级起点、时长与包时戳的
  tick 基数）/ `start_time` / `duration: Option<Timestamp>` /
  `nb_frames` / `metadata`）；`MediaHeader`（`format_name` /
  `streams`（顺序即 index 顺序）/ 容器级 `duration` / `start_time`
  （以 `CANONICAL_TIME_BASE` 计，跨流可比）/ `bit_rate` / `metadata`）；
  `StreamTemplate`（封装侧一次性输入，`time_base` 为**目标时基**——
  `write_packet` 按它重标度；`frame_rate` 仅视频流有意义）。
- `ProbeScore`（u8 newtype，0..=100，`new` 拒绝 >100 的分数）：
  `NONE` = 0、`MINIMUM` = 25（注册表 auto-probe 的录取阈值，**不是**
  构造下限——低于它的分数须可表达）、`EXTENSION` = 50、
  `MAXIMUM` = 100；derive `Ord` / `Hash` 供取最高分与分数桶。
- `ContainerRegistry`（显式构造/传递，无全局单例）：
  `PROBE_WINDOW = 8192`；`open_demuxer` auto-probe = 读窗口（短输入
  读到 EOF 为止）→ seek 回 0 → 逐工厂打分取最高（**同分取先注册者**）
  → 打开，最高分 < `MINIMUM` 或无工厂注册 → `Err(UnknownFormat)`，
  返回 `(胜出工厂名, demuxer)`；`open_demuxer_with` / `create_muxer`
  按名直取（未注册 → `Err(UnknownFormat)`）；`probe()` 一步式
  「auto-probe 打开 + 读头」返回 `(格式名, MediaHeader)`；
  `register_demuxer` / `register_muxer` 同名（`name()` 相等）替换并
  返回旧工厂；`demuxer_names` / `muxer_names`（注册顺序、无重复）。
- `Metadata`：`BTreeMap<String, String>` 封装，按键字节序迭代、确定
  可复现；键值**原样保留不做规范化**（大小写折叠、语言码转换等归
  具体容器实现）。
- `ContainerError`：`Io(io::Error)` / `UnknownFormat` /
  `InvalidData(String)` / `InvalidState(&'static str)` /
  `StreamNotFound(u32)` / `Time(TimeError)`；derive 仅 `Debug`
  （`io::Error` 不可 `Clone` / `PartialEq`，测试断言一律 `matches!`）；
  `From<io::Error>` / `From<TimeError>` 成对。

## 硬件加速

`ye-device` 定义设备内存与硬件编解码会话的抽象，各后端以 feature 开启：

- macOS：VideoToolbox
- Windows：D3D11VA（解码）/ NVENC、AMF（编码）
- Linux：VA-API

软硬路径共享 `ye-codec` / `ye-filter` 的上层 trait；滤镜图负责
设备内存与系统内存之间的搬运，调用方无须感知数据落在哪里。

## 正确性策略

- 解码器以 FFmpeg 为外部 oracle：同一输入流，逐帧比对解码输出；
  容差（bit-exact 或逐通道 PSNR 下限）在 conformance 里逐 codec 声明。
- 测试向量不入库：`tests/conformance` 只维护清单（名称、来源、校验和）
  与获取脚本，运行前拉取到本地缓存。

## 演进规则

API 由 jian / duan / yue 的真实消费需求驱动：先有调用方，再固化抽象。
每一层落地前须在 docs 里补该层的接口契约说明；已落地的契约汇总在
[contracts.md](contracts.md)，改契约须同步改该文。

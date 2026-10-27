# M2 收口施工图——剪接入（抽帧 / 波形 / probe 走冶）

> 2026-10-27 起草（规划师）。依据：勘察材料（ye HEAD=c8a3eb8、jian HEAD=1ae356c，
> 两仓 develop 均 clean）+ 规划实读复核。**同日按评审意见修订（17 条逐条
> 吸收）**，要点：T4 补齐 qa/release 打包 job 的 ye 检出（原漏，照做必挂
> CI）；T1/T2 测试向量路线重定（T1 真路径 skip 合法化 + 定位器基建上提，
> T2 改 ye 自产向量彻底脱 ffmpeg）；T2 验收 skip 语病修正；波形双轨拆
> pub 函数并补 `-ignore_editlist 1`（输入侧）；probe 封面流启发式回退
> （ye Stream 无 disposition）；audio_peaks 前置流布局校验（Frame 无流
> 标识，多音轨素材不得静默混桶）；定位器口径两仓统一；抽帧点位 89 越界
> 修正；jian 侧 mod pub 化；§1.2 CI 现状描述纠偏。
> 范围裁决（硬约束）：两仓施工，完成后 M2 准入达成——「剪的缩略图/抽帧
> （extract_frame）与波形走 ye，同点位与 ffmpeg 抽帧对拍在容差内」
> （roadmap.md:59-62 原文）。同时清偿 M1 收口准入欠账：probe 双引擎
> （roadmap.md:56-58）。门禁口径定死：**ye = fmt + clippy + test 三件套；
> jian = cargo check --locked + cargo test + npm test + npm run build 四件套**。
>
> 与 m3-plan.md 的关系：M3 首轮（锻的编码/转码）已交付收口，本图不动 ye
> 编码面；本图消费的是 M2 解码面（H.264 Baseline 解码、AAC-LC 解码、像素
> 转换/scale、PNG 编码）、M3 的 convert_frames 泵与 H.264 编码器（T2 测试
> 自产向量用）。

## 0. 目标与准入判据

| 判据 | 来源 | 验证落点 |
| --- | --- | --- |
| 剪的抽帧走 ye（extract_frame），ffmpeg 留回退/oracle | roadmap.md:61-62 | T5 双引擎 + T8 对拍 |
| 剪的波形走 ye（AAC 解码→PCM 峰值） | roadmap.md:37,60-61 | T1/T6 + T8 对拍 |
| 同点位与 ffmpeg 对拍在容差内（抽帧像素、波形峰值） | roadmap.md:62 | T3（ye 侧）+ T8（jian 侧） |
| probe_media 加双引擎开关，ffprobe 留 oracle 与回退（M1 欠账） | roadmap.md:56-58 | T7 |
| jian CI 全部 Rust 编译点补 ye 兄弟仓检出 | 勘察 + 评审 | T4 |

## 1. 现状与接入点

### 1.1 ye 侧（消费面，均为实读或勘察指认）

| 接入点 | 现状 | 位置 |
| --- | --- | --- |
| 门面 feature 面 | `mp4`(默认)/`h264`/`aac`/`png` 均已存在，mod 注册在 lib.rs:36-46（实读）；**本轮不新增 feature 名** | `ye/crates/ye/src/lib.rs` |
| 帧消费泵 | `convert_frames(containers, codecs, input: Box<dyn ReadSeek>, chain, pixels, sink, progress, cancel)`（实读 convert.rs:214-223）；视频帧固定链序 像素转换→scale→sink，音频帧解码后直通 sink（convert.rs:273-286 实读）；取消每包检查点→`Cancelled`。**给每条音/视频流都建解码器、`Frame` 不携带流标识**（实读 convert.rs:233-242；勘察 frame.rs:188-195 无 stream_index）——消费助手必须自做流布局前置校验（§2.1/§2.2） | `ye/crates/ye/src/convert.rs` |
| 容器探查 | `ContainerRegistry::probe(input) -> (格式名, MediaHeader)`（实读 registry.rs:158-162）；`Stream` 字段 index/media_type/codec/time_base/start_time/duration/nb_frames/metadata（实读 stream.rs:23-40），**无 disposition/attached_pic 概念**（评审实读 + 全仓 grep 零命中）——封面流判定只能启发式（§2.3） | `ye/crates/ye-container/src/` |
| AudioFrame | packed F32（声道交织小端）、`data()` 整块切片、pts 自带时基；声道级访问 M4 才有——**交织拆分归本轮波形纯核自己做**（勘察 frame.rs:188-265） | `ye/crates/ye-codec/src/frame.rs` |
| AAC 解码 | LC、1/2 声道、1024 帧；PNS/SBR/多声道等一律 Err（勘察 decoder.rs:392-413）→ **jian 侧须 ffmpeg 回退轨** | `ye/crates/ye-codec-aac/src/decoder.rs` |
| AAC 编码 | **不存在**（M3 只交付 H.264 编码 + 音频直通，lib.rs:15 实读 + 评审引 waitlist.md:25-26）——**AAC 测试向量只能 ffmpeg 生成**（§1.4-8） | — |
| H.264 编码 | Baseline 编码器 + MP4 封装已交付（M3）——**T2 测试向量程序内自产的依赖**（先例 crates/ye/tests/encode_h264.rs:26-41 实读，dev 依赖直连 + LCG 确定性向量） | ye-codec-h264 |
| H.264 解码 | Baseline/CAVLC 真实现，pts 透传（评审核实 decoder.rs:503,996）；High/CABAC 未收口，High 素材 ye 轨会 Err（勘察 waitlist.md:23）→ **抽帧回退** | ye-codec-h264 |
| 像素核 | yuv420p→rgba8（BT.601，对齐 swscale，评审引 convert.rs:987-995）、rgba8 scale 已就位（勘察） | `ye/crates/ye/src/convert.rs` |
| PNG 出口 | `ye::png::encode_rgba`（RGBA8→PNG 一步，无损经 ffmpeg oracle 逐字节 0 容差对拍；lib.rs:16 实读）；**只 encode 不 decode** | `ye/crates/ye/src/png.rs` |
| 波形峰值 API | **缺席**（全仓 grep 仅注释提及；waitlist.md:24 明确记录）——T1 新建 | — |
| oracle 测试先例 | `ye-png/tests/oracle_png.rs:26,116`：PATH 找 ffmpeg，找不到打印原因跳过；conformance README 容差口径与「只准收紧」POLICY_NOTE（实读） | `ye/tests/conformance/README.md` |

### 1.2 jian 侧

| 接入点 | 现状 | 位置 |
| --- | --- | --- |
| 命令注册面 | 4 命令：probe_media / ffmpeg_info / extract_frame / run_export（实读 lib.rs:13-18）；**mod 声明现为私有**（lib.rs:3-4 `mod ffmpeg; mod probe;`，实读）——新模块须 pub 化供 tests 直调（§1.4-9） | `jian/src-tauri/src/lib.rs` |
| 抽帧 | ffmpeg 子进程 `-ss {t} -i path -frames:v 1` → `%TEMP%/jian-frame-{纳秒-计数}.png`，负时点钳 0（实读 ffmpeg.rs:122-156）；临时 png 永不清理系已留档低危问题（待开发.md:29，实读 grep） | `jian/src-tauri/src/ffmpeg.rs` |
| probe | 纯 ffprobe，无双引擎；口径纯函数 classify_media_kind（kind 次序/封面流剔除/时长回退）写死单测（勘察 probe.rs 全文，封面剔除逻辑在 probe.rs:73——评审指认） | `jian/src-tauri/src/probe.rs` |
| 前端调用点 | thumbnail.ts:26 invoke `extract_frame`（实读）；probe.ts:20 invoke `probe_media`（实读）；波形零雏形（勘察 grep 全空） | `jian/src/core/media/` |
| ye 依赖 | 无（实读 jian/src-tauri/Cargo.toml，依赖仅 tauri/插件/serde） | `jian/src-tauri/Cargo.toml` |
| CI | ci/qa/release 三 workflow；**fang 检出在 web-gate 与 qa/release 的打包 job，rust-gate 无任何兄弟仓检出**（实读 ci.yml:58-74、qa.yml:43-59 rust-gate 无检出；qa.yml:61-98 qa-app-windows 打包 job 有 fang 无 ye；release.yml 同构，grep 核实）；rust-gate 跑 `cargo check --locked`，打包 job 跑 `npm run tauri build`（同要编译 Rust） | `jian/.github/workflows/` |
| 波形 UI | 未排期——本轮**只交付命令层 + core 门面 + 对拍**，不造 UI（范围④） | — |

### 1.3 样板（duan 已落地的引擎接线，均实读）

- **Rust 命令层**：`duan/src-tauri/src/ye_engine.rs` —— `ye_convert`（pub 纯库
  主体，不碰 Tauri 类型，`tests/ye_engine_e2e.rs` 直接调它对拍，:60-75）+
  `run_transcode_ye`（#[tauri::command(async)]，参数校验/事件桥，:120-131）；
  取消从 kill 子进程换成 AtomicBool（:29 CANCEL_FLAGS 同构表）。错误文案
  中文直给。
- **模块 pub 化**：`duan/src-tauri/src/lib.rs:8-10` `pub mod ye_engine` 等，
  注释言明「pub 供 tests 直接调」（评审引证）——jian 新模块照此。
- **前端翻译层**：`duan/src/core/preset/buildYeTranscodeParams.ts` —— 纯函数，
  能力面外明确拒绝并给可操作文案（「请切回 ffmpeg 引擎」），不静默降级。
- **CI 兄弟仓检出**：`duan/.github/workflows/ci.yml:64-76`（rust-gate）与
  `duan/.github/workflows/qa.yml:89-97`（**打包 job，注释明写「tauri build
  需 ../ye」**，实读）—— checkout `Bai-Gong/ye`（ref=develop, path=ye）→
  `mv ye ../ye` 两步。jian 的 ci.yml 恰无打包 job，**照抄 ci.yml 只补
  rust-gate 会漏 qa/release 打包 job（评审问题 1）**——T4 范围按 §2.4。

### 1.4 已知坑（勘察 + 评审 + 本轮实读，施工时逐条对照）

1. **ye 仓库根是虚拟清单**：路径依赖必须指 `../../ye/crates/ye`
   （duan/src-tauri/Cargo.toml 同款先例）。
2. **jian 无本地 ffmpeg**：本机 PATH 无 ffmpeg、BJ_FFMPEG_DIR 未设，二进制
   实际在 `duan/src-tauri/binaries/`（ffmpeg.exe + ffprobe.exe 实见）。dev
   与对拍测试的 ffmpeg 定位须覆盖仓相对路径（§2.5）。
3. **CI 上没有 duan 也没有 ffmpeg**：jian/ye 的对拍测试在 CI 必然走 skip
   分支——skip 是合法态但要打印原因（oracle_png.rs 先例），本地收口必须
   实跑全绿并记录。
4. **CRLF**：两仓 core.autocrlf=true 且无 .gitattributes；ffmpeg -progress
   行已见 `\r\n`（jian ffmpeg.rs:232 单测盖过）。子进程输出解析一律 trim。
5. **AAC edit list**：ffmpeg 解码 mp4 容器 AAC 默认按 edit list 裁 priming
   帧，对拍参考侧与回退轨都必须带 `-ignore_editlist 1`（conformance
   README:113-116 先例）；**它是 mov 解封装器私有选项，必须放在 `-i`
   之前（输入侧），放输出位 ffmpeg 报 Option not found**（评审问题 5）。
6. **ye 版本号不得 bump**：jian 的 Cargo.lock 锁路径依赖版本，ye 侧若改
   workspace version 会让 jian 的 --locked 红。本轮 ye 只加模块，不动版本。
7. **lib.rs 所有权**：ye 门面 lib.rs 与 jian lib.rs 都是「加一行 mod/注册」
   的必经点——相关任务已按此串行（§4），不得并行抢文件。
8. **AAC 向量只能 ffmpeg 生成**（ye 无 AAC 编码器，§1.1）——依赖 ffmpeg
   向量的测试（T1 消费助手真路径、T3 波形对拍）**定位器落空时合法
   skip**；反之后述 T2 自产向量（H.264 编码器 + MP4 muxer）**无条件跑**。
9. **jian 模块须 pub 化**：`pub mod ye_engine; pub mod waveform;`（duan
   lib.rs:8-10 先例），否则 T8 的 tests/parity.rs 无法直调纯库主体。
10. **Frame 无流标识**（§1.1）：convert_frames 把所有流的帧交织送同一
    sink——多音频流素材若不在助手层前置校验，会把两路样本混进同一组
    桶（**静默错数据**，比 Err 更坏）。§2.1 定死前置校验。

## 2. 设计定形

### 2.1 ye 波形模块（T1，`ye/crates/ye/src/waveform.rs`）

纯核（仅 std，**无条件编译**，进默认门禁基线）：

```rust
pub struct PeakBucket { pub min: f32, pub max: f32, pub rms: f32 }

/// packed 交织 f32（与 AudioFrame.data 同形态）→ [声道][桶]。
/// 声道 c 第 i 个样本 = samples[i * ch + c]（勘察 frame.rs:182-184 交织语义）。
pub fn peaks_by_bucket_count(samples: &[f32], channels: usize, buckets: usize)
    -> Vec<Vec<PeakBucket>>;
pub fn peaks_by_bucket_size(samples: &[f32], channels: usize, samples_per_bucket: usize)
    -> Vec<Vec<PeakBucket>>;
```

- 分桶规则写死：桶 k 覆盖 `[floor(N*k/B), floor(N*(k+1)/B))`（B=桶数，
  N=每声道样本数；桶长入口 B=ceil(N/桶长) 同式推导）——**两入口必须共享
  同一条边界公式**，禁止各写各的；尾桶可不满；N=0 返回空 Vec；桶数 0 是
  InvalidArguments。
- rms = sqrt(mean(x²))，f32 输入用 f64 中间量累计（长素材防漂）。
- 多声道：逐声道独立分桶输出（jian 波形 UI 未定，先给全量）。

消费助手（`#[cfg(all(feature = "mp4", feature = "aac"))]`，函数级门）：

```rust
pub struct AudioPeaks { pub sample_rate: u32, pub channels: usize,
                        pub bucket_samples: usize, pub buckets: usize,
                        pub peaks: Vec<Vec<PeakBucket>> }
/// 前置流布局校验（坑 §1.4-10）：ContainerRegistry::probe 读 MediaHeader，
/// 要求「恰一条音频流」（视频流可有可无，sink 只收 AudioFrame；多音频流
/// → 明确 Err，绝不静默混桶）。probe 消费后把 input seek 回 0 再喂
/// convert_frames。桶长入口流式喂桶；解码 Err（PNS/多声道/非 LC）原样
/// 透传——不在 ye 侧兜底，回退归 jian 命令层。
pub fn audio_peaks(input: Box<dyn ReadSeek>, bucket_samples: usize,
                   cancel: &AtomicBool) -> Result<AudioPeaks, ConvertError>;
/// 桶数入口：内部收全样本再按数分桶（返回 buckets = 请求值，尾桶可不满；
/// 长素材内存局限注释标注，M4 流式化——见 §6）。
pub fn audio_peaks_with_count(input: Box<dyn ReadSeek>, bucket_count: usize,
                              cancel: &AtomicBool) -> Result<AudioPeaks, ConvertError>;
```

- 音频帧经 convert_frames 直通 sink（实读 convert.rs:273-286），VideoChain
  传缺省（不影响音频）；pts 不用于分桶（帧序即样本序）。含视频流时视频
  帧也会被解码并在 sink 丢弃（Baseline 可解、High 打回整体 Err→jian
  回退；泵级选流优化归 M4，§6）。

### 2.2 ye 抽帧门面（T2，`ye/crates/ye/src/frame_grab.rs`）

`#[cfg(all(feature = "mp4", feature = "h264", feature = "png"))]`：

```rust
pub struct GrabbedFrame { pub width: u32, pub height: u32, pub rgba: Vec<u8> } // rgba8
/// 打开 → 逐帧解码（链 yuv420p→rgba8，size 缺省）→ 取首个 pts ≥ time_us
/// 的视频帧 → 取到即置 cancel（泵在下一包检查点返回 Cancelled，门面
/// 翻译为成功）；EOF 仍未取到 → 明确 Err（时点超素材长）。
pub fn decode_frame_rgba(input: Box<dyn ReadSeek>, time_us: i64)
    -> Result<GrabbedFrame, ConvertError>;
/// decode_frame_rgba → ye::png::encode_rgba 一步出口（剪缩略图管线，
/// png.rs:6-8 定位的「最后一环」）。
pub fn grab_frame_png(input: Box<dyn ReadSeek>, time_us: i64) -> Result<Vec<u8>, ConvertError>;
```

- time_us 负值钳 0（与 jian ffmpeg 轨 ffmpeg.rs:127 同语义）。
- pts 缺失的帧：跳过时点判定但计数，若整个素材无任何 pts → 明确 Err
  （不猜帧序）。
- High/CABAC 素材解码 Err 原样透传（jian 回退）。
- **音轨行为写明**（评审问题 13）：含不可解音轨（如多声道 AAC）的 mp4
  会因音频解码器 Err 使 convert_frames 整体打回——行为可兜（jian 回退
  ffmpeg），由 T3 的「视频+不可解音轨」向量锁定，T2 不实现泵级绕行。

### 2.3 jian 引擎开关形态（三处不同形，各有理由）

| 路径 | 形态 | 理由 |
| --- | --- | --- |
| 抽帧 | **独立命令** `extract_frame_ye`（duan 样板同构：pub 纯库主体 `extract_frame_ye_impl` + command 壳）；前端 thumbnail.ts **ye 优先、失败 console.warn 后回退 ffmpeg 轨** | 能力受限的并行路径，独立命令便于 oracle 对拍（两命令并存可互调）；High 素材自动落 ffmpeg 用户无感。**Rust 错误文案只述能力面外原因（如「ye 解码仅支持 H.264 Baseline」），不声称「已回退」——回退动作与提示归前端**（评审问题 16） |
| 波形 | 命令 `waveform(path, buckets)` 内部 auto 选择，**纯库主体拆两个 pub 函数**：`waveform_peaks_ye`（调 `audio_peaks_with_count`，桶数直达、不经 probe 时长估算——避免跨任务耦合与桶数不精确，评审问题 9/14）与 `waveform_peaks_ffmpeg`（`ffmpeg -ignore_editlist 1 -i path -map 0:a:0 -f f32le -c:a pcm_f32le -`，**输入侧选项**（坑 §1.4-5）流式读 stdout（管道读取沿 run_export 先例 ffmpeg.rs:189-195，勿用 .output() 整收），喂同一份分桶纯核） | 前端无感单入口；T8 对拍直接分别调两个 pub 函数（评审问题 8）；两轨桶数契约一致（返回桶数=请求值） |
| probe | `probe_media(path, engine)` **参数式**（缺省 "auto"：ye 轨优先、Err 或疑似封面回退 ffprobe 轨），ProbeResult 加 `usedEngine` 字段（serde camelCase，前端 interface 同步、可选字段不破现有调用） | probe 是高频路径且 ye 轨全覆盖常规 mp4，无需独立命令；roadmap.md:56-58 原文即「加开关」 |

- **probe 封面流启发式**（评审问题 6；ye Stream 无 disposition，§1.1）：
  ye 轨读 MediaHeader 后，命中「多条视频流」或「存在音频流且某视频流
  duration < min(1s, 最长音频流 duration × 5%)」（duration 缺失时按
  nb_frames=1 视为疑似）→ 判疑似封面 → **整体回退 ffprobe 轨**，
  usedEngine="ffmpeg"。判据阈值写死为测试常量；带封面 m4a 向量（§3.1）
  锁定该行为——绝不把封面流当视频流误报 kind=video。
- probe 口径纯函数（classify_media_kind 次序、时长回退）两轨同源复用
  不分叉；封面剔除本身在 ffprobe 轨既有逻辑（probe.rs:73）不动。
- ffmpeg 轨（extract_frame / probe 的 ffprobe 路径）**原样保留**，是回退
  与 oracle，不在本轮改动语义。waveform 的 ffmpeg 轨是新写回退（非既有
  路径），同样定位为 oracle 之用。
- 波形结果结构 `#[serde(rename_all = "camelCase")]` 与 TS interface 逐字段
  对齐、注释互指（probe.rs ↔ probe.ts 先例）。

### 2.4 CI 接线（T4）

- `jian/src-tauri/Cargo.toml` 依赖节加：
  `ye = { path = "../../ye/crates/ye", default-features = false, features = ["mp4", "h264", "aac", "png"] }`
  （比 duan 多开 aac/png——波形与 PNG 出口所需）。
- **ye 检出须覆盖全部 Rust 编译点**（评审问题 1）：
  - ci.yml：rust-gate；
  - qa.yml：rust-gate + `qa-app-windows` 打包 job（tauri build 编译 Rust
    需 ../ye）；
  - release.yml：rust-gate + 对应打包 job（同构）。
  各处均为 duan ci.yml:64-76 两步（checkout Bai-Gong/ye ref=develop
  path=ye → `mv ye ../ye`），打包 job 注释照 duan qa.yml:89-97（「tauri
  build 需 ../ye」）。

### 2.5 ffmpeg 定位器（T1 建、T3/T8 复用，两仓同口径——评审问题 10）

**统一顺序**：① 环境变量 `BJ_FFMPEG_DIR`（目录，须 ffmpeg+ffprobe 双 exe
齐备才认——jian 既有语义，dev 场景实指 duan binaries）② PATH
（oracle_png.rs:26 先例）③ 仓相对路径——ye 侧测试
`CARGO_MANIFEST_DIR/../../../duan/src-tauri/binaries/`，jian 侧
`CARGO_MANIFEST_DIR/../../duan/src-tauri/binaries/` ④ 全落空：eprintln
原因 + **合法 skip**（测试通过，本地收口必须实跑全绿）。ffprobe 同目录
推导。

- **ye 侧基建归 T1**：`ye/crates/ye/tests/common/mod.rs`（定位器 + AAC
  向量配方），T1 的消费助手真路径测试即用；T3 复用**只读不改**（需改动
  回 T1 的文件域，走依赖串行）。
- jian 侧 T8 在 tests/parity.rs 内实现同口径。
- CI 无 duan 检出 → 恒走 ④，门禁不红（坑 §1.4-3）。

## 3. 对拍口径（冻结）

容差常量一律**写死在测试文件顶部 + 「只准收紧」POLICY_NOTE**（沿
conformance README:129-136 与 ye-filter mod.rs 先例：向严修改免评审，任何
放宽须回本施工图重新定口径）。初值依据已注，实测后收紧并把最终数字留在
测试注释里。

### 3.1 向量配方（写死进测试，lavfi 确定性源，conformance 先例）

```text
视频 vec.mp4（抽帧/probe 用）：
  ffmpeg -f lavfi -i testsrc2=duration=3:size=320x240:rate=30 \
         -c:v libx264 -profile:v baseline -pix_fmt yuv420p -y vec.mp4
音频 vec-audio.m4a（波形用，立体声正弦 48k）：
  ffmpeg -f lavfi -i sine=frequency=440:sample_rate=48000:duration=2 \
         -ac 2 -c:a aac -aac_pns 0 -b:a 128k -y vec-audio.m4a
音频 vec-noise.m4a（波形用，单声道白噪声 44.1k，rms 有区分度，
  且对样本错位敏感——能暴露 edit list 口径错误，评审问题 4）：
  ffmpeg -f lavfi -i anoisesrc=color=white:sample_rate=44100:duration=2 \
         -ac 1 -c:a aac -aac_pns 0 -b:a 128k -y vec-noise.m4a
带封面 vec-cover.m4a（probe 回退行为锁定，T7/T8 用）：
  ffmpeg -f lavfi -i testsrc2=size=320x240:rate=1 -frames:v 1 -y cover.jpg
  ffmpeg -i vec-audio.m4a -i cover.jpg -map 0:a -map 1:v -c:v mjpeg \
         -disposition:v:0 attached_pic -y vec-cover.m4a
不可解音轨 vec-video-multi.mp4（抽帧整体 Err 行为锁定，T3/T8 用）：
  ffmpeg -f lavfi -i testsrc2=duration=3:size=320x240:rate=30 \
         -f lavfi -i sine=frequency=440:sample_rate=48000:duration=3 \
         -map 0:v -map 1:a -c:v libx264 -profile:v baseline \
         -c:a aac -ac 6 -aac_pns 0 -y vec-video-multi.mp4
```

- `-profile:v baseline`：ye H.264 只收 Baseline（High 未收口）；
  `-aac_pns 0`：ye AAC 不支持 PNS（conformance README:113 先例）；
  `-ac 6`：ye AAC 拒多声道（decoder.rs 勘察），作「不可解音轨」源。
- **skip 语义分级**（评审问题 2/3）：定位器落空 → 合法 skip（打印原因）；
  定位器命中而向量生成失败（ffmpeg 退出非零/输出异常）= 测试失败，不许
  skip。T2 的自产向量（ye H.264 编码器 + MP4 muxer 程序内合成，
  encode_h264.rs:26-41 先例）不依赖 ffmpeg，**无条件跑、无 skip 分支**。
- **抽帧点位**（评审问题 11 修正）：90 帧、末帧 pts≈2966667µs。取
  `t = f * 33333 + 5000`（µs），f ∈ **{0, 45, 88}**——三点均落在目标帧
  pts 与后继帧 pts 之间，两侧「取 pts ≥ t 的首帧」语义一致取到帧 f。
  越界时点（如 t=2971667 > 末帧 pts）不在对拍点位内，其「ye 轨明确
  Err / ffmpeg 参考侧 0 字节」的行为由 T2 单测断言（自产向量上做）。

### 3.2 波形对拍（T3 ye 侧、T8 jian 侧同口径）

- 参考侧：`ffmpeg -ignore_editlist 1 -i vec.m4a -map 0:a:0 -f f32le
  -c:a pcm_f32le ref.f32`（**-ignore_editlist 1 是 mov 解封装器私有
  选项，必须位于 -i 之前**，坑 §1.4-5），测试内读 f32 用**同一份分桶
  纯核**推峰值（口径自洽：差异只来自解码器）。ye 轨按裸码流语义输出
  全部含 priming 帧（conformance README:113-116），两侧口径一致的前提
  就是该 flag。
- 我方侧：ye 轨 = audio_peaks_with_count（mp4+aac 注册）。
- 容差初值：每桶每声道 |Δmin| ≤ 2e-4、|Δmax| ≤ 2e-4、|Δrms| ≤ 2e-4。
  依据：AAC 解码对拍口径 100% 样本 |Δ|≤4 LSB(s16)（conformance
  README:119），实测 max_abs=1 LSB ≈ 3.05e-5（:121）——峰值差最坏即
  样本差上界，4/32768 ≈ 1.22e-4，取 2e-4 留余量。桶数取 100（2 秒向量
  桶长 ~960 样本）。**两条向量都要过**（正弦对 min/max 敏感、白噪声对
  错位敏感）。
- jian 侧（T8）：直接分别调 `waveform_peaks_ye` 与
  `waveform_peaks_ffmpeg`（§2.3 拆分），同上容差——回退轨自身带
  `-ignore_editlist 1`（评审问题 4）。

### 3.3 抽帧对拍（T3 ye 侧、T8 jian 侧同口径）

- 参考侧：`ffmpeg -ss {t} -i vec.mp4 -frames:v 1 -f rawvideo -pix_fmt rgba
  -v error ref.rgba`（点位 §3.1：f ∈ {0,45,88}）。
- 我方侧：`decode_frame_rgba`（rgba8，宽高断言相等后逐像素比）。
- 容差初值：100% 通道 |Δ| ≤ 2 且 Δ=0 通道占比 ≥ 99%。依据：H.264 YUV
  口径 100% |Δ|≤1 且 99.9% Δ=0（conformance README:91-94），叠加
  yuv420p→rgba8 BT.601 转换两侧舍入路径差异放一档。PNG 文件本身不做像素
  对拍（encode 无损已有逐字节 0 容差先例，lib.rs:16）——另断言 PNG 魔数
  头与文件非空。
- 不可解音轨向量（vec-video-multi.mp4）：断言 ye 侧整体 Err（§2.2 行为
  锁定），ffmpeg 参考侧可正常出帧——差异即「该类素材走 ffmpeg 回退」
  的依据，不做像素对拍。
- jian 侧（T8）：`extract_frame_ye_impl` 出 PNG 文件（魔数/非空断言）+
  `decode_frame_rgba` 像素对拍（两段各自断言，不解码 PNG）。

### 3.4 probe 双轨判等（T7 单测、T8 集成）

- vec.mp4 上 ye 轨 vs ffprobe 轨：width/height/hasAudio/kind **精确等**；
  durationUs |Δ| ≤ 1000（容器 timescale 换算来源不同）；fps 相对差 ≤
  1e-3。
- vec-cover.m4a（带封面）：ye 轨命中疑似封面启发式 → usedEngine=
  "ffmpeg"，最终结果与显式 engine="ffmpeg" 轨**完全一致**（kind=audio，
  不误报 video）。
- 非 mp4 容器（如 jpg）：auto 落 ffprobe 轨且 usedEngine="ffmpeg"。

## 4. 任务链

```
ye-波形API(T1) ──→ ye-抽帧门面(T2) ──┬→ ye-对拍收口(T3) ──────────────┐
                                     └→ jian-抽帧双引擎(T5) ──→ jian-波形命令(T6) ──┐
jian-ye接线(T4) ─┬→ jian-抽帧双引擎(T5)                                                  ├→ jian-对拍收口(T8)
                 └→ jian-probe双引擎(T7) ─────────────────────────────────────────────┘
```

- T1→T2 串行：同碰 ye 门面 lib.rs（§1.4-7）；
- T5→T6 串行：同碰 jian lib.rs 注册面；
- T7 与 T5/T6 并行：probe 参数式开关不碰 lib.rs（§2.3），probe.rs/probe.ts
  独占；T6 已定桶数直达（不经 probe.rs 也不自跑 ffprobe 定时长），与 T7
  **无文件与 API 耦合**（评审问题 9）；
- T3 只碰 ye tests/ 与文档，与 T4 起点即并行；T8 汇合全部。

## 5. 逐任务详述

### T1 ye-波形API
- 交付：§2.1 波形模块（纯核 + 消费助手含前置流布局校验）+ lib.rs 注册
  （pub mod）+ 单测 + `tests/common/mod.rs`（§2.5 定位器 + AAC 向量
  配方，T3 复用基建）。
- 施工要点：分桶边界公式两入口共享；前置校验用 ContainerRegistry::probe
  读 MediaHeader 判「恰一条音频流」，probe 消费后 seek 回 0 再喂泵；纯核
  测试用构造向量（正弦已知 min/max/rms 解析值、直流、空输入、桶数>样本
  数、交织拆分声道独立性）。
- 验收：ye 三件套（`cargo fmt --check`、`cargo clippy --all-targets --
  -D warnings`、`cargo test`）全绿；另 `cargo test -p ye --features
  mp4,aac` 绿——其中消费助手**真解码路径测试按 §3.1 skip 语义：定位器
  命中则实跑，落空则打印原因合法 skip**（真向量实跑收口归 T3）；纯核
  上述边界各至少一条中文命名测试；流布局校验单测（构造双音轨/纯视频
  header → Err）。

### T2 ye-抽帧门面
- 交付：§2.2 frame_grab 模块 + lib.rs 注册（pub mod）+ 单测。
- 施工要点：取帧即置 cancel 的时序（sink 内置位，泵检查点收 Cancelled，
  门面翻译成功，勿把自然 EOF 误译成功）；pts 缺失语义按 §2.2。
- 验收：三件套 + `cargo test -p ye --features mp4,h264,png` 绿；**测试
  向量程序内自产**（H264Encoder 编 I/P 帧 + MP4 muxer 封装，
  encode_h264.rs:26-41 先例，dev 依赖直连），**不依赖 ffmpeg、无条件
  跑、无 skip 分支**（评审问题 2/3）；单测盖：时点钳 0、越界时点明确
  Err、自产 Baseline mp4 真解码取帧（宽高/rgba 断言）、PNG 出口与
  encode_rgba 逐字节一致。

### T3 ye-对拍收口
- 交付：§3.1 定位器（**复用 T1 的 tests/common/mod.rs，只读不改**）+
  §3.2/§3.3 两个 oracle 测试文件 + 行为锁定向量（vec-video-multi 抽帧
  整体 Err）；ye README 测试基线数更新；waitlist.md:24 波形条目、
  roadmap M2 勾项更新。
- 验收：本机经 duan binaries 实跑对拍全绿（**两条波形向量都过**——
  正弦与白噪声；提交信息记实测 max 差与收紧后容差）；临时指坏定位器
  模拟无 ffmpeg → cargo test 全绿（skip 原因打印）；ye 三件套绿；
  README 基线数已更新；vec-video-multi 断言 ye 侧整体 Err。

### T4 jian-ye接线
- 交付：§2.4 依赖 + Cargo.lock + CI ye 检出（**ci.yml rust-gate；
  qa.yml rust-gate + qa-app-windows；release.yml rust-gate + 打包 job**
  ——全部 Rust 编译点，评审问题 1）。
- 验收：`cargo check --manifest-path jian/src-tauri/Cargo.toml --locked`
  本地绿；上述五个 job/步骤位均含 ye 检出 checkout 与 mv 两步（打包
  job 注释含「tauri build 需 ../ye」）；jian 四件套其余件不受影响
  （前端零改动）。

### T5 jian-抽帧双引擎
- 交付：`src-tauri/src/ye_engine.rs`（`extract_frame_ye_impl` pub 纯库
  主体调 ye::frame_grab、PNG 写 `%TEMP%/jian-frame-{纳秒-计数}.png` 与
  ffmpeg 轨同命名约定（unique_token 复用或同构）+ `extract_frame_ye`
  命令壳，duan ye_engine.rs 同构分层）+ lib.rs **`pub mod ye_engine;`**
  （duan lib.rs:8-10 先例，pub 供 tests 直调）与命令注册 +
  thumbnail.ts ye 优先、失败（含 High/CABAC 能力面外）console.warn 后
  回退 ffmpeg 轨 extract_frame + thumbnail.test.ts。ffmpeg 轨原样保留
  作回退/oracle。
- 施工要点：Rust 错误文案只述能力面外原因、**不声称「已回退」**（回退
  动作与提示归前端，评审问题 16）；回退逻辑抽纯函数便于 vitest（mock
  invoke 或决策函数单测）；临时 png 堆积系已留档低危问题（待开发.md:29），
  双引擎共用同一前缀使堆积翻倍——**不新增治理方案，遗留照记**（T8 在
  待开发.md 该条补注）。
- 验收：jian 四件套全绿；Rust 单测盖时点钳 0 与中文错误文案（能力面外
  语义）；vitest 盖 ye 成功直取与 ye 失败回退两分支；lib.rs 注册面更新
  且模块 pub 化。

### T6 jian-波形命令
- 交付：`src-tauri/src/waveform.rs` —— **两个 pub 纯库轨函数**
  `waveform_peaks_ye`（直达 `audio_peaks_with_count`，桶数精确=请求值）
  与 `waveform_peaks_ffmpeg`（`-ignore_editlist 1` 输入侧 + `-map 0:a:0`
  流式读 f32le，喂同款分桶纯逻辑；ffmpeg 定位经 crate 内 ffmpeg_info()
  复用）+ `waveform` 命令壳（auto：ye 轨 Err 即回退 ffmpeg 轨）+ lib.rs
  **`pub mod waveform;`** 与注册 + `src/core/media/waveform.ts` TS 门面
  与类型 + waveform.test.ts。不做 UI。
- 施工要点：serde camelCase 逐字段对齐 TS interface，注释互指；桶数合法
  域校验（16..=8192）中文报错；两轨返回桶数契约一致（=请求值，尾桶可
  不满）写进注释与测试。
- 验收：jian 四件套全绿；桶数 0/超界中文报错单测；TS/Rust 字段逐字段
  对齐且有双向互指注释；两轨各自可独立调用（pub），回退选择逻辑可单测
  （构造性模拟 ye Err）。

### T7 jian-probe双引擎
- 交付：probe.rs 加 engine 参数与 ye 轨（ContainerRegistry::probe 注册
  mp4 → 时长/宽高/fps/音轨 → **复用既有口径纯函数**：classify_media_kind、
  时长回退，两轨同源不分叉）+ **疑似封面启发式回退**（§2.3：多条视频
  流，或存在音频流且视频流 duration < min(1s, 最长音频 duration×5%) /
  nb_frames=1 → 整体回退 ffprobe 轨；阈值写死测试常量）+ ProbeResult.
  usedEngine + probe.ts 透传 + probe.test.ts。
- 施工要点：ye 容器 probe 的 fps 推导若与 ffprobe 口径有出入，以既有
  ffprobe 轨单测口径为准对齐；core/media/probe.ts:3-4「两端口径必须
  一致」注释不变；封面剔除逻辑仍在 ffprobe 轨（probe.rs:73）不动。
- 验收：jian 四件套全绿；单测四条：mp4 向量双轨判等（§3.4 容差）、
  **vec-cover.m4a 启发式命中回退且结果与显式 ffmpeg 轨完全一致
  （kind=audio 不误报）**、非 mp4 素材 auto 落 ffprobe 且
  usedEngine="ffmpeg"、显式 engine="ffmpeg" 不触 ye；probeMedia 缺省
  参数下 importAssets 行为零改动。

### T8 jian-对拍收口
- 交付：`src-tauri/tests/parity.rs`——ffmpeg 定位器（**§2.5 统一口径：
  BJ_FFMPEG_DIR → PATH → 仓相对 ../../duan/src-tauri/binaries → 全落空
  打印原因跳过**，与 ye 侧同序同名）；向量配方写死（§3.1 全套六条，
  含 vec-cover / vec-video-multi）；三组对拍：抽帧 decode_frame_rgba vs
  ffmpeg -ss 同点位 rawvideo 逐像素（f∈{0,45,88}）、**分别直调
  waveform_peaks_ye 与 waveform_peaks_ffmpeg**（§2.3 拆分的接缝，评审
  问题 8）同容差对拍（两条波形向量都过）、probe 双轨判等（含封面
  回退行为）；更新 jian README 门禁与对拍说明、docs 三层勾项（待开发.md
  E 项与目标与里程碑.md E1；**待开发.md:29 临时 png 条目补注双引擎
  翻倍**，评审问题 15）、在本施工图补收尾实录。
- 验收：本机经 duan binaries 实跑 parity 三组全绿（提交信息记录实测
  max 差）；模拟无 ffmpeg 时 `cargo test --manifest-path
  jian/src-tauri/Cargo.toml --locked` 全绿（skip 打印）；jian 四件套
  全绿；待开发.md 相应项已勾并补注、m2-jian-plan.md 收尾实录已补。

## 6. 不做（范围外，明确出界）

- **CABAC/High 真流对拍**：ye M2 遗留（waitlist.md:23），High 素材经
  jian 回退 ffmpeg 轨覆盖，不阻塞本轮准入。
- **剪的波形 UI 完整形态**：未排期，本轮只交付命令层 + core 门面 +
  对拍；UI 待排期后另图。
- **M4 导出接管**（buildExportArgs 降级为「求值 → 调 ye」）：见
  roadmap M4，本轮 run_export 纯 ffmpeg 不动。
- **convert_frames 泵级选流**（多音轨素材的 ye 轨波形、含视频素材免解
  视频流、含不可解音轨素材的 ye 轨抽帧）：本轮以「前置流校验 Err→
  jian 回退」兜住，泵 API 扩展归 M4（评审问题 7/13 的自然延伸）。
- **波形桶数入口的流式化**（长素材内存优化）：M4。
- **多声道 AAC 的 ye 解码**：ye 能力面 M5+，jian 恒走 ffmpeg 回退轨。
- **PNG 解码器**：ye 只 encode，对拍不需要。
- **抽帧临时 png 清理治理**：已留档低危（待开发.md:29），本轮只补注
  双引擎翻倍，不新增方案。

## 7. 门禁与收口

- 每任务完成即提交推送自己仓库（父仓库 AGENTS.md 约定：先子模块后
  父仓库推指针；qa 红线不动）。
- 收口顺序：T3 绿（ye 轨自证）→ T8 绿（jian 轨双引擎互证）→ 父仓库
  更新 ye/jian 双指针一次提交。
- 收尾实录（T8 补写）：逐任务结果、两仓对拍实测数字（波形 max |Δ|、
  抽帧 max |Δ| 与 Δ=0 占比、probe 判等）、门禁清单、遗留。

> **收尾实录**（T8 补写，2026-10-27）：
>
> **逐任务结果**（8 任务全部交付）：T1 波形 API（纯核 + 消费助手 + tests/common
> 基建）、T2 抽帧门面（自产向量无条件跑）、T3 ye 侧对拍（波形两向量全绿，容差
> 2e-4 实测收紧至 1.5e-4；抽帧像素/YUV 对拍实测超容差——归因 ye H.264 解码器
> 与 ffmpeg 在 §3.1 冻结向量上的两处独立分歧〔去块启用首差帧 0 luma、同配方
> 关去块仍首差帧 81 chroma，90 帧 max_abs=74，rgba 域三点位 21/35/130；像素核
> |Δ|≤1 与帧定位已排除〕，解码器修复另行排期，两用例 `#[ignore]` 如实交付
> （**同日晚班修复收口、翻除过门——见本实录末「解码器分歧修复收口」补记**）；
> vec-video-multi 整体打回锁定生效）、T4 jian ye 接线（Cargo 路径依赖 + CI 全部
> Rust 编译点检出）、T5 抽帧双引擎、T6 波形命令、T7 probe 双引擎（含启发式③
> covr 盲区兜底：实测 mp4 家族 attached_pic 封面写 moov.udta.meta 的 covr 原子，
> ye 头整条不可见；AAC priming 使 ye 时长 2026667µs vs ffprobe 2000000µs——
> 音频族整体回退权威轨）、T8 jian 对拍收口（本实录）。
>
> **jian 对拍实测数字**（duan binaries ffmpeg N-127696，2026-10-27，
> `jian/src-tauri/tests/parity.rs` 8 用例）：
> - **波形双轨**（`waveform_peaks_ye` vs `waveform_peaks_ffmpeg`，100 桶，
>   容差 1.5e-4）：正弦 vec-audio |Δmin|=1.840e-6、|Δmax|=5.022e-6、
>   |Δrms|=4.470e-8；白噪声 vec-noise |Δmin|=3.588e-5、|Δmax|=1.168e-5、
>   |Δrms|=2.384e-7——**两向量全过**（提交信息留档数字）。
> - **抽帧**：像素对拍 `#[ignore]`（f=0 在 jian 集成路径复测 max |Δ|=21、
>   Δ=0 通道占比 0.77042，与 ye 侧逐位同数；f=45/88 承 ye 侧同向量实测
>   35〔0.817〕/130〔0.708〕——容差 |Δ|≤2 且 Δ=0≥99% 未达，归因解码器，
>   同 T3 裁决；**同日 ye 侧修复收口后，本用例未随翻除、仍挂 `#[ignore]`——
>   遗留①，翻除复跑归 jian 后续**）；PNG 出口（`extract_frame_ye_impl`
>   临时 png）魔数/非空 ✓；
>   vec-video-multi ye 整体 Err（channelConfiguration=6 述因）+ ffmpeg 参考
>   同点位可出帧 ✓。
> - **probe 双轨**（vec.mp4）：宽高 320x240 / kind=video / hasAudio=false
>   **精确等**，durationUs 两轨均 3000000µs（|Δ|=0 ≤ 1000），fps 两轨均
>   30.0000（相对差 0 ≤ 1e-3）；带封面 vec-cover.m4a：auto 命中启发式回退
>   ffprobe（usedEngine="ffmpeg"、kind=audio、durationUs=2000000µs，与显式
>   ffmpeg 轨逐字段一致）；非 mp4（cover.jpg）：auto 落 ffprobe
>   （kind=image、零时长零帧率）。
>
> **门禁清单**（T8 实跑）：jian 四件套全绿——`cargo check --locked` ✓、
> `cargo test --locked` ✓（lib 单测 46 + parity 7 过 1 ignored）、`npm test` ✓
> （9 文件 77 用例）、`npm run build` ✓；模拟无 ffmpeg（定位器视角三档全落空：
> 仓相对档临时指坏 + PATH 剔除 ffmpeg + BJ_FFMPEG_DIR 未设；生产侧 ffmpeg_info
> 以 sidecar 副本保持 T6/T7 模块内真跑组可跑，模拟后还原）→ 全量
> `cargo test --locked` 全绿，parity 打印落空原因合法 skip。ye 侧门禁由
> T1/T2/T3 各任务验收时自证（T8 只动 jian 代码与两仓文档，ye 代码零改动）。
>
> **越白名单改动**：`jian/src-tauri/src/lib.rs`——T8 施工中经运行主理人裁决扩权：
> `mod probe;` → `pub mod probe;` 一行 + 相邻注释同步（§1.4-9 规划只 pub 化了
> 新模块、漏了 probe；parity.rs 的 probe 双轨判等须直调 `probe_media`。裁决
> 留痕于 T8 施工记录，收尾提交把它纳入 git add 清单）。
>
> **解码器分歧修复收口**（同日晚班补记，收尾文秘核实留档）：早班 T3/T8
> 以 `#[ignore]` 登记的 H.264 Baseline 解码两处分歧，当日修复收口
> （ye-codec-h264）：
>
> - **去块 I 帧（首差帧 0 luma 根因之一）**：解码期行滞后去块使下一行帧内
>   预测读到滤波后样本——改为**图级滤波**（解码期一律不滤，图完成后按
>   光栅序统一 `deblock_picture`；spec 8.4.4 全图解码后才去块，FFmpeg 的
>   行滞后调度经对拍锁定与「图完成时统一滤波」逐位等价）；编码器本地
>   重建同日对齐同调度（位级回环保持 0 容差）。
> - **chroma QP 偏移（同根因之二）**：去块 chroma 边 QP 漏加 PPS
>   `chroma_qp_index_offset`（x264 默认 −2），chroma 阈值整档偏移——补上
>   平移（FFmpeg `get_chroma_qp` 逐字）。
> - **pred_8x16 C→D 回退（长链色度，首差帧 81 chroma 根因）**：n=1 捷径的
>   C 候选右上不可用时以左上 D 顶替（spec 8.4.1.3 / FFmpeg
>   `fetch_diagonal_mv`），右缘宏块不再误落中值分支。
>
> **回归与对拍最终状态**：新回归集 `ip30`（§3.1 冻结配方 + 其 no-deblock
> 变体，90 帧 30fps 单 GOP 长链、ref=3、frame_num 5 次回绕）入 conformance
> 清单——去块条 90 帧 max_abs=0 逐位一致；no-deblock 条 max_abs=1（帧 81
> 起 Cr 平面孤立 ±1 样本，300 帧扩展实测 24 帧 × 每帧 ≤2 样本，M2 YUV 容差
> 〔100% |Δ|≤1 且 ≥99.9% Δ=0，实测 ≥99.998%〕内过门；二轮复核已排除
> MC/参考选择/残差反量化/idct 语义，ffmpeg 二进制输出（含 `-cpuflags 0`
> 纯 C 路径）在涉事块上偏离其公开 C 源数学且 300 帧内偏差双向出现——归档
> ffmpeg 内部舍入边角，无反汇编/自建 ffmpeg 无法进一步定位，记录见
> ye-codec-h264 tests/conformance.rs `ip30` 注释与 tests/conformance/
> manifest.toml 施工登记）。早班两条 `#[ignore]` 用例（frame_grab_oracle
> 像素 + 解码 YUV）**翻除过门**：像素三点位 max |Δ|=1、Δ=0 通道占比
> ≥0.9998（余量全部来自 yuv420p→rgba 两侧舍入——YUV 级同向量去块配方
> 逐位一致）；早班超差数字 21/35/130 与 74/76 全数消除。门禁：ye 三件套
> （`cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` /
> `cargo test --workspace`）全绿（测试基线数见 ye README）。
>
> **遗留**（2026-10-27 晚班终态，覆盖早班登记）：① 抽帧像素对拍——ye 侧
> 已修复收口、两用例翻除过门（见上补记）；**jian 侧 `parity.rs` 同款像素
> 用例仍挂 `#[ignore]`（早班超差登记未随修复翻除）**——jian 经路径依赖已
> 链到修复后的 ye，翻除复跑预期过门，归 jian 后续小步收尾（本轮未改 jian
> 测试代码，如实登记）；② no-deblock 长链 Cr ±1 余留（容差内，归档
> ffmpeg 舍入边角）——收紧 `ip30` 容差前须先收口此余留；③ 抽帧临时 png
> 堆积双引擎翻倍——已补注 jian docs/待开发.md，治理仍归该条不另立方案；
> ④ CABAC/High 真流对拍、多音轨/多声道素材的 ye 轨波形与含不可解音轨素材
> 的 ye 轨抽帧（泵级选流）——§6 范围外归 M4/M5；⑤ E1 准入的「剪 M1 已
> 定稿」欠 GUI 人工回归（E1 代码面已达成）。

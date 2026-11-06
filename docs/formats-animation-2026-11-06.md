# MOV、WAV 与关键帧首批能力（2026-11-06）

本轮只修改冶及父仓库指针。剪、锻源码、依赖、锁文件和流水线均不更新；
新增能力需未来接线后才会出现在产品中。

## 格式范围

| 能力 | 已实现 | 仍不支持 |
| --- | --- | --- |
| MOV 导入 | qt 品牌及经典无 ftyp QuickTime；H.264/AAC；v0/v1 音频描述、wave/esds；探测、seek、抽帧、波形、转 MP4/WAV | HEVC、ProRes、QuickTime PCM 等其它编码；v2 音频描述；MOV 输出 |
| WAV 导入/导出 | RIFF 小端；U8、S16/S24/S32、F32；单/双声道；标准 fmt 与有效位等于存储位的 WAVE_FORMAT_EXTENSIBLE；样本级 seek | RF64/RIFX、压缩 WAV、多声道、多个 data chunk、有效位小于存储位；RIFF 超 4 GiB |
| WAV 转 AAC/MP4 | 保持源采样率/声道，输出呈现时长排除末帧 AAC 填充 | 非 AAC 支持采样率会明确拒绝；改采样率仍限已有 44.1/48kHz 路径 |
| 音轨转 WAV | 显式容器流编号；排除 priming/padding；保留正起点前的静音；进度/取消/事务落盘 | 不做变速或保持音高处理 |

PCM 运行时均在 Rust 内完成，不调用 FFmpeg。F32 中间管线有意保留有限的
超范围样本；整数输出会截幅。S32 解码后进入 F32，会损失低位精度，不能
声称 S32 解码再编码位级无损；需要保留全部整数位时使用原始包重封装。
WAV 波形保持源采样率；时间线混音目前仍只接受 44.1/48kHz 单/双声道。

## 门面接入

MOV 随已有 `mp4` feature，无需新依赖。WAV 显式启用 `wav` feature，
不强制启用 MP4/H.264/AAC；默认 feature 保持不变。

```toml
ye = { git = "https://github.com/Bai-Gong/ye.git", rev = "<本轮已推送提交>", default-features = false, features = ["mp4", "h264", "aac", "png", "wav"] }
```

`container::register_enabled` 注册已启用容器；`register_decoders` 注册已启用
解码器。编码注册保持显式；PCM 可用 `codec_pcm::register_pcm`（默认 S16）
或 `PcmEncoder::new(config, format)`。`media::inspect` 和波形助手已使用
新的注册入口，不按扩展名判断格式。

`audio_export::export_wav(input_path, output_path, options, cancel, progress)`
是安全文件入口；`WavExportOptions` 指定 `stream_index` 与 `PcmFormat`。
None 只接受一条音轨，绝不静默选第一轨。输出保持源采样率/声道，按呈现
时钟导出。`write_wav` 是面向自定义读写器的底层入口，失败可能留下部分
字节；调用方负责输出事务。每批最多处理 1024 个输出样本，内存不随整片
长度累计。RIFF 体积超限会提前拒绝；取消/失败不提交临时文件。

命令行：

```bash
ye probe input.mov
ye probe input.wav
ye convert input.wav output.mp4 --no-video --audio aac:128
ye export-wav input.mov audio.wav --format s24 --stream 1
```

CLI `convert`、`remux`、`export-wav` 均保护已有输出；取消或失败只清理自己
的临时文件，重跑需使用新输出路径。

## 关键帧和转场

新增 `animation::{Keyframe, KeyframeTrack, Interpolation, ClipAnimation,
TimelineOptions}`；原 `TimelineClip`、`Transform`、`AudioMix` 字段不变。
原 `check_timeline`/`render_frames`/`render` 保留静态行为，动画使用对应的
`*_with_options` 入口。

- 视频支持 x/y/scale/rotation/opacity；音频支持 gain/pan。z 顺序保持静态。
- 关键帧时点是**片段在时间线上的局部时间**，不受源裁剪入点影响；支持
  Linear 和 Hold，区间外钳到端点。Hold 在精确关键帧时点切换。
- 视频在精确输出帧时点求值，音频在每个 48kHz 输出样本求值；不先将
  分数帧时间舍入成整数微秒。
- options.clips 为空表示全静态；非空时按输入片段原始顺序一一对应，
  数量必须一致，随后按 z 排序不改变绑定。
- 空关键帧、重复/倒序时点、超片段范围、非有限值、错误层类型和越界属性
  一律在预检中拒绝。插值保持在合法端点范围内，避免极小 scale 下溢为零。
- `fade_in_us` / `fade_out_us` 是线性包络，乘在 opacity/gain 上；两段
  长度之和不得超过片段时长。音频混合在 f64 中累加，最终截幅转 F32，
  避免有限极值样本在多轨混合中产生 NaN。

**视频溶解**：让下层片段保持不透明，在重叠区让上层从 opacity=0 淡入；
不能同时把两层透明度相减/相加后误称等价，因为 source-over 会露出背景。
**音频交叉淡化**：两段重叠音频使用互补的淡出/淡入，或显式 gain 轨；
当前是线性曲线，不是恒功率曲线。

后续优先项仍包括保持音高的变速、HEVC/常见独立压缩音频、图片/字幕、
更完整转场与效果链。本批没有把这些未实现项移出等待清单。

## 验证方式

真实 QuickTime H.264/B 帧＋AAC 造样，验证探测、显示时间 seek、逐包
MOV→MP4 重封装及外部完整解码；门面进一步验证 MOV/MP4 同流抽帧逐字节
一致、波形与 WAV 导出。坏音频描述、空/重复配置、v2、未知编码明确拒绝。

WAV 测试包含五种格式×单/双声道的外部参考，PCM F32 解码和原始包重封装
逐字节比较；另测坏 RIFF/块长度/对齐/扩展描述、非有限样本、seek、4GiB
限制、已有输出、取消清理和无外部工具的 CLI WAV 输出。

时间线测试包含离线像素/PCM黄金值、局部时间、z排序、分数帧边界、逐样本
增益/声像、溶解/交叉淡化、非法输入/取消，以及 H.264/AAC 编码封装后外部
完整解码。实际 PCM WAV 也已作为时间线音频源验证；多音轨带正时间偏移
导出 WAV 验证了选择正确音轨、前导静音、有效样本数。

本机（Apple M4 / macOS arm64、Rust 1.97.0、FFmpeg 9.0.2）最终结果：

- `cargo test --workspace --release --all-features --no-fail-fast`：
  **786 passed / 0 failed / 0 ignored**，本机参考工具在场；未降低既有容差。
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` 与
  `cargo fmt --all --check`：通过。
- 无默认 feature、仅 `wav` 的严格 clippy：均通过；仅 WAV 的三个门面
  集成测试通过，不依赖 MP4/AAC；CLI WAV 导出在空 PATH 下通过。
- 独立审查补齐空嵌套 MOV 配置拒绝、WAV 非空底层输出拒绝、最小正
  subnormal 缩放插值保界、极大有限 F32 混音防 NaN 回归。

首次全仓运行发现旧 remux 测试使用固定输出路径，与新的不覆盖契约冲突；
测试改用每次独立输出目录。WAV-only 并发测试也补了原子序号，避免时钟精度
导致临时目录重名。最终统计为修复后重新执行结果。

日志保留在 `target/formats-animation-workspace-final.log`、
`target/formats-animation-clippy-final.log`、`target/wav-only-test.log`，
生成素材和日志不入库。

# 剪、锻接入准备（2026-11-06）

本轮完善冶，不改动剪、锻的产品仓库。默认能力边界仍是 MP4/fMP4、
progressive 8-bit 4:2:0 H.264、AAC-LC 和静态多轨时间线；不代表
HEVC、所有 H.264 形态或高级编辑已经支持。实测结果见
[本轮验证记录](validation-2026-11-06.md)。

## 依赖与调用

消费方只依赖 `ye` 门面，固定已推送提交并提交 Cargo.lock，保持
`default-features = false, features = ["mp4", "h264", "aac", "png"]`。
锻当前为应用骨架，未保留历史转码后端；重建时使用下列现有入口，不应
把旧锻版本的验收当成新骨架已经接通。

| 用途 | 入口 | 调用方责任 |
| --- | --- | --- |
| 探测、能力检查 | `media::inspect`、`media::validate_decoding` | 按真实媒体头判断；预检不保证后续包无损坏 |
| 单张缩略图 | `frame_grab::decode_frame_rgba` / `grab_frame_png` | 首个精确 PTS ≥ 请求时点；超尾报错 |
| 连续胶片条 | `frame_sequence::decode_frames_rgba` | 请求按时间递增；消费回调中的图片，不累计原尺寸图片 |
| 指定音轨波形 | `waveform::audio_peaks_aligned_stream_with_count` | 使用容器实际流编号，保留呈现时钟 |
| 多层时间线 | `timeline::check_timeline` / `render` | 明确源流与层级；不支持的高级属性先拒绝 |
| 转码 | `convert::check_convert` / `convert_with_options` | 保留取消与失败原因，勿在预检时创建目标文件 |
| 安全落盘 | `output::OutputTransaction` | 全部成功后 commit；不覆盖已有文件 |

## 连续抽帧

`frame_sequence::decode_frames_rgba(input, times_us, stream_index, cancel, sink)`
只打开一次输入、从首个请求前的关键帧开始顺序解码；不创建外部进程，
不缓存整段视频。只转换选中帧，同一帧对应多个请求时复用转换结果。

- 负时点钳到 0，钳制后必须非递减；允许重复。
- `None` 仅接受恰一条视频流；`Some` 是容器实际流编号，不是第几条视频。
- sink 接收请求索引与借用的 `GrabbedFrame`。可立即缩到 192×96 内并编码
  PNG；PNG/缩放失败用 `ConvertError` 返回。
- 取消、解码失败、回调失败立即停止后续交付。末尾请求越界时返回错误，
  早先已经交付的图片仍属于部分结果，调用方不可将整批标为成功。
- 适合相邻可见格；跨很远的稀疏时点仍可使用单张 seek。调用方继续限制
  并发与缓存数量，并在视口失效时设置取消标记。

## 升级时的参考测试口径

这些调整不降低像素或音频容差，也不进入默认产品的外部工具依赖。

1. 裸 AAC 解码/原始波形与呈现波形不同。裸样本参考对 MP4 使用
   `-ignore_editlist 1 -flags2 +skip_manual`（均在 `-i` 前）；FFmpeg 9
   即便忽略 edit list 仍会按尾包 duration 裁 padding。呈现波形和播放
   不应因此跳过正确的 priming/padding 裁剪。
2. YUV→RGB 统一为 13 位定点矩阵、相加后一次舍入，保留黑白端点及
   BT.601/709 的 full/limited 区别。参考明确指定
   `-cpuflags 0 -sws_flags neighbor+full_chroma_int+accurate_rnd`，不依赖
   默认 x86/ARM 实现的不同舍入。旧逐项 floor 的 RGB 值可能变化 1–2。
3. 全 YUV 字节立方体的随机核测试使用 RGB48 参考后量化，避免当前参考
   工具 packed RGB8 极端色度溢出；实际视频抽帧仍直接比较 RGBA8。

剪的隔离副本已确认需要的原始波形参考最小修改（`run_ffmpeg_f32le`
中原有 `if ignore_editlist` 分支）：

```rust
cmd.args(["-ignore_editlist", "1", "-flags2", "+skip_manual"]);
```

兼容功能的 `parity.rs` 抽帧参考也需要固定上述颜色参数。合入时应同步
调用侧参考与相关注释，不能通过放宽容差掩盖参考口径不一致。

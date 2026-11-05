# 2026-11-05 冶本机验证与性能初测

本轮仅完善冶；按用户要求暂停剪/锻接入，不考虑旧录屏和 Windows 环境。
历史安装/听感验收未因此转为通过。

## 解码修复

High/CAVLC 8×8 原色块用例在第 30 包报 `intra pred mode 8`。
补齐变换标志、8×8 帧内模式、四组交错残差、重建和去块强度后，下列
四组各 45 帧的 Y/Cb/Cr 均与参考逐样本一致：

- 红→蓝→白加权 P_Skip，`cabac=0:8x8dct=1`。
- `testsrc2` 全 I 帧。
- `testsrc2` I/P 运动，去块开启。
- 同类 I/P 运动，去块关闭。

新增去块单测验证：仅一组交错系数非零时，整个 8×8 空间块使用非零残差
边界强度，相邻变换块不受影响；nC 预测仍保留独立组计数。

## 全仓验证与既有失败

环境：Apple M4 / macOS arm64；rustc 1.97.0；Homebrew FFmpeg 9.0.2、
x264 r3222。参考工具只用于造样与验证，不进入冶运行时。

执行 `cargo test --workspace --release --all-features --no-fail-fast`：

| 版本 | 通过 | 失败 | 忽略 |
| --- | ---: | ---: | ---: |
| 改动前 `47e94b6`，独立 detached worktree | 735 | 9 | 0 |
| 本轮修复 | 737 | 9 | 0 |

失败用例集合相同；没有新增失败，但不能声明全仓通过。严格 clippy
（workspace/all-targets/all-features，`-D warnings`）及格式检查通过。

| 失败目标 | 数量 | 观察 |
| --- | ---: | --- |
| `ye-codec-h264 / cabac_oracle` | 2 | I/P 帧 41 一个样本、B 帧 1 六个样本存在 ±1 差异 |
| `ye-filter / video::convert` | 2 | RGB24/RGBA 与当前 swscale 不一致 |
| `ye / frame_grab_oracle` | 2 | 通道差最大 2，但精确一致占比低于既有门限 |
| `ye / waveform_oracle` | 2 | RMS 超过 0.00015 门限，min/max 接近参考 |
| `ye-codec-aac / conformance` | 1 | m4a PCM 长度 536576/533296 不等 |

向量按仓库 manifest 配方在本机重新生成；因参考编码工具版本不同，生成
文件 SHA 与历史清单不同，未改写清单、未降低对拍门限。下一轮应先区分
参考工具/裁尾口径与解码算法问题，不能仅凭旧版也失败就认为结果正确。

本机日志：`/tmp/baigong-ye-workspace.log`、
`/tmp/baigong-ye-baseline-workspace.log`、`/tmp/baigong-ye-clippy.log`。
日志和生成素材不入库。

## 性能初测

仅一次本机观测，非独占机器测量，不作为性能承诺。参考素材无音频：
1920×1080、60fps、12 秒、720 帧；x264 veryfast/CRF23、GOP120、B 帧3。
生成命令：

```bash
mkdir -p target/perf-baseline-20261105
ffmpeg -v error -f lavfi -i testsrc2=size=1920x1080:rate=60:duration=12 \
  -c:v libx264 -preset veryfast -crf 23 \
  -x264-params keyint=120:min-keyint=120:scenecut=0:bframes=3 -an \
  target/perf-baseline-20261105/source.mp4
cargo build -p ye --release --all-features --example recording_acceptance
/usr/bin/time -l target/release/examples/recording_acceptance \
  target/perf-baseline-20261105/source.mp4 target/perf-baseline-20261105/frames \
  0 1950000 5950000 11950000
cargo build -p ye-cli --release
/usr/bin/time -l target/release/ye convert \
  target/perf-baseline-20261105/source.mp4 \
  target/perf-baseline-20261105/converted.mp4 --size 320x180
```

输出路径必须不存在；重跑用新目录，不覆盖已有产物。

| 操作 | 观测 |
| --- | --- |
| 0 秒抽帧 | 0.381 秒 |
| 1.95 秒抽帧 | 2.503 秒 |
| 5.95 秒抽帧 | 2.463 秒 |
| 11.95 秒抽帧 | 2.397 秒 |
| 四次抽帧进程峰值 RSS | 452,018,176 字节（约 431 MiB） |
| 全片缩至 320×180、保持 60fps 转码 | 34.21 秒（约 21 帧/秒） |
| 转码峰值 RSS | 333,234,176 字节（约 318 MiB） |

抽帧计时包括重新打开素材、seek/解码、RGBA 转换、PNG 编码及磁盘写入，
不是纯解码耗时。转码输出由外部参考完整解码无错，并确认 720 帧、12 秒、
320×180。尚未对这份性能素材做全部像素对拍，不等同于质量验收。

下一步先给抽帧内部步骤分别计时，确定解码、转换或输出的占比，再决定
连续缩略图共用解码游标或其他优化；不凭总时间直接改写底层算法。

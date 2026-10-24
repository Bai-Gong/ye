# 一致性测试（对标 FATE 的最小可用层）

解码/码流层正确性以系统 FFmpeg 为 oracle 逐项对拍；测试向量**不入库**，
本目录只维护清单与工具：

| 文件 | 性质 | 说明 |
| --- | --- | --- |
| `manifest.toml` | 入库 | 向量清单 v1：就位集（生成配方/下载源 + 实测 sha256）+ 待网络清册 |
| `fetch.sh` | 入库 | 获取/生成脚本：generate 型按配方重放（`{OUT}` 替换缓存路径），source 型 curl 下载；sha256 对账 |
| `src/`（`ye-conformance`） | 入库 | runner crate：sha256/清单解析/YUV/PCM 对拍器可离线单测（`cargo test -p ye-conformance`），bin 编排 oracle 对拍 |
| `.cache/` | 不入库 | 本地向量缓存（`fetch.sh` 生成/拉取） |

## 运行

```bash
bash tests/conformance/fetch.sh              # 1) 生成/取向量
cargo run -p ye-conformance                  # 2) 对拍（默认 manifest/.cache 在本目录）
```

`--manifest PATH` / `--cache DIR` 可覆盖；`YE_CONFORMANCE_CACHE` 环境变量
同效。退出码：0 无失败（skip 合法）；1 有对拍失败；2 清单/用法错误。

## 向量清单（2026-10-24 施工图①裁决）

- **就位集**（`[[vector]]`，6 条）：外网被墙（实测 curl code=000）无法
  下载 FATE 向量，以本机 ffmpeg + 确定性 lavfi 源（testsrc2/smptebars，
  176×144/640x360/1280×720）自造；每条记完整生成命令、ffmpeg 版本
  （`ffmpeg -version` 首行）与**实测** SHA256（禁止推测值）。
- **待网络清册**（`[[pending_vector]]`）：FATE H.264 conformance 的
  Baseline 子集（ba*/B* 系列），登记来源 URL；不参与 ran/skipped 计数
  与终验断言。网络恢复后下载、remux mp4、实测 sha256 升入就位集。
- 增删向量须登记理由，选择权不在施工方。

## 摘要行（终验硬断言的接口，格式冻结）

每次运行末尾必为两行：

```text
CONFORMANCE ran=N skipped=K failed=M sha_ok=P
CONFORMANCE pending=Q（待网络清册，不参与断言）
```

- `ran`：实际执行对拍的**就位集**向量数（含失败）；`failed ≤ ran`；
- `skipped`：未执行（`not_fetched` / `oracle_missing` /
  `unsupported_codec`）——**开发期向量取不到允许 skip，但必须落本行**，
  漏跑不许静默；
- `sha_ok`：sha256 对账通过数。缓存文件在而 sha 不符（如换了 ffmpeg
  版本重生成）**不当 skip**——向量照跑、计 `sha_ok=false`：sha 漂移是
  信号，不是可静默跳过的常态（裁决第 3 条）；
- `pending`：待网络清册条数（单独一行，不参与断言）。

摘要行机制本身的验证方法：缓存里放一个真实向量跑一遍（如
`ran=1 skipped=1`），删掉该向量再跑（应得 `ran=0 skipped=2`）——
施工图①已现场执行过一遍（含 sha 漂移照跑的 `sha_ok=5` 场景），计数
语义另由 `report::tests::removing_a_vector_shifts_ran_to_skipped` 锁定。

## 容差（按 codec 逐条写死，**只准收紧**）

见 `src/tolerance.rs`：向更严方向修改免评审；任何放宽（bit-exact →
PSNR 下限、提高下限、放宽比较维度）必须回施工图重新定口径。当前
h264：NAL 序列（vs `h264_mp4toannexb`）与 SPS 字段
（vs `ffprobe -show_entries stream=profile,level,width,height`）均
bit-exact。M2 解码落地后追加逐帧 YUV bit-exact（`src/compare.rs`
的比较器已备：逐平面 max-abs/PSNR、PCM 逐样本 max-abs）。

## h264 对拍口径细节

- 我方序列 = `ye-container-mp4` 解 AVCC + `ye-codec-h264` 拆 NAL（包内
  NAL 逐包拼接）；
- ffmpeg 侧按 `h264_mp4toannexb` 的参数集语义对齐：该 bsf 只在**首个
  IDR 前缺带内 SPS/PPS** 时把 avcC（extradata）参数集插到 IDR 紧前
  （包首有 SEI 时插在 SEI 之后）——我方照同语义用 avcC 记录补插后
  逐 NAL 比较；
- 尾 NAL 的 `trailing_zero_8bits` 两侧归一化（起始码切分语义差异）；
- SPS 的 profile 命名按 ffprobe 口径映射（66+constraint_set1 →
  Constrained Baseline……），level 数值判等（ffprobe 有 `30`/`3.0`
  两种记法）；
- 排查差异：`YE_CONFORMANCE_DEBUG_NALS=1 cargo run -p ye-conformance`
  打印两侧逐 NAL 的 (序号, type, len)。

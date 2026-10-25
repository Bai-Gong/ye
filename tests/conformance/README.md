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
cargo run -p ye-conformance -- --set intra   #    施工图②子阶段集选通（intra / ip / all）
```

`--manifest PATH` / `--cache DIR` 可覆盖；`YE_CONFORMANCE_CACHE` 环境变量
同效。退出码：0 无失败（skip 合法）；1 有对拍失败；2 清单/用法错误
（含 `--set` 选中 0 条——拼错集名不许假绿，见「集选通」）。

## 集选通（施工图②子阶段门禁，2026-10-24）

`--set NAME` 按清单 `set` 标签选通：`intra` = ②a 纯 I 集（3 条）、
`ip` = ②b I/P 混合集（3 条）、不带 `--set` / `--set all` = 全集
（②c，12 条 = v1 六条 + intra 三条 + ip 三条）。只有选中的向量进
ran/skipped 计数，运行头行打印 `set=… selected=N/总`；**选中 0 条按
用法错误退出 2**——②a/②b 过界断言是「某集全部对拍过」，集名拼错
若静默退出 0 会让门禁假绿。

## 向量清单（v1 = 施工图①就位集 + 施工图②增补）

- **v1 就位集**（`[[vector]]` 无 `set` 键，6 条）：外网被墙（实测 curl
  code=000）无法下载 FATE 向量，以本机 ffmpeg + 确定性 lavfi 源
  （testsrc2/smptebars，176×144/640x360/1280×720）自造；每条记完整
  生成命令、ffmpeg 版本（`ffmpeg -version` 首行）与**实测** SHA256
  （禁止推测值）。
- **②a 纯 I 集**（`set="intra"`，3 条，testsrc2 三档分辨率）：施工图
  原文配方 `ffmpeg -intra` 在本机 ffmpeg（N-123627-g5dfe661f03-20260327）
  已移除（实测 Unrecognized option 'intra'），按其语义等价形式 `-g 1`
  生成；ffprobe 实测 45/45 帧 pict_type=I 且 key_frame=1、
  Constrained Baseline（三档 level 1.0/2.2/3.1）。每向量 45 个 IDR——
  多 IDR 的 NAL 参数集插入语义即由本集实测锁定（见「h264 对拍口径
  细节」）。
- **②b I/P 混合集**（`set="ip"`，3 条）：`-g 15`，ffprobe 实测 45 帧 =
  3×(1I+14P)、I@0/15/30、无 scenecut 插帧、Constrained Baseline；P 帧
  含真实运动（testsrc2），运动补偿/参考帧链有效受压（静态图的 P 帧
  几乎全 skip，不构成 ②b 压力，故不用 smptebars）。
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

`--set` 选通运行时摘要行只数**选中集**的向量（选通不是 skip，选中面
在运行头行明示）；逐帧 YUV 口径「未接线」（我方解码输出缺席）不打断
摘要行语义——它不是向量级 skip，只在该向量的明细里落 NOTE。

## 逐帧 YUV 对拍（施工图②，M2 解码落地后的主口径）

- **参考侧**：`ffmpeg -i <vec> -pix_fmt yuv420p -f rawvideo ref.yuv`
  （runner 内加 `-map 0:v:0 -v error`，语义同施工图原文）；输出为
  **显示尺寸**（含 frame cropping）的 yuv420p 平面布局，帧尺寸取自
  我方 avcC SPS 的 `Sps::width()/height()`（与 ffprobe 同口径）。
- **我方侧契约**：解码输出写 `<cache>/<name>.our.yuv`（显示尺寸
  yuv420p，逐帧顺序拼接，无 padding）。**文件缺席时本口径不参与该
  向量 pass/fail**，明细落 NOTE——②a/②b 解码接线后生成该文件即自动
  加入对拍（②a 门槛：`--set intra` 全 PASS；②b：`--set ip`；②c：
  不带 `--set` 跑全集）。解码侧也可直接调 lib（`compare::diff_yuv_frames`
  + `tolerance::Tolerance::check_yuv_frames`）自建测试。
- **容差**（施工图②原文定死，`tolerance.rs` 落地）：100% 像素 |Δ|≤1
  且 ≥99.9% 像素 Δ=0，**逐帧判定**（每帧全部样本 |Δ|≤1 且该帧 Δ=0
  占比 ≥999‰——单帧劣化不能被其他帧摊薄）。只准收紧：降 max_abs /
  升占比下限 / 收为 bit-exact 免评审，任何放宽须回施工图。
- **失败归档**（四元组：向量名/首差帧/首差宏块位置/max-abs）：
  超容差时自动落 `<cache>/failures/<name>.fail.txt`（另含容差判据行
  与帧数/帧长），FAIL 明细行同步给出摘要；向量重新通过时旧归档自动
  清除。宏块位置为 16×16 亮度宏块网格（色度样本按 420 采样比换算）。

摘要行机制本身的验证方法：缓存里放一个真实向量跑一遍（如
`ran=1 skipped=1`），删掉该向量再跑（应得 `ran=0 skipped=2`）——
施工图①已现场执行过一遍（含 sha 漂移照跑的 `sha_ok=5` 场景），计数
语义另由 `report::tests::removing_a_vector_shifts_ran_to_skipped` 锁定。

## AAC 对拍口径（施工图③，2026-10-25）

- **向量**（`codec="aac"`，6 条，容器 `adts`/`loas`/`mp4` 三入口）：
  本机 ffmpeg 确定性 lavfi 源生成，工具矩阵覆盖 48k 立体声正弦
  （TNS 开）、44.1k 单声道正弦（PNS/TNS 关）、44.1k 立体声 m4a
  （esds 入口）、48k 瞬态（八短窗）、48k 白噪声（强度立体声开）、
  48k LOAS/LATM。PNS（噪声码本 13）按 unsupported 收紧——向量全部
  `-aac_pns 0` 生成，放开须补向量。
- **参考侧**：`ffmpeg -i <vec> -f s16le -c:a pcm_s16le`（runner 内
  `-map 0:a:0 -v error`；**mp4 条带 `-ignore_editlist 1`**——我方
  解码器按裸码流语义输出全部存储帧，ffmpeg 默认按 edit list 裁去
  1 个 priming 帧，两侧口径须一致）。
- **我方侧**：`ye-codec-aac` 解码（f32 归一化）→ s16 →
  `<cache>/<name>.our.pcm`（交错、逐帧顺序，无 padding）。
- **容差**（施工图③原文定死）：100% 样本 |Δ|≤4 且**每帧** |Δ|≤1
  样本占比 ≥999‰（逐帧判定，`Tolerance::PcmTwoLevel`）。实测六条
  全部 max_abs=1、|Δ|>1 样本 0（即 100% ≤1 LSB，严于口径）。
- **失败归档**：超容差落 `<cache>/failures/<name>.fail.txt`
  （向量名/容差判据/max_abs 位（帧, 样本））。
- **标定对齐说明**：FFmpeg AAC 解码器 IMDCT 有效标定 ×1/M（其
  `ff_mdct_init` 把 `sqrt(1/1024)` 折进旋转因子两次）、M/S 蝶形无
  1/√2、TNS 反射系数先取负再 Levinson——我方按 ISO 标定实现后由
  `ye-codec-aac` 调用侧折算对齐（见其 decoder 模块「输出标定」）。

## 容差（按 codec 逐条写死，**只准收紧**）

见 `src/tolerance.rs`：向更严方向修改免评审；任何放宽（bit-exact →
PSNR 下限、提高下限、放宽比较维度）必须回施工图重新定口径。当前
h264 三条：NAL 序列（vs `h264_mp4toannexb`）与 SPS 字段
（vs `ffprobe -show_entries stream=profile,level,width,height`）均
bit-exact；逐帧 YUV（vs `ffmpeg -pix_fmt yuv420p -f rawvideo`）为
施工图②的 max-abs 容差（见上一节）。

## h264 对拍口径细节

- 我方序列 = `ye-container-mp4` 解 AVCC + `ye-codec-h264` 拆 NAL（包内
  NAL 逐包拼接）；
- ffmpeg 侧按 `h264_mp4toannexb` 的参数集语义对齐：**每个 IDR 前若缺
  带内 SPS/PPS，把 avcC（extradata）参数集插到 IDR 紧前**（包首有
  SEI 时插在 SEI 之后）——2026-10-24 施工图②以 45-IDR 向量实测锁定
  （形态 `[SEI,SPS,PPS,IDR]×45`，非只插首个 IDR；单 IDR 流为既有
  行为）。我方照同语义用 avcC 记录补插后逐 NAL 比较，实现在
  `src/nalalign.rs`（离线单测锁定）；
- 尾 NAL 的 `trailing_zero_8bits` 两侧归一化（起始码切分语义差异）；
- SPS 的 profile 命名按 ffprobe 口径映射（66+constraint_set1 →
  Constrained Baseline……），level 数值判等（ffprobe 有 `30`/`3.0`
  两种记法）；
- 逐帧 YUV 口径仅支持 4:2:0 向量（`chroma_format_idc=1`）：ffmpeg
  参考侧已被 `-pix_fmt yuv420p` 规格化，非 420 码流明确 FAIL 而非
  静默转换；
- 排查差异：`YE_CONFORMANCE_DEBUG_NALS=1 cargo run -p ye-conformance`
  打印两侧逐 NAL 的 (序号, type, len)。

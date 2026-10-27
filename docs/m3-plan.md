# M3 首轮施工图——编码与转码直通（锻启动）

> 2026-10-26 起草（规划师），基于 HEAD=165f022 勘察；同日按评审意见
> 修订（12 条，逐条吸收：文件域纠偏至 tests/、clippy 债实数更正、
> 参数集雏形分层消除依赖倒置、非默认门禁显式化、PSNR 向量族定形、
> 桩文案过期处置、旧 fps API 裁决、T6 时序串行化、调试残骸、行号校准）。
> 范围裁决：**首轮只在 ye 仓库施工**。交付两条线——① H.264 软件编码器
> （Baseline/CAVLC、I/P 帧、定 QP〔简单 CRF 可选〕、无 B 帧），② 处理图
> 最小集补齐（crop / fps / volume；scale 与像素格式转换复用 M2 既有核）。
> convert 编码槽接线、`ye convert` 端到端、CLI 接线**不进本任务表**——归
> 脚本的专门集成阶段。AAC-LC 编码、2-pass、B 帧/CABAC 编码、锻 duan
> 仓库接线见「后续批次」章节，不施工。

> **收尾实录（2026-10-26/27）**：首轮收官——施工 **6/6 完成**（T1 已
> 单独提交 `eb36761`，T2–T6 在收尾提交内）；§7.5 集成阶段同轮落地
> （convert 编码槽接线、`ye convert` 端到端、CLI 换调 + feature 接线，
> contracts.md 以 2026-10-27 标注）。三项结论：**全仓门禁全绿**；
> **端到端转码通过**（ffprobe 三重验收，见 T5 与 §7.5 结果行）；**无
> 未过项**。遗留（§6）与后续批次（§7）不变。逐任务结果见 §4 各
> 任务「结果」行。

## 0. 编号承接

M2 施工图的编号体系在桩文案里被引用，本图延续引用关系以便追溯：

- M2 施工图④ = convert 骨架 + 像素核（已交付）；
- M2 施工图⑤⑥ = H.264/AAC 编码器（`convert.rs` 的 `ENCODE_SLOT_STUB`
  文案「编码器随施工图⑤⑥交付」，见 `crates/ye/src/convert.rs:116-118`）；
  本轮任务 T2–T5 即⑤的 H.264 半边（该文案随 T5 更新，见 §5 裁决 8）；
- M2 施工图⑦ = 通用图调度（`graph.rs:8` 桩文案「桩：M3 交付」，
  `crates/ye-filter/src/graph.rs:111`）——本轮任务 T6。

## 1. 编码侧可用存量（勘察 + 评审核实）

全仓零编码器实现（`Encoder` trait/注册表/桩已定形但无真 crate），但解码侧
留下了大量可直接复用的资产（均在 `crates/ye-codec-h264/src/`，模块清单见
`lib.rs:37-51`，re-export 见 `:53-60`）：

| 存量 | 位置（本次实读核实） | 编码侧用法 |
| --- | --- | --- |
| CAVLC 读残差（coeff_token/level/total_zeros/run_before） | `cavlc.rs:410`（`read_residual`，`pub(crate)`） | 写侧码表与之互为镜像，位级回环测试的读端 |
| BitReader（u(1)/u(n)/ue/se） | `bitreader` 模块 | BitWriter 的回环对偶 |
| NAL 层：AVCC⇄Annex B 无损互转、rbsp 防竞争、avcC 解析 | `nal` 模块（`lib.rs:53-60` re-export `to_avcc`/`avcc_to_annexb` 等；`AvcConfigRecord::parse` 在 `nal.rs:334`） | 输出码流形态转换、ffmpeg 对拍喂料、avcC 写侧回环读端 |
| SPS/PPS 解析 + `Sps::is_baseline` | `sps` 模块（`is_baseline` 在 `sps.rs:520`） | 写侧 SPS/PPS 的回环读端；Baseline 边界参照 |
| 运动补偿（含半像素插值） | `mc.rs:114`/`:178`（`pub(crate)`） | P 帧运动补偿残差计算须与解码侧逐位一致 |
| 去块滤波 | `deblock.rs:157`/`:204`（`pub(crate)`） | 编码本地解码环内复用（防 drift 的硬要求） |
| 像素核（scale/crop_rgba/fps/volume_f32） | `ye-filter/src/video/`（`video/mod.rs:25-29` 契约、`:59` re-export） | T6 的核存量 |
| ffmpeg 守卫 + LCG 向量发生器先例 | `ye-filter/src/video/mod.rs:84-91`（`ffmpeg_available`）、`:108-126`（`Lcg`） | 编码对拍测试的构造性向量模式 |
| 封装出口 | `ye-container-mp4` 写入向 H264→avc1、Aac→mp4a（勘察确认） | 编码包进 MP4 已备好，本轮不接线只保兼容 |
| 门面出口 | `crates/ye/src/codec_h264.rs:14-16`（`register_h264` 现只挂解码工厂） | T5 加编码注册面 |
| 契约设施 | `CodecError::InvalidArguments`（`ye-codec/src/error.rs:44`）、`CodecRegistry::create_encoder`（`registry.rs:68`）、`Timestamp::compare`（`ye-time/src/timestamp.rs:147`） | 工厂校验/注册/时戳比较的既有落点 |

以上 `pub(crate)` 复用面意味着编码链任务（T2–T4）在 `encoder/` 新目录内
施工即可取用，不动解码侧原文件——链上文件重叠实际只有 `lib.rs` 一处。

契约层两处待扩（本轮定形，均公有字段结构、加字段属破编译改动；评审已
核实仓内构造面收敛：`VideoEncoderConfig` 仅 `ye-codec/src/stubs.rs:165`
与 `ye-codec/src/registry.rs:415-422` 两处，均在 T5 文件域内；
`FilterDescriptor` 仓内无 ye-filter 外构造点）：

- `VideoEncoderConfig`（`crates/ye-codec/src/codec.rs:95-104`）只有
  width/height/format/frame_rate，**无质量参数**——T5 扩（见 §5 裁决）；
- `FilterDescriptor` 只有 name/inputs/outputs，**无滤镜参数**——T6 定形。

**勘误**（评审实跑更正勘察结论）：HEAD 165f022 的 clippy 债为
ye-codec-h264 lib 59 错 **外加 `ye-codec-aac/src/filterbank.rs:466` 附近
`needless_late_init` 1 错**（`let mut contrib: Vec<f64>;` 声明与初始化
分离）；fmt 82 处 diff 确全部落在 `ye-codec-h264/src/`。无 ffmpeg 时
`cargo test --workspace` 恰 3 项 panic，位置在 **tests/ 集成测试**（非
src/）：`ye-codec-h264/tests/conformance.rs:117,129` 与
`ye-codec-aac/tests/conformance.rs:177`。

## 2. 任务链总览

```
门禁清偿(T1) ─┬→ 编码写侧地基(T2) → 帧内编码路径(T3) → 帧间编码路径(T4) ─┐
              └→ 处理图最小集(T6) ─────────────────────────────────────┴→ 编码收口对拍(T5)
```

- T1→T2→T3→T4 串行：同 crate（ye-codec-h264）文件所有权重叠，硬串行；
- T6 挂在 T1 之后（而非与其并行）：T1 的全仓验收要在 T6 动 ye-filter
  之前定格，否则并行分支中间态会污染 T1 的全绿基线；T1 体量小，T6 在
  T1 完成后仍与 T2–T4 充分并行；
- T5 汇合两支：全仓收口门禁（三件套全绿须 T6 已完成），且统一同步
  `contracts.md`（编码器契约节 + 滤镜调度/参数契约节同文件，文件所有权
  互斥规则要求与 T6 串行）。

## 3. 对拍口径（冻结）

本轮新增的对拍口径写入此处，容差/门槛一律**写死在测试常量、只准收紧免
评审、放宽须回施工图重新定口径**（对齐 `ye-filter/src/video/mod.rs:37-50`
与 `tests/conformance` 的 POLICY_NOTE 先例）。

1. **回环位级一致（正确性主验收，先于一切指标）**：编码器本地解码环的
   重建帧（预测 + 变换量化重建 + 去块）与自家 `H264Decoder` 解同一码流
   的输出帧**逐像素相等**。I 帧（T3）与 I+P 序列（T4）都要验。任何
   drift 都是不可交付缺陷，不设容差。
2. **合法性双解码（T5）**：我方编码码流须能被双方解码——
   (a) 自家 `H264Decoder`：输入按其契约取 AVCC 形态 + avcC extradata
   （`ye-codec-h264/src/lib.rs` 输入契约节）；
   (b) ffmpeg：`avcc_to_annexb` 转 Annex B 写 `.h264` 裸流，ffmpeg 解码
   无错误且**帧数与送入帧数一致**。
   ffmpeg 不在 PATH 时 (b) 打印跳过信息返回（不 fail），守卫复用
   `ffmpeg_available()` 模式（`video/mod.rs:84-91` 先例）。
3. **画质指标（T5）**：回环帧与源帧的 Y 分量 PSNR 门槛写死为测试常量。
   **门槛适用向量族 = 三类平滑合成向量：纯色、渐变、运动方块（平移）**
   （LCG 生成）；初值由本图定：QP 20 → ≥36 dB、QP 32 → ≥28 dB。
   LCG 噪声（纹理）向量**只用于口径 1 的位级回环与口径 2 的合法性，
   不设 PSNR 门槛**（噪声在高 QP 下 PSNR 天然不可比，混入即口径不唯一）。
   实测后只准收紧；放宽须回施工图。
4. **向量策略**：编码对拍用**确定性合成向量**（LCG 生成，先例
   `video/mod.rs:108-126`）；可选增强：把 `tests/conformance` 既有解码
   向量（intra/ip 组）解出的帧作为编码输入再回环。**不新增不删改
   manifest.toml 条目**（增删须登记理由且选择权不在施工方，本轮不碰）。
5. **T6 滤镜对拍**：crop（yuv420p 域纯字节搬运 → 0 容差）、fps（最近帧
   选择，与 `fps_nearest_frame_indices` 既有语义一致，对齐 ffmpeg
   `fps` 滤镜 round=near 口径）、volume（纯增益乘法 + 削波，对齐 ffmpeg
   `volume` 定点口径，容差 1 LSB 级）。PATH 无 ffmpeg 自动跳过。
6. **oracle 跳过三分支语义（T1 统一定形，随守卫改造写入 doc）**：
   ffmpeg 缺席 → 测试打印跳过信息返回（不计失败，探测次序**先于**向量
   检查——干净克隆 + 无 ffmpeg 亦可复现全绿）；ffmpeg 在而向量缓存缺席
   （未跑 `bash tests/conformance/fetch.sh`；`.cache` 为 git 忽略的本地
   资产）→ **按失败处理**（门禁不许假绿，此语义保留，现
   `ye-codec-h264/tests/conformance.rs:12-13` 头注须同步改写以覆盖
   新的分支表述）；ffmpeg 在且向量在 → 正常对拍。

其余一律遵守库既约定：零第三方依赖（仅 std + workspace path crate）、
不含 unsafe、禁 thiserror、错误手工 `Display`（文案英文短句、doc 注释
中文）、下层错误包装透出不吞、时戳 `Timestamp` 自描述不裸传 i64、
`Send + Sync` 上界、注册表显式构造显式传递。

**门禁通用注记**：`crates/ye/tests/stubs.rs`（桩聚合回归）只在
`--features codec-stubs,filter-stubs` 下编译、**不进
`cargo test --workspace` 默认门禁**（该文件头注 `:5-9` 明示）——凡动
stubs 的任务（T5）必须额外显式跑该 feature 组合（见 T5 验收）。

## 4. 任务详述

### T1 门禁清偿（M2 遗留债，编码链前置）

**结果（收尾实录）**：**完成**——已单独提交 `eb36761`：clippy 59+1
错清零、fmt 82 处对齐、对拍 spawn 三分支守卫 + h264 侧头注改写、
scratch dump 弃写死路径改系统临时目录（该提交说明即验收记录）。
后续任务（T2–T6）的门禁验收基线由此成立。

**为什么排第一**：HEAD 165f022 上三件套只有 test（带 ffmpeg 时）能过
（§1 勘误后的实数：clippy 59+1 错、fmt 82 处 diff、无 ffmpeg 3 项
panic）。不清偿则编码链每个任务的门禁验收都无法表达。

- **交付**：① clippy/fmt 修复——ye-codec-h264 全部（59 错 + 82 处
  diff）与 `ye-codec-aac/src/filterbank.rs` 的 1 错（机械重构，不改
  行为，测试数不减）；② spawn 守卫——
  `ye-codec-h264/tests/conformance.rs:117,129` 与
  `ye-codec-aac/tests/conformance.rs:177` 的 ffmpeg spawn 点按 §3.6
  三分支语义补守卫，并同步改写 h264 侧头注 `:12-13`（现文案「向量
  缺失…按失败处理」只覆盖旧的两分支）；③ 调试残骸清理——
  `ye-codec-h264/tests/conformance.rs:168-170` 写死的
  `C:/Users/hi/...` dump 路径（他人机器遗留）改为系统临时目录或删除
  该 dump 分支。
- **文件域**：`ye/crates/ye-codec-h264/src/`、
  `ye/crates/ye-codec-h264/tests/`、
  `ye/crates/ye-codec-aac/src/filterbank.rs`、
  `ye/crates/ye-codec-aac/tests/`。
- **验收**（ye/ 下实跑，两种环境各一遍）：
  - PATH 无 ffmpeg（干净克隆亦可）：`cargo test --workspace` 全绿
    （原 3 项 panic 点改为打印跳过、不计失败）、
    `cargo clippy --all-targets -- -D warnings` 退出 0、
    `cargo fmt --check` 无 diff；
  - PATH 加回 ffmpeg（如 duan/src-tauri/binaries）且向量缓存就位
    （`bash tests/conformance/fetch.sh` 已跑）：三项全绿。
  - clippy 修复仅机械重构，行为不改、测试数不减。完成即 commit+push
    （develop）。

### T2 编码写侧地基（码流语法与 CAVLC 写侧）

**结果（收尾实录）**：**完成**——`encoder/` 地基落地：
`bitwriter`（u(1)/u(n)/ue/se、rbsp trailing bits、防竞争写侧
escape）、CAVLC 写侧全 nC 桶码表 + level 转义 + total_zeros +
run_before（与读侧 `cavlc` 互为镜像）、`mb` 宏块层语法写骨架
（模块清单见 `crates/ye-codec-h264/src/encoder/mod.rs:38-47`）；
位级回环组（写→读全等且读侧恰消费写出位数）随 crate 测试交付。

- **交付**：`encoder` 模块落地——
  - `BitWriter`：u(1)/u(n)/ue/se 写、rbsp trailing bits、防竞争字节
    写侧 escape（`nal` 模块读侧 unescape 的对偶）；
  - CAVLC 写侧：coeff_token 全 nC 桶码表、level（含 level_prefix）、
    total_zeros、run_before——与 `cavlc.rs:410` 读侧互为镜像；
  - 宏块层语法写骨架：Baseline I/P 的 mb_type 编码映射、CBP、
    mb_qp_delta 写序。
- **文件域**：`ye/crates/ye-codec-h264/src/encoder/`、
  `ye/crates/ye-codec-h264/src/lib.rs`（挂模块）。
- **验收**：`cargo test -p ye-codec-h264` 全绿，含位级回环组：写侧产出
  经 `BitReader`/`cavlc` 读回全等，用例覆盖全部 nC 桶、ue/se 边界值、
  全零/饱和系数块；`cargo clippy -p ye-codec-h264 --all-targets --
  -D warnings` 0 错、`cargo fmt -p ye-codec-h264 --check` 干净。

### T3 帧内编码路径（I 帧全链闭环，含参数集雏形）

**结果（收尾实录）**：**完成**——`intra`（Intra16x16/Intra4x4/chroma
模式合法性镜像 + SAD/SATD 决策）、`transform`（前向 4x4 整数 DCT /
DC Hadamard 与量化，自解码侧反量化/反变换算术精确反推）、`frame`
（I 帧全链：参数集/slice header 写侧 + 宏块循环 + 本地解码环复用
解码侧去块，行粒度去块调度与解码器同构）；黑盒回环用例（编码 →
NAL/码流 → `H264Decoder` → 逐像素比对本地重建）交付。

- **交付**：I 帧编码——Intra16x16/Intra4x4 预测与模式决策（SAD/SATD）、
  4x4 整数 DCT 与 luma-DC/chroma-DC Hadamard、量化/反量化/重建
  （Baseline 平坦量化矩阵，QP 全域）、**编码本地解码环含去块滤波**
  （复用 `deblock.rs:157+`，I 帧边界同样滤波）；
  **帧级码流组装雏形**：SPS/PPS/slice header 写侧以测试用固定形态
  落地（参数先收窄为常量：单一分辨率、固定 profile/level 标志、
  I 帧一种 slice 类型），使黑盒回环（见验收）在本任务即可表达——
  参数集**定形**（level 查表、GOP 可配、avcC 写侧）归 T5，防止
  依赖倒置（T3/T4 验收不得依赖 T5 交付物）。
- **文件域**：同 T2（`encoder/` + `lib.rs`，链上串行重叠）。
- **验收**：合成 yuv420p 帧（偶数尺寸；纯色/渐变/纹理三类 LCG 生成）
  编码为**完整可解码的 I 帧码流**（含雏形 SPS/PPS/slice header）后，
  自家 `H264Decoder` 按其输入契约解码成功（黑盒：经 NAL/码流路径，
  不经内部结构接线），输出与编码器重建帧**逐像素相等**（§3.1 位级
  一致，0 容差）；`cargo test -p ye-codec-h264` + crate 域 clippy/fmt
  全绿。

### T4 帧间编码路径（P 帧 + 运动估计）

**结果（收尾实录）**：**完成**——`motion`（整像素 SAD 运动估计 +
半像素细化，候选预测块一律经 `mc.rs` 冻结核生成——决策与重建同一
块）、`pframe`（P_Skip / P_L0_16x16 / 宏块级 intra 回退三模式决策、
MVD 中值预测编码、Baseline 单参考帧会话、P 片头写侧、去块进重建
环）；I+P 合成运动序列逐帧回环用例交付。

- **交付**：P 帧——整像素 SAD 运动估计（搜索范围可配）+ 半像素细化
  （**运动补偿残差计算复用 `mc.rs:114+` 半像素插值核，与解码侧逐位
  一致**）、P_L0_16x16 / P_Skip / 宏块级 intra 回退三种模式决策
  （分块 inter 模式收紧不做，见 §5）、MVD 中值预测与编码、参考帧管理
  （Baseline 单参考帧）、frame_num/POC 递增、**P 帧 slice header 写侧**
  （续用 T3 雏形形态：frame_num/ref_idc/POC 固定策略）；去块进重建环。
  合成平移运动序列 I+P 回环。
- **文件域**：同 T2。
- **验收**：合成运动序列（平移方块/滚动渐变，≥30 帧，I+P 混合）编码为
  完整码流后自家解码器黑盒逐帧回环**逐像素一致**（§3.1）；P_Skip 判定
  路径有独立用例（skip 宏块重建值 == 参考帧平移值）；`cargo test -p
  ye-codec-h264` + crate 域 clippy/fmt 全绿。

### T6 处理图最小集（T1 完成后与 T2–T4 并行）

**结果（收尾实录）**：**完成**——`topological_order` 真实现（Kahn
算法，成环 → 新变体 `FilterError::Cycle`；原 `Unsupported` 变体随
桩删除）、`FilterParams` 参数承载定形（crop 的 x/y/width/height、
fps 的 num/den、volume 的 gain；纯数据无不变式，合法性归执行核）、
`crop_yuv420p`（全偶校验、纯字节搬运 0 容差）、旧 fps API 改
`Timestamp` 入参（i64 签名删除）、`volume_gain_from_db`（dB → 线性
增益换算）；ffmpeg 对拍组（无 ffmpeg 跳过）+ 跨时基比较用例交付。

- **交付**：
  - `FilterGraph::topological_order` 真实现（Kahn 或 DFS）：DAG 返回
    依赖序（每节点排在其全部上游之后），成环 → 报错（用
    `FilterError` 既有变体还是新增 Cycle 变体由施工定形，语义与文案
    写 doc）；空图返回空序；桩文案常量与桩测试（`graph.rs:8`、
    `graph.rs:228-257`）随之删除替换；
  - **滤镜参数形态定形**：`FilterDescriptor` 扩参数承载（crop 的
    x/y/w/h、fps 的 num/den、volume 的 gain）——形态由施工定形并写
    进模块 doc（契约正本同步归 T5 统一进 contracts.md）；
  - 核补齐：yuv420p 平面域 crop（新写；crop_rgba 既有保留；yuv420p 域
    参数须偶数对齐，违者 `Err`）；fps 消费形态对齐
    `fps_nearest_frame_indices`；volume_f32 包装成滤镜形态（gain 线性
    倍率，dB 换算口径写 doc）；
  - **旧 fps 公共 API 改造**（§5 裁决 7）：现
    `fps_nearest_frame_indices(pts_us: &[i64], …)`（契约
    `video/mod.rs:26`、公共导出 `ye-filter/src/lib.rs:48`）裸传 i64 µs
    违反「时戳不裸传 i64」硬约定——改造为 `Timestamp` 入参形态并
    **删除旧签名**（评审已核实：门面 `crates/ye/src/` 未 re-export、
    仓内无消费者，破面仅限本 crate 测试）。
- **文件域**：`ye/crates/ye-filter/src/`。
- **验收**：`cargo test -p ye-filter` 全绿——topological_order 的
  DAG/成环/空图/孤立节点用例、三滤镜语义用例（fps 用例含跨时基
  比较，走 `Timestamp::compare`）、§3.5 的 ffmpeg 对拍组（容差写死、
  无 ffmpeg 跳过）；`cargo clippy -p ye-filter --all-targets --
  -D warnings` 0 错、`cargo fmt -p ye-filter --check` 干净。scale 与
  像素格式转换不重做（复用 M2 核）。

### T5 编码收口对拍（参数集定形、trait 接入、注册、双解码对拍）

**结果（收尾实录）**：**完成**——契约层扩 `QualityMode`
（`Qp(u8)` 必做 + **简单 CRF 一并交付**，裁决 5 的可选项已取）与
`VideoEncoderConfig` 的 gop/quality 字段；真工厂参数校验（QP 0 →
`InvalidArguments`）；`params` 参数集定形（level 查表取最小满足项 +
avcC 写侧构造，经 `Sps`/`AvcConfigRecord::parse` 回环）；门面
`register_h264_encoder`；H.264 编码桩删除（`register_stubs` 收窄为
解码×2 + AAC 编码）；AAC 桩文案按裁决 6 更新、convert 桩文案按
裁决 8 更新；contracts.md 补「H.264 编码器输出契约」与「处理图
最小集契约」两节。**对拍全过**（`crates/ye/tests/encode_h264.rs`，
进默认全仓门禁）：位级回环 0 容差（平滑三类 + LCG 噪声、每组 32
帧 I+P、QP 20/32 两档）、ffmpeg 双解码（Annex B 裸流无错 + 帧数
一致，无 ffmpeg 跳过）、Y-PSNR 逐帧门槛 QP20≥36dB / QP32≥28dB
（§3.3 初值未动）；门面注册路径经 `h264` feature 组覆盖
（`facade_register_h264_encoder_full_chain`）。

- **交付**：
  - **参数集写侧定形**（T3/T4 雏形 → 可配）：Baseline（profile_idc=66、
    constraint_set 标志）、level_idc 按 帧尺寸×帧率 查表取满足的最小值、
    GOP/IDR 间隔可配；写出的参数集经自家 `Sps` 解析回环校验；avcC 记录
    **写侧**构造（SPS/PPS 落位，`AvcConfigRecord::parse`（`nal.rs:334`）
    回环）；
  - `Encoder` trait 接入（`codec.rs:61-79` 状态机）：feed/pull/flush、
    输出包按 `EncoderConfig.time_base` 打戳（帧号 × frame_rate 递推），
    B 帧缺席故无重排（dts=pts，doc 写明）；输出 `Packet.data` = AVCC
    长度前缀形态、extradata = avcC（对齐解码器输入契约与 MP4 出口，
    见 §5 裁决 1）；
  - 契约层扩：`VideoEncoderConfig` 增质量字段（`Qp(u8)` 必做，简单
    CRF 可选；GOP/IDR 间隔字段）——公有字段结构加字段，仓内构造点
    仅 `ye-codec/src/stubs.rs:165` 与 `ye-codec/src/registry.rs:415-422`
    （评审核实），均在本文件域内随改；**真工厂校验参数**（宽高偶数、
    QP 值域、格式为 yuv420p，非法 → `CodecError::InvalidArguments`
    （`error.rs:44`）；对照桩的「工厂不校验」语义，真实现交付即校验）；
  - 注册与桩退位：`encoder_factory` + `register_h264_encoder`（门面
    `crates/ye/src/codec_h264.rs` 对齐 `register_h264` 形态）；
    `ye-codec/src/stubs.rs` 删 **H.264 编码桩**（`stubs.rs:33-35`/
    `:105-110`/`:135` 的 h264_encoder 三处与相关测试），
    `register_stubs` 相应收窄；**AAC 桩文案更新**（§5 裁决 6）：
    `AAC_ENCODE_MSG`（`stubs.rs:38-40`）与模块 doc（`:12-14`）的
    「编码 → M3 交付」改为后续批次标注（对齐本图 §7.1），行为测试
    `encoder_stubs_error_with_m3_marker`（`stubs.rs:299-323`）断言
    同步；**门面桩测试修复**：`crates/ye/tests/stubs.rs:26` 断言
    `supports_encoder(CodecId::H264)` 在桩收窄后必挂，随桩语义更新
    （改为「H.264 编码经真工厂注册可查、桩不再提供」之类，施工定形）；
  - **convert 桩文案更新**（§5 裁决 8）：`crates/ye/src/convert.rs`
    的 `ENCODE_SLOT_STUB`（`:116-118`）「编码器随施工图⑤⑥交付」
    失真——只改文案（H.264 Baseline 编码器已交付、编码槽接线归 M3
    集成阶段），**不动泵逻辑**（接线归集成阶段）；
  - contracts.md 统一同步：编码器输出契约（AVCC+avcC、时戳、QP 语义、
    收紧清单）与 T6 的滤镜调度/参数契约两节。
- **文件域**：`ye/crates/ye-codec-h264/src/`、`ye/crates/ye-codec/src/`、
  `ye/crates/ye/src/codec_h264.rs`、`ye/crates/ye/src/convert.rs`、
  `ye/crates/ye/tests/`、`ye/docs/contracts.md`。
- **验收**（全仓收口，T6 已完成是前提）：
  1. §3 口径 1/2/3 全部通过：合成序列（平滑三类向量族各 ≥1 组、
     ≥30 帧、I+P、QP 20 与 32 两档）→ 回环位级一致 +
     `avcc_to_annexb` 后 ffmpeg 解码无错误且帧数一致（无 ffmpeg
     跳过）+ PSNR 门槛达标（适用集与数值按 §3.3，噪声向量不进
     门槛集）；
  2. 注册面：经门面 `register_h264_encoder` 注册后
     `CodecRegistry::create_encoder`（`registry.rs:68`）全链可用；
     AAC 桩行为不变（文案按裁决 6 更新后测试同步过）；
  3. 全仓门禁两种环境全绿（ye/ 下实跑）：`cargo test --workspace`、
     `cargo clippy --all-targets -- -D warnings`、
     `cargo fmt --check`——PATH 无 ffmpeg 一遍、PATH 有 ffmpeg 且
     向量就位一遍；**另显式跑非默认 feature 组合**
     `cargo test -p ye --features codec-stubs,filter-stubs` 全绿
     （桩聚合回归不进默认门禁，见 §3 尾注记）；
  4. 完成即 commit+push（develop）；父仓库指针更新归收尾阶段，不在
     本任务内。

## 5. 设计裁决与收紧登记（本轮定形，收紧合规即码流仍标准合法）

| # | 裁决 | 理由 |
| --- | --- | --- |
| 1 | 输出码流形态 = AVCC + avcC extradata（非 Annex B 直出） | 解码器输入契约（`ye-codec-h264/src/lib.rs` 输入契约节）与 MP4 出口（H264→avc1）原生对齐；ffmpeg 对拍经既有 `avcc_to_annexb` 转换 |
| 2 | 质量参数首轮 `Qp(u8)`，**QP 0 显式 Err**（接受 1–51） | QP 0 的无缩放语义边缘、消费方无用例；形态收紧一律 Err 的库风 |
| 3 | P 帧宏块模式收紧：P_L0_16x16 + P_Skip + intra 回退；16x8/8x16/8x8 分块 inter 不做 | 压缩率让位于任务量可控；编码端模式选择自由，码流仍合法；分块模式登记后续批次 |
| 4 | 无 B 帧、单参考帧、CAVLC only、profile Baseline | 首轮范围硬约束（CABAC 解码侧真流对拍本身还卡着，见 §6） |
| 5 | 简单 CRF 可选交付（帧内复杂度自适应 ΔQP 类），真码控（CBR/2-pass/CRF 完整版）不做 | 「定 QP 或简单 CRF」范围；真码控登记后续批次 |
| 6 | 去块滤波必须进编码本地解码环（T3 起即含） | H.264 本地解码环定义；不含则回环位级一致（§3.1）不可能达成 |
| 7 | 运动补偿/去块/CAVLC 读表一律复用解码侧实现（`cavlc.rs:410`/`mc.rs:114`/`deblock.rs:157` 均 `pub(crate)`，同 crate 可取用），不另写副本 | 单一事实源；副本必然 drift，§3.1 无法守恒 |
| 8 | **参数集雏形分层**：T3/T4 产「测试用固定形态」的 SPS/PPS/slice header 写侧（黑盒回环即需），T5 定形（level 查表、GOP、avcC、门面暴露） | 消除依赖倒置——T3/T4 的黑盒验收不得依赖 T5 交付物（评审问题 3） |
| 9 | **旧 fps API 改造**：`fps_nearest_frame_indices` 的 `pts_us: &[i64]` 签名改造为 `Timestamp` 入参并删除旧签名，不留 deprecated | 「时戳不裸传 i64」硬约定；评审核实门面未导出、仓内零消费者，库未发版不留兼容包袱（评审问题 8） |
| 10 | **AAC 编码桩文案**：「桩：M3 交付」改为后续批次标注（模块 doc `:12-14` 与 `AAC_ENCODE_MSG` `:38-40` 及测试断言同步） | AAC 编码已移入后续批次，保留旧文案即固化过期里程碑（评审问题 7） |
| 11 | **convert 桩文案**：`ENCODE_SLOT_STUB` 随 T5 更新为「H.264 Baseline 已交付、接线归 M3 集成阶段」，不动泵逻辑 | 文案与交付状态一致；接线仍归集成阶段（评审问题 9） |
| 12 | **T6 时序**：T6 依赖 T1（门禁清偿）而非与其并行 | T1 的全仓全绿验收必须在 T6 改动 ye-filter 前定格（评审问题 10） |

## 6. 遗留与风险（可见性保留，不在本任务表）

- **CABAC 解码真流对拍卡点**（M2 WIP 165f022）：mb_type→cbf 位区间
  一处 bin 分歧；证据链在会话记录未入库存档、本地无法复核。不阻塞
  本轮（Baseline/CAVLC 编码不依赖 CABAC 解码侧），但 CABAC/B 帧向量集
  与「CAVLC+B 如实收紧」仍欠账，归 M2 收口轨道。
- **波形峰值缺席**（roadmap M2「音频 PCM → 波形峰值」只有解码半边）：
  剪侧消费方也缺席，不进本轮；峰值 API 归后续按剪的真实需求排期。
- **剪/锻接 ye 未动**：jian 无 ye 依赖、duan probe 仍 ffprobe（D1）、
  duan 转码仍 ffmpeg sidecar（D2）——duan 接线归后续批次。
- **ye-cli**：convert 子命令仍 M1 桩、features 只开 mp4——CLI 接线归
  集成阶段（届时换调门面泵并开 h264/aac/png feature）。
  （**收尾实录更新**：该遗留已随集成阶段解除——`ye convert` 真实现
  + `h264` feature 开启，2026-10-27，见 §7.5 结果行。）
- **duan/docs/目标与里程碑.md 引用 ye/docs/integration.md 不存在**：
  过期引用，归收尾阶段处置（本图不列任务）。
- **`crates/ye/tests/` 其余文件**（convert/png/remux）同样只在特定
  feature 组合下编译——T5 显式跑 `codec-stubs,filter-stubs` 组合已
  覆盖桩面；其余组合的接线归集成阶段验证。

## 7. 后续批次（不施工，只登记）

1. **AAC-LC 编码器**（ye-codec-aac 编码侧；届时删 AAC 编码桩，
   convert 音频编码槽解锁；T5 裁决 10 的文案届时再随交付更新）；
2. **H.264 真码控**：CBR/A-VBR、2-pass、完整 CRF；分块 inter 模式
   （16x8/8x16/8x8）与多参考帧；
3. **B 帧 / CABAC / High Profile 编码**（前置：CABAC 解码真流对拍
   收口）；
4. **锻 duan 仓库接线**：D1 probe 换冶、D2 转码换冶（ye M3 集成验收
   后解锁）；
5. 集成阶段（脚本另行调度，非本表）：convert 编码模式接线
   （`crates/ye/src/convert.rs` 编码槽，参数形态届时定形）、
   `ye convert` 端到端、CLI 换调 + feature 接线、父仓库指针与
   roadmap/waitlist 收口。
   **结果（收尾实录）**：**已于本轮收尾同步完成**（2026-10-27）——
   convert 编码模式真实现（视频链 解码→处理链→H.264 编码→MP4 封装，
   音频包直通、每轨平移至首包 dts=0；avcC 取自首个 IDR 带内
   SPS/PPS，封装头延迟至首视频包产出）、`ye convert <IN> <OUT>
   [--size 宽x高]` CLI 子命令 + ye-cli 增开 `h264` feature。
   **端到端转码通过**（`crates/ye-cli/tests/convert_e2e.rs`，进默认
   全仓门禁、无 ffmpeg 跳过）：ffmpeg 造源 3s 176x144 H.264+AAC →
   `ye convert --size 320x240` → ffprobe 三重验收——① 输出
   `-v error` 解码无错；② 流参数 h264 320x240、音频参数与源一致
   （直通）；③ 时长差 ≤0.1s 且视频帧数逐帧相等（3s@15fps = 45 帧）。
   父仓库指针更新归父仓库收尾提交（不在 ye 仓库内）。

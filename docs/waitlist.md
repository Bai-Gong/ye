# M2+ 等待清单

冶（ye）在 M1 收口后**还做不到**、消费方（剪/锻）在等的能力总账：
每个缺口标**阻塞层**（ye 内哪一层挡着）与**放行里程碑**。配套：
接口契约见 [contracts.md](contracts.md)，排期见 [roadmap.md](roadmap.md)，
桩语义（可构造 / 可查询 / 调用即 Unsupported）见 contracts.md
「M2+ 接口桩契约」一节。

**已可用（不在此表）**：M1——MP4（progressive 与 fMP4）读 + 写 +
probe（流表 / 帧率 / 码率）+ remux 直通，经 `ContainerRegistry`
auto-probe；M2——H.264 Baseline 解码、AAC-LC 解码、yuv→rgb 像素
转换与 scale、PNG 编码、`convert_frames` 帧消费管线（ffmpeg oracle
对拍过；High/CABAC 解码收口见下表第 1 行）；**M3 首轮（2026-10-26）
——H.264 软件编码（Baseline/CAVLC、I/P 帧、定 QP + 简单 CRF）、
处理图最小集（`topological_order` + crop/fps/volume + `FilterParams`）、
convert 编码模式真实现（视频重编码 + MP4 封装、音频直通）与
`ye convert` CLI（--size），端到端转码经 ffprobe 三重验收通过**
（契约见 contracts.md「锻的最小 API 面」「H.264 编码器输出契约」
「处理图最小集契约」）；**M3 补齐（2026-10-26 晚）——真码控**
（`EncoderConfig.bit_rate`：CBR 风味/A-VBR，实测 300k +9.3%、800k
−0.7%，None 逐字节回归）**+ AAC-LC 编码器**（SCE/CPE 长窗 + 轻量
心理声学 + 码率环 + FIL 补齐；自家解码 vs ffmpeg 逐样本 100%
|Δ|≤1e-5〔实测 8.9e-8〕、SNR 128k 28.2dB、码率命中 +0.5%；AAC 编码
桩随交付删除）**+ convert 音频三态/剥视频**（`AudioEncodeSettings`
Copy/Aac/Drop、`video=None`、剥光守卫、CLI --vb/--audio/--no-video），
锻侧拒绝面收窄（目标码率/纯音频 mp4/剥音轨/AAC 重编码放行）**；**剪接线（2026-10-27 M2 收口，m2-jian-plan
T1–T8 + 同日解码器分歧修复）——`waveform` 模块（分桶纯核 +
`audio_peaks`/`audio_peaks_with_count` 消费助手）与 `frame_grab` 门面
（`decode_frame_rgba`/`grab_frame_png`）交付，剪侧 probe/抽帧/波形
三命令双引擎接线落地；oracle 对拍：波形两条向量（正弦/白噪声）逐桶
逐声道全绿（实测最坏 |Δmax|=5.022e-6 / 8.810e-5，容差按实测收紧
2e-4→1.5e-4）、vec-video-multi 不可解音轨抽帧整体 Err 行为锁定生效；
早班像素对拍暴露的解码器分歧（去块启用首差帧 0 luma + 关去块首差帧
81 chroma）同日修复收口（去块延后图级 + chroma QP 偏移 + pred_8x16
C→D 回退），像素/YUV 对拍用例翻除 `#[ignore]` 过门（三点位
max |Δ|=1、去块配方 90 帧逐位一致），`ip30` 回归集入库**（对拍口径与
skip 分级语义见 `crates/ye/tests/common/mod.rs` 与两个 `*_oracle.rs`）。

| # | 缺口（消费方需求） | 阻塞层 | 放行里程碑 | 现状 |
| --- | --- | --- | --- | --- |
| 1 | 抽帧缩略图（剪 M1：H.264 帧 → PNG） | ye-codec（High/CABAC/B 帧解码收口） | M2 收口轨道 | Baseline 链路已交付且分歧修复收口（2026-10-27：H.264 Baseline 解码 + yuv→rgb/scale + PNG 编码真实现；m2-jian-plan T3 对拍暴露的 30fps 长链两处分歧——去块启用首差帧 0 luma〔行内去块污染帧内预测 + chroma QP 漏加 PPS 偏移〕、关去块首差帧 81 chroma〔pred_8x16 缺 C→D 回退〕——同日修复：去块延后图级 + `chroma_qp_offset` 接线 + pred_8x16 回退，`ip30` 回归集入库，去块条 90 帧逐位一致、像素对拍翻除 `#[ignore]` 过门〔三点位 max |Δ|=1〕）。余留（如实）：no-deblock 条帧 81 起 Cr ±1 孤立样本（容差内，归档 ffmpeg 舍入边角，收紧 `ip30` 容差前须先收口——ye-codec-h264 tests/conformance.rs `ip30` 注释）；jian 侧像素用例仍挂 `#[ignore]` 未随修复翻除（归 jian 后续）。High/CABAC 解码基础设施已落地（165f022）但真流对拍卡点未收（m3-plan §6），High 素材导入仍不可用 |
| 2 | 真转码（锻首个任务：改分辨率/码率出 MP4） | ~~锻 duan 接线；ye 真码控~~ 均已交付 | **M3（收口）** | 已交付收口（M3 首轮 2026-10-26 + M3 补齐同日晚）：H.264 Baseline 编码 + 真码控（CBR 风味/A-VBR）+ AAC-LC 编码 + convert 三态（视频重编码、音频 直通/AAC 重编码/剥轨、剥视频）+ `ye convert` CLI 全量旗标；锻侧 D1/D2 接线与拒绝面收窄同步落地（码率模式/纯音频 mp4/剥音轨/AAC 重编码放行，非 MP4 系容器/非 AAC 音源/改采样率/>2 声道明确拒绝，probe 增 sampleRate/channels）。余留（如实）：2-pass/完整 CRF 归后续批次；改帧率仍拒（fps 滤镜参数未接） |
| 3 | 导出合成（剪：filter_complex 全集 + 多输入时间线合成 + 进度） | ye-filter（全集与调度）+ 编码 + 合成 API | **M4** | **M4 首轮已交付（2026-10-26 深夜）**：`ye::compose` 顺序拼接泵（多段 MP4 裁剪窗口拼接 → H.264+AAC 重编码 MP4，CFR 桶采样/跨段音频累积/与 convert 同契约的进度取消，契约见 contracts.md「合成导出最小集契约」）+ 剪导出双引擎接线（`run_export_ye` 命令 + `buildYeExportParams` 纯函数翻译层：纯剪辑时间线〔首尾相接、无任何变换〕走 ye，变速/画中画/叠加/独立音轨/转场等明确拒绝指路 ffmpeg；e2e 与 ffmpeg concat 对照五点位灰度 |Δ|≤3.0，实测 1.62）。余留（如实）：叠加/变速/旋转/透明度/混音/转场等时间线合成全集、seek 快进、段级静音填充——归 M4 后续批次 |
| 4 | MOV 容器（QT 族导入） | ye-container（新实现 crate，MP4 近亲可大量复用） | 持续轨道·下一梯队 | 未开工；按剪的导入统计驱动（同梯队：WAV/MP3 纯探测） |
| 5 | MKV / WebM 容器 | ye-container（Matroska 实现 crate） | **M5** | 未开工 |
| 6 | GIF / 图片探测（纯探测，无解封装） | ye-container（纯探测工厂） | 未排期 | 未开工；按导入统计驱动 |

维护约定：

- 缺口关闭（能力交付）时删行，并在 [contracts.md](contracts.md) 补
  对应契约；真实现落地后同步删除对应桩（替换路径见「M2+ 接口桩
  契约」）。
- 新增缺口先进本表再排期；里程碑变更须同步 [roadmap.md](roadmap.md)
  修订记录。

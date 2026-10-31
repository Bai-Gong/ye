# 能力缺口等待清单

冶（ye）**还做不到**、消费方（剪/锻）在等的能力总账：每个缺口标
**阻塞层**（ye 内哪一层挡着）与**放行里程碑**。配套：接口契约见
[contracts.md](contracts.md)，排期与修订记录见 [roadmap.md](roadmap.md)。

**已可用（不在此表）**：

- M1——MP4（progressive 与 fMP4）读 + 写 + probe + remux 直通
  （`ContainerRegistry` auto-probe）。
- M2——H.264 Baseline 解码、AAC-LC 解码、yuv→rgb 与 scale、PNG 编码、
  波形峰值、抽帧门面（High/CABAC 真流对拍收口见下表第 1 行）。
- M3——H.264 软件编码（定 QP / 简单 CRF / 真码控）、AAC-LC 编码、
  处理图最小集、`ye convert` 端到端（音频三态/剥视频）。
- M4 首轮——`ye::compose` 顺序拼接泵 + 剪导出双引擎接线。

各批次交付明细见 [roadmap.md](roadmap.md) 修订记录与施工图归档。

| # | 缺口（消费方需求） | 阻塞层 | 放行里程碑 | 现状 |
| --- | --- | --- | --- | --- |
| 1 | 抽帧缩略图（剪 M1：H.264 帧 → PNG） | ye-codec（High/CABAC/B 帧解码收口） | M2 收口轨道 | Baseline 链路已交付且分歧修复收口（2026-10-27，`ip30` 回归集入库，详录见 roadmap 修订记录）。余留（如实）：no-deblock 条帧 81 起 Cr ±1 孤立样本（容差内，归档 ffmpeg 舍入边角，收紧 `ip30` 容差前须先收口——ye-codec-h264 tests/conformance.rs `ip30` 注释）；jian 侧像素用例仍挂 `#[ignore]` 未随修复翻除（归 jian 后续）；**2026-10-31 库内 High/CABAC 收口：progressive 8-bit 4:2:0 的全 I / I/P / B 金字塔及去块、加权变体五组 Y/Cb/Cr 逐帧 bit-exact 通过；剪/锻的导入放行仍需调用侧验证，不等于消费方已放开 High 导入** |
| 2 | 真转码（锻首个任务：改分辨率/码率出 MP4） | ~~锻 duan 接线；ye 真码控~~ 均已交付 | **M3（收口）** | 已交付收口（M3 首轮 + 补齐，2026-10-26）：编码 + 真码控 + AAC-LC 编码 + convert 三态 + CLI 全量旗标 + 锻侧接线与拒绝面矩阵。余留（如实）：2-pass/完整 CRF 归后续批次；改帧率仍拒（fps 滤镜参数未接） |
| 3 | 导出合成（剪：filter_complex 全集 + 多输入时间线合成 + 进度） | ye-filter（全集与调度）+ 编码 + 合成 API | **M4** | M4 首轮已交付（2026-10-26）：`ye::compose` 顺序拼接泵 + 剪导出双引擎（契约见 contracts.md「合成导出最小集契约」）。余留（如实）：叠加/变速/旋转/透明度/混音/转场等时间线合成全集、seek 快进、段级静音填充——归 M4 后续批次 |
| 4 | MOV 容器（QT 族导入） | ye-container（新实现 crate，MP4 近亲可大量复用） | 持续轨道·下一梯队 | 未开工；按剪的导入统计驱动（同梯队：WAV/MP3 纯探测） |
| 5 | MKV / WebM 容器 | ye-container（Matroska 实现 crate） | **M5** | 未开工 |
| 6 | GIF / 图片探测（纯探测，无解封装） | ye-container（纯探测工厂） | 未排期 | 未开工；按导入统计驱动 |

维护约定：

- 缺口关闭（能力交付）时删行，并在 [contracts.md](contracts.md) 补
  对应契约；真实现落地后同步删除对应桩。
- 新增缺口先进本表再排期；里程碑变更须同步 [roadmap.md](roadmap.md)
  修订记录。

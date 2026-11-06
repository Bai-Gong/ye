# 能力缺口等待清单

冶（ye）**还做不到**、消费方（剪/锻）在等的能力总账：每个缺口标
**阻塞层**（ye 内哪一层挡着）与**放行里程碑**。配套：接口契约见
[contracts.md](contracts.md)，排期与修订记录见 [roadmap.md](roadmap.md)。

**已可用（不在此表）**：

- M1——MP4（progressive 与 fMP4）读 + 写 + probe + remux 直通
  （`ContainerRegistry` auto-probe）。
- M2——H.264 Baseline 解码、AAC-LC 解码、yuv→rgb 与 scale、PNG 编码、
  波形峰值、抽帧门面；High/CABAC progressive 8-bit 4:2:0 真流对拍及
  消费方真实录屏抽帧已验证，剩余组合见下表。
- M3——H.264 软件编码（定 QP / 简单 CRF / 真码控）、AAC-LC 编码、
  处理图最小集、`ye convert` 端到端（音频三态/剥视频）、有理数目标帧率、
  44.1/48kHz 流式重采样、任务预检与输出文件保护。
- M4 静态多轨——`ye::compose` 顺序拼接、`ye::timeline` 多层变换/透明叠加、
  混音/增益/声像、黑场/静音、跨 GOP seek、显式源音轨选择；剪已接入。

- 2026-11-05：High/CAVLC 8×8 的 progressive 8-bit 4:2:0 I/P 路径已补齐；
  全 I、运动 I/P（去块开/关）和加权色块 4 组各 45 帧三平面精确对拍通过。
  不据此声明 CAVLC B 帧或所有自定义矩阵组合均已验收。

- 2026-11-06：9 项既有对拍失败全部清除；补齐确定性颜色转换、AAC KBD 窗、
  H.264 舍入及 B 直接预测修复。连续抽帧共用游标已交付，验收见
  [本轮记录](validation-2026-11-06.md)，接入见 [接入准备](integration-readiness.md)。

各批次交付明细见 [roadmap.md](roadmap.md) 修订记录与施工图归档。

| # | 缺口（消费方需求） | 阻塞层 | 放行里程碑 | 现状 |
| --- | --- | --- | --- | --- |
| 1 | H.264 舍入边角 | ye-codec-h264 | 解码收口轨道 | `ip30` no-deblock 帧 81 起 Cr ±1 孤立样本仍按既有容差归档；收紧容差前需单独收口。High/CABAC 已有消费方抽帧验收，不再列为整体导入阻塞。 |
| 2 | 编码质量与码控扩展 | ye-codec-h264 | 后续编码批次 | 基础转码和改帧率已交付；2-pass、完整 CRF 与压缩效率评估待做。 |
| 3 | 高级时间线导出 | ye-filter + 时间线调度 | M4 后续 | 静态叠加/旋转/透明度/混音、seek 和静音填充已交付；关键帧、变速、转场、字幕/图片等高级编辑仍待实现。 |
| 4 | MOV 容器（QT 族导入） | ye-container（新实现 crate，MP4 近亲可大量复用） | 持续轨道·下一梯队 | 未开工；按剪的导入统计驱动（同梯队：WAV/MP3 纯探测） |
| 5 | MKV / WebM 容器 | ye-container（Matroska 实现 crate） | **M5** | 未开工 |
| 6 | GIF / 图片探测（纯探测，无解封装） | ye-container（纯探测工厂） | 未排期 | 未开工；按导入统计驱动 |
| 8 | 基础版交付验收 | 剪/锻消费方与运行环境 | P3/P4 收口 | 真实 84 秒录屏未完整渲染；独立双音轨听感、干净 Windows 安装后的完整交互闭环待验。本轮库内 753 项通过；旧录屏与 Windows 环境按用户要求不纳入本轮，历史待验项未转为通过。 |

维护约定：

- 缺口关闭（能力交付）时删行，并在 [contracts.md](contracts.md) 补
  对应契约；真实现落地后同步删除对应桩。
- 新增缺口先进本表再排期；里程碑变更须同步 [roadmap.md](roadmap.md)
  修订记录。

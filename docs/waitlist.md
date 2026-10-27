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
「处理图最小集契约」）。

| # | 缺口（消费方需求） | 阻塞层 | 放行里程碑 | 现状 |
| --- | --- | --- | --- | --- |
| 1 | 抽帧缩略图（剪 M1：H.264 帧 → PNG） | ye-codec（High/CABAC/B 帧解码收口） | M2 收口轨道 | Baseline 链路已交付（M2：H.264 Baseline 解码 + yuv→rgb/scale + PNG 编码真实现，ffmpeg oracle 对拍过）；High/CABAC 解码基础设施已落地（165f022）但真流对拍卡点未收（m3-plan §6），High 素材导入仍不可用 |
| 2 | 波形图（剪 M2：AAC → PCM 峰值） | 峰值计算 API（AAC-LC 解码已交付） | 未排期 | AAC-LC 解码真实现已交付（M2，ffmpeg oracle 对拍过）；PCM → 波形峰值 API 缺席（只有解码半边，剪侧消费方也未定），按剪的真实需求排期（m3-plan §6） |
| 3 | 真转码（锻首个任务：改分辨率/码率出 MP4） | 锻 duan 接线（D1 probe / D2 转码换冶）；ye 内仅剩真码控（CBR/A-VBR，后续批次） | **M3** | ye 侧已交付（M3 首轮 2026-10-26）：H.264 Baseline 编码 + convert 真实现（视频重编码/音频直通）+ `ye convert`，端到端转码三重验收通过；「改码率」暂以 QP/简单 CRF 间接控制（目标码率归后续批次）；锻侧 D1/D2 接线未动 |
| 4 | 导出合成（剪：filter_complex 全集 + 多输入时间线合成 + 进度） | ye-filter（全集与调度）+ 编码 + 合成 API | **M4** | 处理图调度已交付（M3 `topological_order` 真实现，桩随删）；滤镜全集与多输入合成 API 未开工 |
| 5 | MOV 容器（QT 族导入） | ye-container（新实现 crate，MP4 近亲可大量复用） | 持续轨道·下一梯队 | 未开工；按剪的导入统计驱动（同梯队：WAV/MP3 纯探测） |
| 6 | MKV / WebM 容器 | ye-container（Matroska 实现 crate） | **M5** | 未开工 |
| 7 | GIF / 图片探测（纯探测，无解封装） | ye-container（纯探测工厂） | 未排期 | 未开工；按导入统计驱动 |

维护约定：

- 缺口关闭（能力交付）时删行，并在 [contracts.md](contracts.md) 补
  对应契约；真实现落地后同步删除对应桩（替换路径见「M2+ 接口桩
  契约」）。
- 新增缺口先进本表再排期；里程碑变更须同步 [roadmap.md](roadmap.md)
  修订记录。

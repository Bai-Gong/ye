# 路线图

- **M0（当前）**：仓库骨架、分层约定、文档。
- **M1**：`ye-time` / `ye-pixfmt` 基础类型；`ye-container-mp4`
  解封装 + 封装（含 fMP4）。准入需求：`jian` 能列出素材信息。
  - 第一步（基础类型与核心契约）已完成：`ye-time`、`ye-pixfmt`、
    `ye-codec`、`ye-container` 四个 crate 的基础类型、trait 与注册表
    已落地并通过各自单元测试，接口契约详见 `architecture.md`。
    剩余：`ye-container-mp4` 解封装 + 封装（含 fMP4）。
- **M2**：H.264 软件解码（先 Baseline，后 High / CABAC / B 帧），
  以 FFmpeg 为 oracle 建立一致性测试。准入需求：`jian` 时间线可回放。
- **M3**：处理图最小集（像素格式转换、scale）；`ye probe` / `ye convert`
  端到端跑通。准入需求：`duan` 完成一次转码任务。
- **M4**：H.264 软件编码（CRF / CBR、2-pass）；AAC-LC 编解码。
- **M5**：硬件加速铺开（VideoToolbox / D3D11VA / VA-API / NVENC）；
  Matroska 容器。
- **M6+**：HEVC、ProRes、DNxHR、MXF 等，按工具需求排期。

原则：每个里程碑以某个真实工具的消费需求为准入条件，不为了齐备而齐备。

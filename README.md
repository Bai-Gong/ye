# 百工·冶 (Ye)

对标 **FFmpeg（libav\*）** 的媒体编解码与处理基础库，纯 Rust 实现。

## 定位

- 百工系基础设施，与 `fang/` 同级：**不以独立产品交付**，以 Cargo 依赖形式接入
  剪（`jian`）、锻（`duan`）、乐（`yue`）等 Tauri 创作工具的后端。
- API 从真实工具开发中需求驱动地沉淀，不做超前抽象。

## 命名

《考工记》"攻金之工六"：筑、冶、凫、栗、段、桃。**冶氏主冶炼**——把矿石炼成可用的金属。
媒体处理同理：把压缩码流"冶炼"为原始帧（解码），再"铸"回型材（编码、封装）。

冶（炼材）→ 锻（成器）：`ye/` 提供底层编解码与封装能力，`duan/` 在其上构建转码产品。

## 设计原则

1. **纯 Rust，零 GPL/LGPL 依赖**。对外保持 `MIT OR Apache-2.0` 双许可
   （正式分发前补齐 LICENSE 文本），可安全并入闭源商业发行。
2. **FFmpeg 只作测试基准（oracle），不进产品**。解码器与 FFmpeg 逐帧比对验证
   正确性，借鉴 FilmCraft 的工程实践。
3. **分层解耦**：时基/像素格式 → 编解码 → 封装 → 处理图 → 设备，层间只经 trait 交互。
4. 具体编解码器与容器以独立 crate 落地（如 `ye-codec-h264`、`ye-container-mp4`），
   由 `ye` 门面按 feature 汇总，控制编译时间与产物体积。

## 路径结构

```
ye/
├── Cargo.toml            # workspace 根
├── crates/
│   ├── ye/               # 门面 crate：统一 re-export，外部工具只依赖这一个
│   ├── ye-time/          # 有理数时基、时间戳、帧率换算（对标 AVRational）
│   ├── ye-pixfmt/        # 像素格式、色彩空间、采样布局的描述
│   ├── ye-codec/         # 编解码 trait 与注册表（Packet ⇄ Frame）
│   ├── ye-container/     # 封装/解封装 trait 与注册表（Stream ⇄ Packet）
│   ├── ye-filter/        # 媒体处理图（对标 lavfi）
│   ├── ye-device/        # 硬件加速后端（VideoToolbox / D3D11VA / VA-API / NVENC）
│   └── ye-cli/           # `ye` 命令行（对标 ffmpeg / ffprobe）
├── docs/
│   ├── architecture.md   # 分层架构与数据模型
│   └── roadmap.md        # 里程碑
└── tests/
    └── conformance/      # 一致性测试：FFmpeg oracle 比对策略与测试流索引
```

未来的 `ye-codec-h264`、`ye-codec-aac`、`ye-container-mp4` 等实现 crate
同样放在 `crates/` 下，命名规则 `ye-codec-<名>` / `ye-container-<名>`。

## 分支与协作

遵循百工目录统一约定：`main` 稳定 / `develop` 日常开发 / `qa` 测试验证。

## 路线图

见 [docs/roadmap.md](docs/roadmap.md)。概要：M1 容器读写（MP4）→
M2 H.264 解码 → M3 处理图与 CLI → M4 H.264 编码 → M5 硬件加速铺开。

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
   正确性。
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
│   ├── ye-container-mp4/ # MP4 解封装与封装实现（feature `mp4`，progressive 与 fMP4）
│   ├── ye-codec-h264/    # H.264 解码器与编码器（含 AAC 见 ye-codec-aac）
│   ├── ye-filter/        # 媒体处理图（对标 lavfi）
│   ├── ye-device/        # 硬件加速后端（VideoToolbox / D3D11VA / VA-API / NVENC）
│   └── ye-cli/           # `ye` 命令行（对标 ffmpeg / ffprobe）
├── docs/                 # 架构 / 契约 / 路线图 / 等待清单 / 历史施工图（见下）
└── tests/
    └── conformance/      # 一致性测试：FFmpeg oracle 比对策略与测试流索引
```

## 快速上手

```console
$ cargo run -p ye-cli -- probe 素材.mp4
素材.mp4: mp4, 2 路流
流 #0 [视频] codec=h264 …
流 #1 [音频] codec=aac …

$ cargo run -p ye-cli -- remux 素材.mp4 直通.mp4     # 不转码直通（ffmpeg -c copy 等价）
$ cargo run -p ye-cli -- convert 素材.mp4 出片.mp4 --size 320x240   # 转码（重编码 + 封装）
```

作为库使用：只依赖门面 `ye`，容器能力按 feature 开启（`mp4` 默认开启）：

```toml
ye = { path = "../ye", features = ["mp4"] }
```

```rust
use ye::container::mp4::Mp4Reader;

let reader = Mp4Reader::open("素材.mp4")?;
for s in reader.streams() {
    println!("流 #{} {:?} 时长 {:?}", s.index, s.codec.codec, s.duration);
}
```

封装走 `ye::container::mp4::Mp4Writer`，直通走 `ye::container::remux`；
转码与多段时间线合成分别走 `ye::convert` / `ye::compose` 门面
——契约见 [docs/contracts.md](docs/contracts.md)。

## 构建与测试

在 `ye/` 下执行：`cargo test`、`cargo clippy --all-targets -- -D warnings`、
`cargo fmt --check`。

## 文档

| 文档 | 内容 |
| --- | --- |
| [docs/architecture.md](docs/architecture.md) | 分层架构与数据模型 |
| [docs/contracts.md](docs/contracts.md) | 各层已落地的接口契约 |
| [docs/roadmap.md](docs/roadmap.md) | 里程碑、排期与修订记录 |
| [docs/waitlist.md](docs/waitlist.md) | 能力缺口等待清单 |
| [docs/m2-jian-plan.md](docs/m2-jian-plan.md)、[docs/m3-plan.md](docs/m3-plan.md) | 历史施工图归档 |

## 分支与协作

遵循百工目录统一约定：`main` 稳定 / `develop` 日常开发 / `qa` 测试验证，
提交规范与推送顺序见集群 [AGENTS.md](../AGENTS.md)。

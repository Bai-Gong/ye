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
│   ├── ye-container-mp4/ # MP4 解封装与封装实现（feature `mp4`，progressive 与 fMP4）
│   ├── ye-filter/        # 媒体处理图（对标 lavfi）
│   ├── ye-device/        # 硬件加速后端（VideoToolbox / D3D11VA / VA-API / NVENC）
│   └── ye-cli/           # `ye` 命令行（对标 ffmpeg / ffprobe）
├── docs/
│   ├── architecture.md   # 分层架构与数据模型
│   ├── contracts.md      # 各层已落地的接口契约
│   └── roadmap.md        # 里程碑
└── tests/
    └── conformance/      # 一致性测试：FFmpeg oracle 比对策略与测试流索引
```

未来的 `ye-codec-h264`、`ye-codec-aac`、`ye-container-mkv` 等实现 crate
同样放在 `crates/` 下，命名规则 `ye-codec-<名>` / `ye-container-<名>`。

## 快速上手

```console
$ cargo run -p ye-cli -- probe 素材.mp4
素材.mp4: MP4, 2 路流
流 #0 [视频] codec=avc1 时基=1/1000 时长=0.030s (211680 tick) 样例数=3 尺寸=64x96
流 #1 [音频] codec=mp4a 时基=1/48000 时长=0.001s (7056 tick) 样例数=1 声道=2 采样率=48000Hz

$ cargo run -p ye-cli -- remux 素材.mp4 直通.mp4
素材.mp4 → 直通.mp4: MP4, 2 路流, 55 样例直通（不转码）
```

作为库使用：只依赖门面 `ye`，容器/编解码能力按 feature 开启
（当前仅 `mp4`，默认开启）：

```toml
ye = { path = "../ye", features = ["mp4"] }
```

```rust
use ye::container::mp4::Mp4Reader;

let reader = Mp4Reader::open("素材.mp4")?;
for s in reader.streams() {
    println!("{} {:?}", s.codec, s.duration);
}
```

封装走同一个门面路径（`ye::container::mp4::Mp4Writer`，progressive
与 fMP4 均可），用法见 [docs/contracts.md](docs/contracts.md)。

feature 接线规则与各层接口契约见 [docs/contracts.md](docs/contracts.md)。

## 当前状态（2026-10）

M1 读写已闭环：

- `ye-time` / `ye-pixfmt` 基础类型（零第三方依赖）。
- `ye-container-mp4`：MP4 解封装（progressive 与 fMP4）+ 封装
  （progressive 与 fMP4），零第三方依赖；以 PATH 上的 ffmpeg/ffprobe
  作 oracle 对拍（流参数、逐包时戳/大小/关键帧严格一致）。
- 门面 feature 接线（`ye/mp4` → `ye-container/mp4`）；
  `ye-cli` 提供 `ye probe` 与 `ye remux`（不转码重封装直通）。
- 测试 125 个通过（单元层手工构造字节；oracle 组无 ffmpeg 即跳过），
  零网络、临时文件全在 target/ 下。

构建与门禁：`cargo test`、`cargo clippy --all-targets -- -D warnings`、
`cargo fmt --check`（在 `ye/` 下执行）。

## 分支与协作

遵循百工目录统一约定：`main` 稳定 / `develop` 日常开发 / `qa` 测试验证。

## 路线图

见 [docs/roadmap.md](docs/roadmap.md)。概要（按剪/锻的消费排期）：M1 容器
读写＋probe（收口＝剪的 probe 接入）→ M2 解码（剪抽帧/波形）→ M3 编码＋
转码直通（锻首个任务）→ M4 合成导出（接管剪的导出）→ M5 格式铺开＋硬件
加速。

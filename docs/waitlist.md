# M2+ 等待清单

冶（ye）在 M1 收口后**还做不到**、消费方（剪/锻）在等的能力总账：
每个缺口标**阻塞层**（ye 内哪一层挡着）与**放行里程碑**。配套：
接口契约见 [contracts.md](contracts.md)，排期见 [roadmap.md](roadmap.md)，
桩语义（可构造 / 可查询 / 调用即 Unsupported）见 contracts.md
「M2+ 接口桩契约」一节。

**已可用（M1 交付，不在此表）**：MP4（progressive 与 fMP4）读 + 写 +
probe（流表 / 帧率 / 码率）+ remux 直通，经 `ContainerRegistry`
auto-probe；`convert` 为占位桩（参数 / 进度 / 取消契约已定形，见
contracts.md「锻的最小 API 面」）。

| # | 缺口（消费方需求） | 阻塞层 | 放行里程碑 | 现状 |
| --- | --- | --- | --- | --- |
| 1 | 抽帧缩略图（剪 M1：H.264 帧 → PNG） | ye-codec（H.264 解码）+ ye-filter（yuv→rgb 像素转换 / scale）+ PNG 编码 | **M2** | 解码桩：注册可查、可构造，`feed`/`pull` 即 `UnsupportedStub` |
| 2 | 波形图（剪 M2：AAC → PCM 峰值） | ye-codec（AAC 解码） | **M2** | 同上 |
| 3 | 真转码（锻首个任务：改分辨率/码率出 MP4） | ye-codec（H.264/AAC 编码）+ ye-filter（处理图最小集）+ convert 真实现 | **M3** | convert 占位桩；编码桩 `UnsupportedStub`；处理图 `topological_order` 即 `Unsupported` |
| 4 | 导出合成（剪：filter_complex 全集 + 多输入时间线合成 + 进度） | ye-filter（全集与调度）+ 编码 + 合成 API | **M4** | 处理图桩（组装可用） |
| 5 | MOV 容器（QT 族导入） | ye-container（新实现 crate，MP4 近亲可大量复用） | 持续轨道·下一梯队 | 未开工；按剪的导入统计驱动（同梯队：WAV/MP3 纯探测） |
| 6 | MKV / WebM 容器 | ye-container（Matroska 实现 crate） | **M5** | 未开工 |
| 7 | GIF / 图片探测（纯探测，无解封装） | ye-container（纯探测工厂） | 未排期 | 未开工；按导入统计驱动 |

维护约定：

- 缺口关闭（能力交付）时删行，并在 [contracts.md](contracts.md) 补
  对应契约；真实现落地后同步删除对应桩（替换路径见「M2+ 接口桩
  契约」）。
- 新增缺口先进本表再排期；里程碑变更须同步 [roadmap.md](roadmap.md)
  修订记录。

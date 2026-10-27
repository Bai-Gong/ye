# 路线图

<!-- 2026-10-24 合流（B 案）：以下两段分别来自 develop（契约层合流前的
     原始里程碑）与 m1-mp4（按消费方重排的修订说明），裁决为双方内容
     都保留；现行排期以下方「里程碑」一节为准。 -->

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

> 2026-10-22 按剪、锻的消费计划重排：里程碑不再按"解码→滤镜→编码"的
> 库内能力顺序排，而是按消费方接入 ye 的先后排——先到的需求先做，
> ffmpeg 在对拍期继续给剪/锻顶班。

## 消费方需求对照（排期依据）

剪的方案见 [jian/docs/开发方案.md](../../jian/docs/开发方案.md)
（里程碑 M1–M5）与待开发清单；锻目前只有定位（对标 Media Encoder 的
转码工具，方案未细化），以"第一个转码任务"为准入锚点。

| 消费方 | 需求（时间序） | 冶的承接 | 里程碑 |
| --- | --- | --- | --- |
| 剪 M1（已落地） | probe 素材元数据 | MP4 解封装 + `ye probe` | M1 |
| 剪 M1（已落地） | 抽帧缩略图（PNG） | H.264 解码 + 像素转换/scale + PNG 编码 | M2 |
| 剪 M2（音频基础） | 波形图 | AAC 解码 → PCM 峰值 | M2 |
| 剪 M1 起持续加重 | 导出合成（filter_complex 全集 + H.264/AAC 编码 + 进度） | 合成滤镜全集 + 编码 + 时间线合成 API | M4 |
| 剪 M3（时间自由） | 速度曲线导出 | 分段变速的 setpts 等价能力 | M4 |
| 剪 M4（效果自由） | 效果/转场滤镜 | ye-filter 按双端注册表逐个落地 | M4 尾～M6+ |
| 剪 M5（进阶） | LUT 调色、代理剪辑 | LUT 滤镜；代理转码与锻共用管线 | M5/M6+ |
| 锻（首个任务） | MP4/H.264+AAC → 改分辨率/码率的 MP4 | 解码→滤镜→编码直通 + `ye convert` | M3 |
| 锻（后续） | 格式铺开、预设、批量队列 | 容器/编解码扩展 + 处理图扩容 | M5/M6+ |

剪的预览走浏览器（video 池 → canvas → WebCodecs），不经 ye——冶只承接
Rust 后端的媒体任务（probe / 抽帧 / 波形 / 导出 / 代理），不承接预览解码。

## 里程碑

- **M1（已完成，收口中）**：`ye-time` / `ye-pixfmt` 基础类型；
  `ye-container-mp4` 解封装 + 封装（progressive 与 fMP4），ffmpeg/ffprobe
  oracle 对拍通过；`ye probe` / `ye remux` 可用。2026-10-24 合流收口：
  m1-mp4 并入 develop（B 案，契约层为主体）、移植挂 develop 契约类型、
  `Demuxer`/`Muxer` trait 接线 + 注册表 auto-probe、门面 `remux` 助手、
  `convert` 占位（锻 M3 接入契约定形）、ye-codec/ye-filter 接口桩就位
  ——M2+ 缺口总账见 [waitlist.md](waitlist.md)。收口准入：**剪的素材
  导入 probe 换 ye**——jian 侧 `probe_media` 加双引擎开关，ffprobe 留作
  oracle 与回退。
- **M2 解码——服务剪的抽帧与波形**：H.264 软件解码（先 Baseline，后
  High / CABAC / B 帧）、AAC-LC 解码、yuv→rgb 像素转换与 scale 最小集、
  PNG 编码；音频 PCM → 波形峰值。进度勾项（2026-10-27 收口终态，m2-jian-plan
  T1–T8 + 同日解码器分歧修复）：✅ Baseline 解码（一致性向量口径）、✅
  AAC-LC 解码、✅ yuv→rgb 与 scale、✅ PNG 编码、✅ 波形峰值（纯核 +
  消费助手 + oracle 对拍两条向量全绿，容差收紧 2e-4→1.5e-4）、✅ 抽帧门面
  （`decode_frame_rgba`/`grab_frame_png` + vec-video-multi 整体 Err 行为
  锁定）、✅ 抽帧像素/解码 YUV 对拍——早班发现的去块启用 + 30fps 长链分歧
  （max_abs=74~76）同日修复收口（去块延后图级 + chroma QP 偏移 +
  pred_8x16 C→D 回退），`ip30` 回归集入库（去块条 90 帧逐位一致），两条
  对拍用例翻除 `#[ignore]` 过门（像素三点位 max |Δ|=1；no-deblock 条
  max_abs=1 余留归档 ffmpeg 舍入边角，容差内）；High/CABAC 收口沿 M2
  收口轨道开放（waitlist.md 第 1 行）。准入（剪的抽帧/波形走 ye 且同点位
  对拍在容差内）：**代码面达成**——剪侧 probe/抽帧/波形三命令双引擎落地
  （jian T4–T8），波形/probe 对拍全绿、抽帧像素对拍 ye 侧过门；jian 侧
  像素用例未随修复翻除（`#[ignore]` 遗留）与「剪 M1 已定稿」GUI 人工回归
  两项欠账如实登记（m2-jian-plan 收尾实录遗留①⑤）。
- **M3 编码与转码直通——锻启动**：H.264 软件编码（CRF / CBR，2-pass
  可后置）、AAC-LC 编码、处理图最小集（scale / fps / 像素格式 / 裁剪 /
  音量）、`ye convert` 端到端。准入：**锻完成第一个转码任务**（改分辨率
  与码率，时长/画面与 ffmpeg 对拍）；锻的工程骨架（Tauri+React，集群
  套路）随本里程碑启动。
- **M4 合成导出——接管剪的导出**：处理图扩到剪 filter_complex 全集——
  overlay 链 / scale / rotate / 不透明度 / color 底板 / 时间窗显隐，音频
  atempo（多段拆分）/ volume / adelay / amix，含分段速度曲线的 setpts
  等价能力；多输入时间线合成 API + 进度回调（剪的 buildExportArgs 降级
  为"求值 → 调 ye"）。准入：剪导出走 ye 出片，与 ffmpeg 导出对拍
  （时长差 < 一帧、同点位抽帧视觉一致），剪保留双引擎开关一版。
- **M5 格式铺开 + 硬件加速**：HEVC 解码（手机素材主力）、Matroska
  （MKV/WebM）；硬解（D3D11VA / VideoToolbox / VA-API）与硬编
  （NVENC / AMF / VideoToolbox）。准入：剪可导入 HEVC 素材出片；锻一次
  硬编转码跑通（软硬结果对拍）。剪的代理剪辑在此提速。
- **M6+**：HEVC / AV1 编码、ProRes / DNxHR / MXF、LUT 与调色滤镜
  （剪 M5）、文字渲染归属决策（剪侧渲染成图片素材 vs ye-filter 文字
  滤镜）、锻预设生态所需扩展。按届时工具的真实节奏排期。

## 持续轨道（不占里程碑编号）

- **探测/解封装格式覆盖**：剪用户导入什么就补什么。下一梯队：MOV
  （QT 族，MP4 近亲）、WAV / MP3（纯探测）、GIF；按剪的导入统计驱动，
  不预先铺开。
- **oracle 一致性**：每个解码器/滤镜以 ffmpeg 为基准逐帧（音频逐样本）
  对拍，容差在 `tests/conformance` 逐项声明；剪/锻切换 ye 前必跑。

## 接入方式

- 剪/锻均为 Tauri：后端以 cargo 依赖门面 `ye`（feature 按需），不走
  sidecar 进程；ffmpeg/ffprobe 在对拍期保留为 oracle 与回退开关。
- 切换不阻塞剪/锻自己的排期：ye 里程碑交付前，剪继续用 ffmpeg 现路径
  （probe / 抽帧 / 导出）顶着，交付后再逐点切换。
- 每接入一个调用点，对拍通过后 ffmpeg 对应命令才下线（探测层格式覆盖
  够用前，ffprobe 开关不下线）。

## 原则

- 每个里程碑以剪/锻某个真实消费点为准入条件，不为了齐备而齐备。
- 剪的预览（浏览器解码）不迁就 ye；导出一致性靠"同一求值 + 双引擎
  对拍"兜底。

## 修订记录

- 2026-10-27：M2 收口——施工 8/8（m2-jian-plan T1–T8）+ 同日解码器分歧
  修复收口，两仓门禁全绿。**接通状态（剪侧，T4–T8）**：probe 双引擎
  （`engine` 参数，auto=ye 优先、疑似封面启发式〔多条视频流 / 短视频流 /
  moov.udta.meta covr 盲区〕整体回退 ffprobe 轨，AAC priming 时长偏差使
  音频族整体回退权威轨）、抽帧双引擎（`extract_frame_ye` 命令，前端
  thumbnail.ts ye 优先、失败回退 ffmpeg 轨）、波形命令（auto 路由，
  `waveform_peaks_ye`/`waveform_peaks_ffmpeg` 两 pub 轨可独立调）——
  波形双轨正弦/白噪声两向量逐桶全绿（jian 侧最坏 |Δmin|=3.588e-5 ≤
  容差 1.5e-4）、probe 双轨判等全绿（宽高/kind 精确等，带封面 m4a 回退
  后逐字段一致）。**解码器分歧处置**：早班像素对拍暴露的两处分歧（去块
  启用首差帧 0 luma：解码期行内去块污染帧内预测 + chroma QP 漏加 PPS
  偏移；关去块首差帧 81 chroma：pred_8x16 缺 C→D 回退）同日修复收口
  ——去块延后到图级（编码器本地重建同步对齐，位级回环保持 0 容差）、
  `chroma_qp_offset` 接线、pred_8x16 回退补齐；`ip30` 回归集（90 帧
  30fps 长链 × 去块/关去块两条）入 conformance 清单，像素/YUV 对拍用例
  翻除 `#[ignore]` 过门（像素三点位 max |Δ|=1、去块条 90 帧逐位一致）。
  **未过项/遗留（如实）**：① no-deblock 条帧 81 起 Cr 平面孤立 ±1 样本
  （300 帧扩展实测 24 帧 × 每帧 ≤2 样本，M2 YUV 容差〔≥99.9% Δ=0，实测
  ≥99.998%〕内过门；二轮复核已排除 MC/参考选择/残差反量化/idct 语义，
  归档 ffmpeg 内部舍入边角——收紧 `ip30` 容差前须先收口，见
  ye-codec-h264 tests/conformance.rs `ip30` 注释）；② jian 侧像素对拍
  用例仍挂 `#[ignore]` 未随修复翻除（经路径依赖已链修复后的 ye，翻除
  复跑归 jian 后续）；③ High/CABAC 真流对拍仍开放（waitlist.md 第 1
  行）。收尾实录与遗留总账见
  [m2-jian-plan.md](m2-jian-plan.md) 文末。
- 2026-10-27：剪接线 ye 侧交付（m2-jian-plan T1–T3）——`waveform`
  模块（分桶纯核 + 消费助手）与 `frame_grab` 门面入库；oracle 对拍
  基建（`tests/common/mod.rs` 定位器 + lavfi 向量配方，四档定位 +
  分级 skip 语义）与两个对拍测试文件落地。实测：波形对拍正弦/白噪声
  两条向量逐桶逐声道全绿（最坏 |Δmax|=5.022e-6 / 8.810e-5，容差按
  实测收紧 2e-4→1.5e-4）；vec-video-multi（`-ac 6` 不可解音轨）抽帧
  整体 Err 行为锁定生效；抽帧像素对拍发现 H.264 Baseline 解码在
  去块启用 + 30fps 长链真实内容上与 ffmpeg 分歧（90 帧 max_abs=74、
  首差帧 0 luma；关去块对照仍 max_abs=76、首差帧 81 chroma；像素核
  |Δ|≤1 与帧定位已排除，归因解码器；M2 一致性向量全部 `no-deblock=1`
  未覆盖该形态）——像素/YUV 两条对拍用例以 `#[ignore]` 如实登记，
  解码器修复工程师接手后翻除过门（证据链见
  [waitlist.md](waitlist.md) 第 1 行）。M2 准入（剪侧接线 + 对拍在
  容差内）仍开放（jian 侧 T4–T8 在途）。
- 2026-10-26：M3 首轮收尾——施工 6/6 + 集成阶段同轮落地，全仓门禁
  全绿、端到端转码通过、无未过项。交付：H.264 软件编码真实现
  （Baseline/CAVLC、I/P 帧、定 QP + 简单 CRF、无 B 帧；H.264 编码桩
  随删）、处理图最小集（`topological_order` 真实现、`FilterParams`、
  crop/fps/volume、fps API 改 `Timestamp` 入参）、convert 编码模式
  真实现（视频重编码 + MP4 封装、音频直通）+ `ye convert` CLI
  （--size）。对拍：编码器位级回环 0 容差 + ffmpeg 双解码 + Y-PSNR
  门槛（QP20≥36dB / QP32≥28dB）全过；端到端经 ffprobe 三重验收
  （解码无错 / 流参数 / 时长与帧数一致）。施工图与逐任务实录见
  [m3-plan.md](m3-plan.md)。范围外如实登记：AAC-LC 编码、真码控
  （CBR/A-VBR、2-pass、完整 CRF）、B 帧/CABAC/High 编码、分块 inter
  模式归后续批次（m3-plan §7）；CABAC 解码真流对拍卡点沿 M2 收口
  轨道开放（m3-plan §6）；锻 duan 接线（D1/D2）未动——M3 准入
  （锻完成第一个转码任务）仍开放，[waitlist.md](waitlist.md) 已同步。
- 2026-10-24：M1 冶侧收口——m1-mp4 合流（B 案）+ 移植挂 develop
  契约类型 + `Demuxer`/`Muxer` trait 接线 + 门面 `remux` 助手 +
  `convert` 占位（锻 M3 接入契约定形）+ ye-codec/ye-filter 接口桩；
  新增 [waitlist.md](waitlist.md)（M2+ 等待清单：缺口 × 阻塞层 ×
  准入里程碑）。收口准入（剪 probe 双引擎）仍开放。
- 2026-10-22：按剪/锻消费计划重排——M2 准入从"剪时间线可回放"改为
  "剪抽帧/波形"（剪的预览走浏览器，不经 ye，原准入是伪需求）；原
  M3 滤镜最小集与 M4 编码合并为新 M3 转码直通（convert 端到端本来就
  需要编码，锻首个任务作准入）；新增 M4 合成导出承接剪的
  filter_complex 全集；硬件加速与格式铺开归 M5；新增消费方对照表、
  持续轨道与接入方式两节。
- 2026-10-20：初版（M1 收口时留档）。

//! 门面桩聚合入口（`ye::stubs`）的回归：feature 开启后入口可达，
//! 且桩语义（可构造 / 可查询 / 调用即 Unsupported）经门面路径成立。
//!
//! 默认 feature 集不含 `codec-stubs`/`filter-stubs`，本文件仅在
//! `cargo test -p ye --features codec-stubs,filter-stubs` 下有内容
//! （不属 `cargo test --workspace` 的默认门槛，见 docs/contracts.md
//! 「M2+ 接口桩契约」）。
//!
//! 2026-10-26 随 M3 编码收口更新：**H.264 编码桩已删除**；同日 M3 补齐
//! **AAC-LC 编码桩删除**（`register_stubs` 收窄为 解码×2，编码位分别经
//! 真工厂 `ye_codec_h264::encoder_factory` / `ye_codec_aac::encoder_factory`
//! 注册）；**处理图桩已由 `topological_order` 真实现取代**（M3 处理图
//! 最小集，`Unsupported` 变体随之删除）——两 feature 的聚合入口保留为
//! 「本构建依赖桩面/处理图最小面」的显式标记，语义断言随实际面同步。

#![cfg(any(feature = "codec-stubs", feature = "filter-stubs"))]
#![forbid(unsafe_code)]

// —— 编解码桩：注册可查、可构造、feed/pull 即 UnsupportedStub ——

#[cfg(feature = "codec-stubs")]
mod codec_stubs {
    use ye::codec::{CodecError, CodecId, CodecParameters, MediaType, Packet};
    use ye::stubs::codec::register_stubs;

    #[test]
    fn queryable_creatable_then_unsupported() {
        let mut reg = ye::codec::CodecRegistry::new();
        register_stubs(&mut reg);
        // 可查询：注册项经 supports_* / *_ids 可见
        assert!(reg.supports_decoder(CodecId::H264));
        assert!(reg.supports_decoder(CodecId::Aac));
        // 编码桩已全部删除（H.264：M3 真实现接管；AAC-LC：M3 补齐真
        // 实现接管）：桩注册表不再提供任何编码位
        assert!(
            !reg.supports_encoder(CodecId::H264),
            "H.264 编码桩已随真实现删除，桩注册表不得再提供"
        );
        assert!(
            !reg.supports_encoder(CodecId::Aac),
            "AAC 编码桩已随真实现删除，桩注册表不得再提供"
        );
        assert_eq!(reg.encoder_ids().collect::<Vec<_>>(), Vec::new());
        // 可构造：任意参数一律成功，codec_id() 可查
        let mut dec = reg
            .create_decoder(&CodecParameters::new(CodecId::H264, MediaType::Video))
            .unwrap();
        assert_eq!(dec.codec_id(), CodecId::H264);
        // 不可用：feed/pull 调用即 UnsupportedStub，文案含去向标注
        match dec.feed(&Packet::empty(0)) {
            Err(CodecError::UnsupportedStub(msg)) => {
                assert!(msg.contains("桩：M2 交付"), "msg: {msg}");
            }
            other => panic!("expected UnsupportedStub, got {other:?}"),
        }
        assert!(matches!(dec.pull(), Err(CodecError::UnsupportedStub(_))));
        // flush 无操作
        dec.flush();
    }

    /// H.264 编码经真工厂注册可查（桩收窄的对偶面）：门面默认 feature
    /// 集不含 `h264`，经 dev 依赖的 `ye_codec_h264::encoder_factory`
    /// 直连（集成阶段开 `h264` feature 后同一工厂经
    /// `ye::codec_h264::register_h264_encoder` 注册）。
    #[test]
    fn h264_encode_via_real_factory_not_stub() {
        let mut reg = ye::codec::CodecRegistry::new();
        register_stubs(&mut reg);
        assert!(!reg.supports_encoder(CodecId::H264));
        let old = reg.register_encoder(CodecId::H264, ye_codec_h264::encoder_factory);
        assert!(old.is_none(), "桩收窄后该位为空");
        assert!(reg.supports_encoder(CodecId::H264));
        // 真工厂校验参数：质量值越界（QP 0）→ InvalidArguments
        let bad = ye::codec::EncoderConfig {
            codec: CodecId::H264,
            time_base: ye::time::Rational::new(1, 25),
            bit_rate: None,
            video: Some(ye::codec::VideoEncoderConfig {
                width: 96,
                height: 64,
                format: ye::pixfmt::PixelFormat::Yuv420p,
                frame_rate: ye::time::Rational::RATE_25,
                gop: 12,
                quality: ye::codec::QualityMode::Qp(0),
            }),
            audio: None,
        };
        assert!(
            matches!(
                reg.create_encoder(&bad),
                Err(CodecError::InvalidArguments(_))
            ),
            "真工厂须校验参数（QP 0 拒收）"
        );
    }
}

// —— 处理图（最小集已交付）：组装 + 拓扑排序真实生效 ——

#[cfg(feature = "filter-stubs")]
mod filter_stubs {
    use ye::codec::MediaType;
    use ye::stubs::filter::{
        FilterDescriptor, FilterError, FilterGraph, FilterParams, PadDescriptor, PadRef,
    };

    fn video_filter(name: &str) -> FilterDescriptor {
        let pad = PadDescriptor {
            name: "default".to_string(),
            media: MediaType::Video,
        };
        FilterDescriptor {
            name: name.to_string(),
            inputs: vec![pad.clone()],
            outputs: vec![pad],
            params: FilterParams::None,
        }
    }

    /// M3 处理图最小集后：`topological_order` 为真实现（Kahn，成环
    /// 报 `FilterError::Cycle`——原 `Unsupported` 桩已随交付删除）。
    /// 本组经聚合入口 `ye::stubs::filter` 走与 `ye::filter` 同一批类型。
    #[test]
    fn graph_assembles_and_toposort_is_real() {
        let mut g = FilterGraph::new();
        let a = g.add_filter(video_filter("scale"));
        let b = g.add_filter(video_filter("format"));
        let c = g.add_filter(video_filter("crop"));
        // 组装真实生效：connect 记边、结构可读回
        g.connect(PadRef { node: a, pad: 0 }, PadRef { node: b, pad: 0 })
            .unwrap();
        g.connect(PadRef { node: b, pad: 0 }, PadRef { node: c, pad: 0 })
            .unwrap();
        assert_eq!((g.node_count(), g.edge_count()), (3, 2));
        assert_eq!(g.filter(a).unwrap().name, "scale");
        // 真实现：依赖序可排（每节点排在其全部上游之后）
        assert_eq!(g.topological_order().unwrap(), vec![a, b, c]);
        // 成环 → Cycle（原 Unsupported 变体已删除，编译面即验证）
        g.connect(PadRef { node: c, pad: 0 }, PadRef { node: a, pad: 0 })
            .unwrap();
        assert!(matches!(g.topological_order(), Err(FilterError::Cycle(_))));
    }
}

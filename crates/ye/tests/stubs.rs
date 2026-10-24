//! 门面桩聚合入口（`ye::stubs`）的回归：feature 开启后入口可达，
//! 且桩语义（可构造 / 可查询 / 调用即 Unsupported）经门面路径成立。
//!
//! 默认 feature 集不含 `codec-stubs`/`filter-stubs`，本文件仅在
//! `cargo test -p ye --features codec-stubs,filter-stubs` 下有内容
//! （不属 `cargo test --workspace` 的默认门槛，见 docs/contracts.md
//! 「M2+ 接口桩契约」）。

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
        assert!(reg.supports_encoder(CodecId::H264));
        assert!(reg.supports_encoder(CodecId::Aac));
        // 可构造：任意参数一律成功，codec_id() 可查
        let mut dec = reg
            .create_decoder(&CodecParameters::new(CodecId::H264, MediaType::Video))
            .unwrap();
        assert_eq!(dec.codec_id(), CodecId::H264);
        // 不可用：feed/pull 调用即 UnsupportedStub，文案含里程碑标注
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
}

// —— 处理图桩：组装真实生效、topological_order 即 Unsupported ——

#[cfg(feature = "filter-stubs")]
mod filter_stubs {
    use ye::codec::MediaType;
    use ye::stubs::filter::{FilterDescriptor, FilterError, FilterGraph, PadDescriptor, PadRef};

    fn video_filter(name: &str) -> FilterDescriptor {
        let pad = PadDescriptor {
            name: "default".to_string(),
            media: MediaType::Video,
        };
        FilterDescriptor {
            name: name.to_string(),
            inputs: vec![pad.clone()],
            outputs: vec![pad],
        }
    }

    #[test]
    fn graph_assembles_then_toposort_unsupported() {
        let mut g = FilterGraph::new();
        let a = g.add_filter(video_filter("scale"));
        let b = g.add_filter(video_filter("format"));
        // 组装真实生效：connect 记边、结构可读回
        g.connect(PadRef { node: a, pad: 0 }, PadRef { node: b, pad: 0 })
            .unwrap();
        assert_eq!((g.node_count(), g.edge_count()), (2, 1));
        assert_eq!(g.filter(a).unwrap().name, "scale");
        // 不可用：topological_order 调用即 Unsupported，文案含里程碑标注
        match g.topological_order() {
            Err(FilterError::Unsupported(msg)) => {
                assert!(msg.contains("桩：M4 交付"), "msg: {msg}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }
}

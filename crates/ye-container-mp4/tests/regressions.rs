//! 上一轮评审缺陷的回归测试：
//! extra_data 未按子 box 声明 size 截断 / 巨型样例分配未按文件长度
//! 校验 / box 树递归无深度上限 / 声明值与文件长度失衡（trun 无逐样例
//! 字段的巨量样例、locate 定长求和随序号线性增长）。
//!
//! 移植后 extra_data 位于 `Stream.codec.extradata`，四个加固回归的
//! 断言本体原样保留。

mod helpers;

use helpers::*;
use ye_container_mp4::{Mp4Error, Mp4Reader};

/// avcC 之后还有兄弟 box（pasp/btrt）时，extra_data 只含 avcC 声明
/// size 内的载荷，兄弟 box 的字节不得混入（回归：child_payload 曾把
/// 头之后的字节整段取到 entry 末）。
#[test]
fn extra_data_stops_at_first_child_declared_size() {
    let pasp = r#box("pasp", &[0, 0, 0, 1, 0, 0, 0, 1]); // hSpacing/vSpacing 16.16
    let btrt = r#box("btrt", &[0, 0, 0x10, 0, 0, 1, 0, 0, 0, 1, 0, 0]);
    let stbl = vec![
        stsd_video_raw(64, 96, &[r#box("avcC", &AVCC), pasp, btrt]),
        stts(&[(1, 10)]),
        stsc(&[(1, 1, 1)]),
        stsz_var(&[4]),
        stco(&[0]),
    ];
    let r = Mp4Reader::from_vec(file_with(&[video_trak(&stbl, 1000, 30)])).unwrap();
    let s = r.stream(0).unwrap();
    assert_eq!(
        s.codec.extradata,
        Some(AVCC.to_vec()),
        "extra_data 应止于 avcC 声明 size"
    );
}

/// 子 box size==0 ⇒ 载荷延伸到 entry 末（尾随字节并入）；
/// 子 box 声明 size 越出 entry ⇒ Malformed。
#[test]
fn extra_data_child_size_zero_and_overflow() {
    // size==0 的 avcC：载荷 = 头之后到 entry 末，尾随的 btrt 整个并入。
    let mut avcc0 = Vec::new();
    push(&mut avcc0, &be_u32(0));
    push(&mut avcc0, b"avcC");
    push(&mut avcc0, &AVCC);
    let tail = r#box("btrt", &[0; 12]);
    let mut expect = AVCC.to_vec();
    push(&mut expect, &tail);
    let stbl = vec![
        stsd_video_raw(64, 96, &[avcc0, tail]),
        stts(&[(1, 10)]),
        stsc(&[(1, 1, 1)]),
        stsz_var(&[4]),
        stco(&[0]),
    ];
    let r = Mp4Reader::from_vec(file_with(&[video_trak(&stbl, 1000, 30)])).unwrap();
    let s = r.stream(0).unwrap();
    assert_eq!(
        s.codec.extradata,
        Some(expect),
        "size==0 子 box 应取到 entry 末"
    );

    // 声明 size 远超 entry 实际长度 ⇒ Malformed。
    let mut big = Vec::new();
    push(&mut big, &be_u32(8 + 78 + AVCC.len() as u32 + 64));
    push(&mut big, b"avcC");
    push(&mut big, &AVCC);
    let stbl = vec![
        stsd_video_raw(64, 96, &[big]),
        stts(&[(1, 10)]),
        stsc(&[(1, 1, 1)]),
        stsz_var(&[4]),
        stco(&[0]),
    ];
    match Mp4Reader::from_vec(file_with(&[video_trak(&stbl, 1000, 30)])) {
        Err(Mp4Error::Malformed { reason, .. }) => {
            assert!(reason.contains("越出"), "reason={reason}");
        }
        other => panic!("应为 Malformed，实为 {other:?}"),
    }
}

/// stsz 定长声明近 4 GiB 的样例而文件极小：读样例前须按文件长度校验
/// 并返回 Malformed，不得先做巨型分配（回归：曾直接 vec![0u8; size]）。
#[test]
fn huge_sample_size_rejected_before_alloc() {
    let build = |chunk1: u32| {
        let stbl = vec![
            stsd_video(64, 96, &AVCC),
            stts(&[(1, 10)]),
            stsc(&[(1, 1, 1)]),
            stsz_fixed(u32::MAX, 1),
            stco(&[chunk1]),
        ];
        file_with(&[video_trak(&stbl, 1000, 30)])
    };
    let probe = build(0);
    let chunk1 = (probe.len() - MDAT.len()) as u32;
    let mut r = Mp4Reader::from_vec(build(chunk1)).unwrap();
    match r.read_packet(0, 0) {
        Err(Mp4Error::Malformed { path, reason }) => {
            assert!(path.ends_with("stsz"), "path={path}");
            assert!(reason.contains("越出文件"), "reason={reason}");
        }
        other => panic!("应为 Malformed，实为 {other:?}"),
    }
}

/// 数百层嵌套容器 box：超过深度上限返回 Malformed，不得栈溢出
/// （回归：walk 递归曾无深度上限）。
#[test]
fn overly_nested_containers_rejected() {
    let mut inner = r#box("free", &[0; 4]);
    for _ in 0..200 {
        inner = wrap("trak", &[inner]);
    }
    let mut f = Vec::new();
    push(&mut f, &ftyp());
    push(&mut f, &inner);
    match Mp4Reader::from_vec(f) {
        Err(Mp4Error::Malformed { reason, .. }) => {
            assert!(reason.contains("嵌套深度"), "reason={reason}");
        }
        other => panic!("应为 Malformed，实为 {other:?}"),
    }
}

/// 无逐样例字段的 trun（flags 仅 data-offset）声明巨量样例：解析期须按
/// 文件长度拒绝（Malformed），不得按声明的 sample_count 逐样例入表触发
/// 巨型分配 abort（回归：守卫 need(n*per_sample) 在 per_sample==0 时
/// 空转——52 字节文件曾令进程以「memory allocation of 120GB failed」死亡）。
#[test]
fn trun_without_per_sample_fields_bounded_by_file_len() {
    let evil = |count: u32| {
        // 整文件仅一个 moof（52 字节）：tfhd(0x020000 default-base-is-moof,
        // track=1，无任何默认值字段) + trun(v0, flags=0x000001 仅 data-offset,
        // sample_count=count)。
        let mut tfhd_p = Vec::new();
        push(&mut tfhd_p, &be_u32(0x0002_0000));
        push(&mut tfhd_p, &be_u32(1)); // track_ID
        let mut trun_p = Vec::new();
        push(&mut trun_p, &be_u32(0x0000_0001)); // v0 + data-offset 标志
        push(&mut trun_p, &be_u32(count)); // sample_count
        push(&mut trun_p, &be_u32(0)); // data_offset（相对 moof 起点）
        wrap(
            "moof",
            &[wrap(
                "traf",
                &[r#box("tfhd", &tfhd_p), r#box("trun", &trun_p)],
            )],
        )
    };
    // u32::MAX 档：修复前 open 直接 abort（~120GB 分配失败）。
    match Mp4Reader::from_vec(evil(u32::MAX)) {
        Err(Mp4Error::Malformed { reason, .. }) => {
            assert!(reason.contains("超出文件"), "reason={reason}");
        }
        other => panic!("应为 Malformed，实为 {other:?}"),
    }
    // 1e8 档：同样解析期拒绝，且 <100ms（修复前 3.65s + ~5.6GB 分配后才 Err）。
    let t0 = std::time::Instant::now();
    match Mp4Reader::from_vec(evil(100_000_000)) {
        Err(Mp4Error::Malformed { reason, .. }) => {
            assert!(reason.contains("超出文件"), "reason={reason}");
        }
        other => panic!("应为 Malformed，实为 {other:?}"),
    }
    assert!(
        t0.elapsed().as_millis() < 100,
        "应 <100ms 拒绝，实耗 {:?}",
        t0.elapsed()
    );
}

/// stsz 定长 size=1/count=u32::MAX + stsc 单段 samples_per_chunk=u32::MAX
/// 的微小文件：locate 对 chunk 内前序样例的求和不得随攻击者声明的样例
/// 序号线性增长——须先按文件长度做整体界校验，read_packet(0, 1e9) 立即
/// Malformed（回归：逐样例求和曾令 1e9 序号耗 ~1.13s 才被读前校验拦下）。
#[test]
fn locate_fixed_size_span_bounded_by_file_len() {
    let build = |c1: u32| {
        let stbl = vec![
            stsd_video(64, 96, &AVCC),
            stts(&[(u32::MAX, 10)]),
            stsc(&[(1, u32::MAX, 1)]),
            stsz_fixed(1, u32::MAX),
            stco(&[c1]),
        ];
        file_with(&[video_trak(&stbl, 1000, 30)])
    };
    // 占位探测真实 chunk 偏移（mdat 载荷起点，在文件内）。
    let probe = build(0);
    let c1 = (probe.len() - MDAT.len()) as u32;
    let mut r = Mp4Reader::from_vec(build(c1)).unwrap();
    assert_eq!(r.sample_count(0).unwrap(), u32::MAX as usize);
    let t0 = std::time::Instant::now();
    match r.read_packet(0, 1_000_000_000) {
        Err(Mp4Error::Malformed { reason, .. }) => {
            assert!(reason.contains("越出文件"), "reason={reason}");
        }
        other => panic!("应为 Malformed，实为 {other:?}"),
    }
    assert!(
        t0.elapsed().as_millis() < 10,
        "应立即（<10ms）拒绝，实耗 {:?}",
        t0.elapsed()
    );
}

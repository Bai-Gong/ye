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

/// 变长 stsz 单 chunk 大样例数的全量顺序读：locate 走 chunk 内前缀和
/// 缓存（首次访问该 chunk O(该 chunk 样例数) 一次性累积，此后 O(1)），
/// 整体必须近线性完成（回归：此前每次 locate 都对 chunk 内前序样例
/// 线性重求和，全量顺序读是平方级——评审实测 30k 样例 0.53s、60k
/// 2.10s，2× 样例 4× 耗时；200k 样例平方级预计 >20s，缓存路径 <1s）。
#[test]
fn locate_variable_stsz_sequential_read_is_not_quadratic() {
    const N: usize = 200_000;
    // 样例 size 交替 1/2（变长表形态，总跨度 300000 字节）。
    let sizes: Vec<u32> = (0..N).map(|i| 1 + (i % 2) as u32).collect();
    let total: usize = sizes.iter().sum::<u32>() as usize;
    let payload: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
    let build = |chunk1: u32| {
        let stbl = vec![
            stsd_video(64, 96, &AVCC),
            stts(&[(N as u32, 10)]),
            stsc(&[(1, N as u32, 1)]),
            stsz_var(&sizes),
            stco(&[chunk1]),
        ];
        let mut f = Vec::new();
        push(&mut f, &ftyp());
        push(&mut f, &moov(&[video_trak(&stbl, 1000, 30)]));
        push(&mut f, &r#box("mdat", &payload));
        f
    };
    let probe = build(0);
    let chunk1 = (probe.len() - payload.len()) as u32;
    let mut r = Mp4Reader::from_vec(build(chunk1)).unwrap();
    assert_eq!(r.sample_count(0).unwrap(), N);
    let t0 = std::time::Instant::now();
    let mut off = 0usize;
    for (i, &sz) in sizes.iter().enumerate() {
        let p = r.read_packet(0, i).unwrap();
        assert_eq!(p.data.len(), sz as usize, "样例 {i} 尺寸");
        assert_eq!(&*p.data, &payload[off..off + sz as usize], "样例 {i} 数据");
        off += sz as usize;
    }
    let elapsed = t0.elapsed();
    assert!(
        elapsed.as_secs() < 5,
        "全量顺序读应近线性（<5s），实耗 {elapsed:?}（平方级回归）"
    );
    // 缓存命中路径：乱序随机访问同样正确（同一 chunk 的前缀和复用）。
    for i in [N - 1, 0, N / 2, N - 2] {
        let p = r.read_packet(0, i).unwrap();
        assert_eq!(p.data.len(), 1 + (i % 2), "随机访问样例 {i}");
    }
}

/// stss 存在却缺样号 1（或空表）：解析期 Malformed（回归：此前
/// keyframe_index_before 的防御性兜底 Ok(0) 会把帧间帧当关键帧返回，
/// seek 落点从该处起解码花屏且不报 Err）。
#[test]
fn stss_missing_sample_one_is_malformed() {
    let build = |stss_box: Vec<u8>| {
        let stbl = vec![
            stsd_video(64, 96, &AVCC),
            stts(&[(3, 10)]),
            stss_box,
            stsc(&[(1, 2, 1), (2, 1, 1)]),
            stsz_var(&[4, 5, 6]),
            stco(&[0, 9]),
        ];
        file_with(&[video_trak(&stbl, 1000, 30)])
    };
    // stss={3}：0 号样例非关键帧，回溯落空。
    match Mp4Reader::from_vec(build(stss(&[3]))) {
        Err(Mp4Error::Malformed { reason, .. }) => {
            assert!(reason.contains("缺样号 1"), "reason={reason}");
        }
        other => panic!("应为 Malformed，实为 {other:?}"),
    }
    // 空表同样拒绝。
    match Mp4Reader::from_vec(build(stss(&[]))) {
        Err(Mp4Error::Malformed { reason, .. }) => {
            assert!(reason.contains("为空"), "reason={reason}");
        }
        other => panic!("应为 Malformed，实为 {other:?}"),
    }
}

/// ctts / stss 重复出现：与 stsd/stts/stsc/stsz/stco 一致地报
/// 「样例表 box 重复出现」（回归：此前静默后者覆盖前者，损坏文件
/// 改变时戳解读而不报 Err）。
#[test]
fn duplicate_ctts_and_stss_rejected() {
    let build = |extra: &[Vec<u8>]| {
        let stbl = [
            stsd_video(64, 96, &AVCC),
            stts(&[(3, 10)]),
            stsc(&[(1, 2, 1), (2, 1, 1)]),
            stsz_var(&[4, 5, 6]),
            stco(&[0, 9]),
        ];
        // 在 stsc 之前插入两份同表 box。
        let mut with_dup = vec![stbl[0].clone(), stbl[1].clone()];
        with_dup.extend_from_slice(extra);
        with_dup.extend_from_slice(&stbl[2..]);
        file_with(&[video_trak(&with_dup, 1000, 30)])
    };
    for dup in [
        vec![ctts_v1(&[(1, -5), (2, 0)]), ctts_v1(&[(3, 7)])],
        vec![stss(&[1]), stss(&[1])],
    ] {
        match Mp4Reader::from_vec(build(&dup)) {
            Err(Mp4Error::Malformed { reason, .. }) => {
                assert!(reason.contains("重复出现"), "reason={reason}");
            }
            other => panic!("应为 Malformed，实为 {other:?}"),
        }
    }
}

/// 无逐样例字段的 trun：字节界（样例数 ≤ 文件长度 / n×缺省 size ≤
/// 文件长度）之后还有逐样例物化的内存界——单轨 fragment 样例总数
/// 超 2^26（≈6.7e7，每样例物化约 56 字节 ⇒ 最坏 ≈3.5GB）一律
/// Malformed（回归：字节界按文件字节计不按内存计，300MB~1GB 构造
/// 文件可触发 34~56GB 分配令 Vec::push 直接 abort 而非返回 Err）。
#[test]
fn trun_sample_count_bounded_by_materialization() {
    // 字节界须先满足（size 缺省 0 ⇒ n ≤ file_len）：padding 出
    // > 2^26 字节的文件，再声明 2^26+1 样例——命中物化上限。
    let n: u32 = (1 << 26) + 1;
    let mut moof_p_trun = Vec::new();
    push(&mut moof_p_trun, &be_u32(0x0000_0001)); // v0 + data-offset 标志
    push(&mut moof_p_trun, &be_u32(n));
    push(&mut moof_p_trun, &be_u32(0)); // data_offset
    let moof = wrap(
        "moof",
        &[wrap(
            "traf",
            &[
                r#box(
                    "tfhd",
                    &[be_u32(0x0002_0000).to_vec(), be_u32(1).to_vec()].concat(),
                ),
                r#box("trun", &moof_p_trun),
            ],
        )],
    );
    let mut f = Vec::new();
    push(&mut f, &moof);
    let pad_len = n as usize + 64 - f.len();
    f.resize(f.len() + pad_len, 0);
    assert!(f.len() > n as usize, "字节界须被满足");
    let t0 = std::time::Instant::now();
    match Mp4Reader::from_vec(f) {
        Err(Mp4Error::Malformed { reason, .. }) => {
            assert!(reason.contains("超上限"), "reason={reason}");
        }
        other => panic!("应为 Malformed，实为 {other:?}"),
    }
    // 拒绝发生在物化之前（不预分配数十 GB）。
    assert!(
        t0.elapsed().as_millis() < 500,
        "应快速拒绝，实耗 {:?}",
        t0.elapsed()
    );
}

//! 测试辅助：手工构造 MP4 字节，零外部文件零网络。
// 各测试二进制只用其中一部分构造器。
#![allow(dead_code)]

/// 黄金路径 mdat 载荷：16 字节 0xA0..0xAF。
pub const MDAT: [u8; 16] = [
    0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xAB, 0xAC, 0xAD, 0xAE, 0xAF,
];

/// avcC 载荷（AVCDecoderConfigurationRecord 本体，内容任意但定长便于断言）。
pub const AVCC: [u8; 14] = [
    0x01, 0x64, 0x00, 0x1E, 0xFF, 0xE1, 0x00, 0x08, 0x1A, 0x2B, 0x3C, 0x4D, 0x5E, 0x6F,
];

/// esds 载荷（含 4 字节版本/标志；内容任意但定长便于断言）。
pub const ESDS: [u8; 29] = [
    0x00, 0x00, 0x00, 0x00, // version/flags
    0x03, 0x19, 0x00, 0x01, 0x00, // ES_Descriptor 头
    0x04, 0x11, 0x40, 0x15, // DecoderConfigDescriptor 头
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // avgBitrate 等
    0x05, 0x02, 0x10, 0x88, // DecSpecificInfo
    0x06, 0x01, 0x02, // SLConfigDescriptor
];

pub fn be_u16(v: u16) -> [u8; 2] {
    v.to_be_bytes()
}

pub fn be_u32(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

pub fn be_u64(v: u64) -> [u8; 8] {
    v.to_be_bytes()
}

pub fn push(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(bytes);
}

fn kind4(kind: &str) -> [u8; 4] {
    assert_eq!(kind.len(), 4, "box 类型须为四字符：{kind}");
    kind.as_bytes().try_into().unwrap()
}

/// 拼 8 字节头（size u32 + type）+ 载荷。
pub fn r#box(kind: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    push(&mut out, &be_u32(8 + payload.len() as u32));
    push(&mut out, &kind4(kind));
    push(&mut out, payload);
    out
}

/// largesize 版本：size==1 + 8 字节真实大小（造 64 位大 box 用）。
pub fn box64(kind: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + payload.len());
    push(&mut out, &be_u32(1));
    push(&mut out, &kind4(kind));
    push(&mut out, &be_u64(16 + payload.len() as u64));
    push(&mut out, payload);
    out
}

/// size==0 版本：延伸到父容器末（顶层末 box 即到 EOF）。
pub fn box_to_eof(kind: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    push(&mut out, &be_u32(0));
    push(&mut out, &kind4(kind));
    push(&mut out, payload);
    out
}

/// 容器 box：拼接子 box 后整体加 8 字节头。
pub fn wrap(kind: &str, children: &[Vec<u8>]) -> Vec<u8> {
    let mut payload = Vec::new();
    for c in children {
        push(&mut payload, c);
    }
    r#box(kind, &payload)
}

pub fn ftyp() -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, b"isom");
    push(&mut p, &be_u32(0x200));
    push(&mut p, b"isomiso2avc1mp4a");
    r#box("ftyp", &p)
}

/// mvhd v0（timescale=1000, duration=30, next_track_ID=2）。
pub fn mvhd() -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be_u32(0)); // version 0 + flags
    push(&mut p, &be_u32(0)); // creation_time
    push(&mut p, &be_u32(0)); // modification_time
    push(&mut p, &be_u32(1000)); // timescale
    push(&mut p, &be_u32(30)); // duration
    push(&mut p, &be_u32(0x0001_0000)); // rate
    push(&mut p, &be_u16(0x0100)); // volume
    push(&mut p, &be_u16(0)); // reserved
    push(&mut p, &be_u32(0));
    push(&mut p, &be_u32(0)); // reserved
    for v in [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        push(&mut p, &be_u32(v)); // matrix
    }
    for _ in 0..6 {
        push(&mut p, &be_u32(0)); // pre_defined
    }
    push(&mut p, &be_u32(2)); // next_track_ID
    r#box("mvhd", &p)
}

/// tkhd v0（flags=3：enabled|in_movie）。
pub fn tkhd(track_id: u32, duration: u32) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be_u32(0x0000_0003)); // version 0 + flags
    push(&mut p, &be_u32(0));
    push(&mut p, &be_u32(0)); // creation/modification
    push(&mut p, &be_u32(track_id));
    push(&mut p, &be_u32(0)); // reserved
    push(&mut p, &be_u32(duration));
    push(&mut p, &be_u32(0));
    push(&mut p, &be_u32(0)); // reserved
    push(&mut p, &be_u16(0)); // layer
    push(&mut p, &be_u16(0)); // alternate_group
    push(&mut p, &be_u16(0)); // volume
    push(&mut p, &be_u16(0)); // reserved
    for v in [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        push(&mut p, &be_u32(v)); // matrix
    }
    push(&mut p, &be_u32(0)); // width 16.16（读取端不用）
    push(&mut p, &be_u32(0)); // height 16.16
    r#box("tkhd", &p)
}

/// mdhd v0（language=und）。
pub fn mdhd(timescale: u32, duration: u32) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be_u32(0)); // version 0 + flags
    push(&mut p, &be_u32(0));
    push(&mut p, &be_u32(0)); // creation/modification
    push(&mut p, &be_u32(timescale));
    push(&mut p, &be_u32(duration));
    push(&mut p, &be_u16(0x55C4)); // language
    push(&mut p, &be_u16(0)); // pre_defined
    r#box("mdhd", &p)
}

pub fn hdlr(handler: &str) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be_u32(0)); // version + flags
    push(&mut p, &kind4(handler));
    push(&mut p, &[0; 12]); // reserved
    push(&mut p, b"Handler\0");
    r#box("hdlr", &p)
}

/// avc1 visual entry 固定字段（78 字节）。
fn visual_entry_fixed(w: u16, h: u16) -> Vec<u8> {
    let mut e = Vec::with_capacity(78);
    push(&mut e, &[0; 6]); // reserved
    push(&mut e, &be_u16(1)); // data_reference_index
    push(&mut e, &be_u16(0)); // pre_defined
    push(&mut e, &be_u16(0)); // reserved
    push(&mut e, &[0; 12]); // pre_defined[3]
    push(&mut e, &be_u16(w)); // width
    push(&mut e, &be_u16(h)); // height
    push(&mut e, &be_u32(0x0048_0000)); // horizresolution 72dpi
    push(&mut e, &be_u32(0x0048_0000)); // vertresolution
    push(&mut e, &be_u32(0)); // reserved
    push(&mut e, &be_u16(1)); // frame_count
    push(&mut e, &be_u32(0));
    push(&mut e, &[0; 28]); // compressorname 32 字节
    push(&mut e, &be_u16(24)); // depth
    push(&mut e, &be_u16(0xFFFF)); // pre_defined = -1
    assert_eq!(e.len(), 78);
    e
}

/// avc1 visual entry + 任意子 box 序列（第一个通常是 avcC；测试构造
/// 兄弟 box（pasp/btrt/…）或畸形声明 size 的子 box 用）。
/// entry 的 size 字段覆盖固定字段 + 全部子 box 的实际字节。
pub fn stsd_video_raw(w: u16, h: u16, children: &[Vec<u8>]) -> Vec<u8> {
    let fixed = visual_entry_fixed(w, h);
    let mut tail = 78usize;
    for c in children {
        tail += c.len();
    }
    let mut entry = Vec::new();
    push(&mut entry, &be_u32(8 + tail as u32));
    push(&mut entry, b"avc1");
    push(&mut entry, &fixed);
    for c in children {
        push(&mut entry, c);
    }

    let mut p = Vec::new();
    push(&mut p, &be_u32(0)); // version + flags
    push(&mut p, &be_u32(1)); // entry_count
    push(&mut p, &entry);
    r#box("stsd", &p)
}

/// avc1 visual entry（固定字段 78 字节）+ avcC 子 box。
/// entry 的 size 字段须覆盖固定字段 + 子 box（子 box 在 entry 内部）。
pub fn stsd_video(w: u16, h: u16, avcc: &[u8]) -> Vec<u8> {
    stsd_video_raw(w, h, &[r#box("avcC", avcc)])
}

/// mp4a audio entry（固定字段 20 字节，samplerate 16.16）+ esds 子 box。
pub fn stsd_audio(ch: u16, rate_hz: u32, esds: &[u8]) -> Vec<u8> {
    let mut e = Vec::with_capacity(20);
    push(&mut e, &[0; 6]); // reserved
    push(&mut e, &be_u16(1)); // data_reference_index
    push(&mut e, &be_u16(ch)); // channelcount
    push(&mut e, &be_u16(16)); // samplesize
    push(&mut e, &be_u16(0)); // pre_defined
    push(&mut e, &be_u16(0)); // reserved
    push(&mut e, &be_u32(rate_hz << 16)); // samplerate 16.16
    assert_eq!(e.len(), 20);

    let mut entry = Vec::new();
    let esds_box = r#box("esds", esds);
    push(&mut entry, &be_u32(8 + 20 + esds_box.len() as u32));
    push(&mut entry, b"mp4a");
    push(&mut entry, &e);
    push(&mut entry, &esds_box);

    let mut p = Vec::new();
    push(&mut p, &be_u32(0)); // version + flags
    push(&mut p, &be_u32(1)); // entry_count
    push(&mut p, &entry);
    r#box("stsd", &p)
}

/// stsd entry_count=2（错误用例：多 sample description）。
pub fn stsd_two_entries(w: u16, h: u16, avcc: &[u8]) -> Vec<u8> {
    let mut e = vec![0u8; 78];
    e[24..26].copy_from_slice(&be_u16(w));
    e[26..28].copy_from_slice(&be_u16(h));
    let mut entry = Vec::new();
    let avcc_box = r#box("avcC", avcc);
    push(&mut entry, &be_u32(8 + 78 + avcc_box.len() as u32));
    push(&mut entry, b"avc1");
    push(&mut entry, &e);
    push(&mut entry, &avcc_box);
    let mut p = Vec::new();
    push(&mut p, &be_u32(0));
    push(&mut p, &be_u32(2)); // entry_count = 2
    push(&mut p, &entry);
    push(&mut p, &entry);
    r#box("stsd", &p)
}

pub fn stts(runs: &[(u32, u32)]) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be_u32(0));
    push(&mut p, &be_u32(runs.len() as u32));
    for &(count, delta) in runs {
        push(&mut p, &be_u32(count));
        push(&mut p, &be_u32(delta));
    }
    r#box("stts", &p)
}

/// ctts v1（有符号偏移，负值合法）。
pub fn ctts_v1(runs: &[(u32, i32)]) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be_u32(1 << 24)); // version 1
    push(&mut p, &be_u32(runs.len() as u32));
    for &(count, offset) in runs {
        push(&mut p, &be_u32(count));
        push(&mut p, &(offset as u32).to_be_bytes());
    }
    r#box("ctts", &p)
}

pub fn stss(samples: &[u32]) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be_u32(0));
    push(&mut p, &be_u32(samples.len() as u32));
    for &s in samples {
        push(&mut p, &be_u32(s)); // 1-based 样号
    }
    r#box("stss", &p)
}

pub fn stsc(runs: &[(u32, u32, u32)]) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be_u32(0));
    push(&mut p, &be_u32(runs.len() as u32));
    for &(first_chunk, spc, desc) in runs {
        push(&mut p, &be_u32(first_chunk));
        push(&mut p, &be_u32(spc));
        push(&mut p, &be_u32(desc));
    }
    r#box("stsc", &p)
}

/// stsz 变长：sample_size=0 + 逐样例大小。
pub fn stsz_var(sizes: &[u32]) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be_u32(0));
    push(&mut p, &be_u32(0)); // sample_size = 0
    push(&mut p, &be_u32(sizes.len() as u32));
    for &s in sizes {
        push(&mut p, &be_u32(s));
    }
    r#box("stsz", &p)
}

/// stsz 定长：sample_size!=0。
pub fn stsz_fixed(size: u32, count: u32) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be_u32(0));
    push(&mut p, &be_u32(size));
    push(&mut p, &be_u32(count));
    r#box("stsz", &p)
}

/// stsz 截断（错误用例）：声明 count 但只给部分大小。
pub fn stsz_truncated(sizes: &[u32], declared: u32) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be_u32(0));
    push(&mut p, &be_u32(0));
    push(&mut p, &be_u32(declared));
    for &s in sizes {
        push(&mut p, &be_u32(s));
    }
    r#box("stsz", &p)
}

pub fn stco(offsets: &[u32]) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be_u32(0));
    push(&mut p, &be_u32(offsets.len() as u32));
    for &o in offsets {
        push(&mut p, &be_u32(o));
    }
    r#box("stco", &p)
}

pub fn co64(offsets: &[u64]) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be_u32(0));
    push(&mut p, &be_u32(offsets.len() as u32));
    for &o in offsets {
        push(&mut p, &be_u64(o));
    }
    r#box("co64", &p)
}

/// 标准视频轨 stbl 七表（黄金路径布局）。
/// stts=[(3,10)]，ctts v1=[(1,-5),(2,0)]，stss=[1]，
/// stsc=[(1,2,1),(2,1,1)]，stsz 变长 [4,5,6]，stco/co64 二选一。
pub fn golden_stbl(chunk_offsets: &[u64], use_co64: bool) -> Vec<Vec<u8>> {
    let chunks = if use_co64 {
        co64(chunk_offsets)
    } else {
        stco(&chunk_offsets.iter().map(|&o| o as u32).collect::<Vec<_>>())
    };
    vec![
        stsd_video(64, 96, &AVCC),
        stts(&[(3, 10)]),
        ctts_v1(&[(1, -5), (2, 0)]),
        stss(&[1]),
        stsc(&[(1, 2, 1), (2, 1, 1)]),
        stsz_var(&[4, 5, 6]),
        chunks,
    ]
}

/// 视频轨：tkhd(track_id=7) + mdhd + hdlr=vide + minf(stbl(children))。
pub fn video_trak(stbl_children: &[Vec<u8>], timescale: u32, duration: u32) -> Vec<u8> {
    wrap(
        "trak",
        &[
            tkhd(7, duration),
            wrap(
                "mdia",
                &[
                    mdhd(timescale, duration),
                    hdlr("vide"),
                    wrap("minf", &[wrap("stbl", stbl_children)]),
                ],
            ),
        ],
    )
}

/// 音频轨：tkhd(track_id=3) + mdhd + hdlr=soun + minf(stbl(children))。
pub fn audio_trak(stbl_children: &[Vec<u8>], timescale: u32, duration: u32) -> Vec<u8> {
    wrap(
        "trak",
        &[
            tkhd(3, duration),
            wrap(
                "mdia",
                &[
                    mdhd(timescale, duration),
                    hdlr("soun"),
                    wrap("minf", &[wrap("stbl", stbl_children)]),
                ],
            ),
        ],
    )
}

pub fn moov(traks: &[Vec<u8>]) -> Vec<u8> {
    let mut children = vec![mvhd()];
    children.extend_from_slice(traks);
    wrap("moov", &children)
}

/// ftyp + moov + mdat(MDAT)。
pub fn file_with(traks: &[Vec<u8>]) -> Vec<u8> {
    let mut f = Vec::new();
    push(&mut f, &ftyp());
    push(&mut f, &moov(traks));
    push(&mut f, &r#box("mdat", &MDAT));
    f
}

/// 黄金路径文件（占位偏移先探测布局再回填，偏移定长不影响布局）。
pub fn golden_file(chunk_offsets: &[u64], use_co64: bool) -> Vec<u8> {
    file_with(&[video_trak(&golden_stbl(chunk_offsets, use_co64), 1000, 30)])
}

/// 黄金路径的两个 chunk 偏移（mdat 载荷起点、+9）。
pub fn golden_offsets(file: &[u8]) -> (u64, u64) {
    let chunk1 = (file.len() - MDAT.len()) as u64;
    let chunk2 = chunk1 + 9; // chunk1 含样例 0/1，大小 4+5
    (chunk1, chunk2)
}

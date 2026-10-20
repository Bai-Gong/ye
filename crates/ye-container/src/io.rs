//! IO 组合 trait：可作 trait 对象的 `Read + Seek` / `Write + Seek` 组合。

use std::io::{Read, Seek, Write};

/// 读端组合 trait：[`Read`] + [`Seek`] + `Send` + `Sync`。
///
/// ## 为什么是组合 trait 而不是 type alias
///
/// `type ReadSeek = dyn Read + Seek (+ Send + Sync)` 形式违反 trait 对象
/// 「只允许一个非 auto trait」规则——实测 rustc 1.99.0 对该行报
/// E0225（`only auto traits can be used as additional traits in a trait
/// object`，对裸 `dyn Read + Seek` 同样成立）。组合 trait + 下方空白实现
/// 实测编译通过，且 `Box<dyn ReadSeek>` 满足 `Send + Sync`。
///
/// ## 空白实现（blanket impl）
///
/// 一切 `Read + Seek + Send + Sync` 类型（含 [`std::fs::File`]、
/// [`std::io::Cursor`]）自动满足本 trait；具体 demuxer/muxer 持有
/// `Box<dyn ReadSeek>` / `Box<dyn WriteSeek>` 字段即满足 trait 的
/// `Send + Sync` 上界（连带规定见全库约定）。
pub trait ReadSeek: Read + Seek + Send + Sync {}

impl<T: Read + Seek + Send + Sync> ReadSeek for T {}

/// 写端组合 trait：[`Write`] + [`Seek`] + `Send` + `Sync`（构造理由同
/// [`ReadSeek`]，E0225 规避一致）。
pub trait WriteSeek: Write + Seek + Send + Sync {}

impl<T: Write + Seek + Send + Sync> WriteSeek for T {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// 编译期断言：trait 对象形式满足 Send + Sync（type alias 形式
    /// 无法通过编译——E0225——本测试的存在即为凭）。
    #[test]
    fn trait_objects_are_send_sync() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn ReadSeek>();
        assert_send_sync::<Box<dyn ReadSeek>>();
        assert_send_sync::<dyn WriteSeek>();
        assert_send_sync::<Box<dyn WriteSeek>>();
    }

    /// 空白实现生效：std::fs::File 与 Cursor<Vec<u8>> 均满足组合 trait。
    #[test]
    fn blanket_impl_admits_std_types() {
        fn generic_read<T: ReadSeek + ?Sized>(_t: &mut T) {}
        fn generic_write<T: WriteSeek + ?Sized>(_t: &mut T) {}

        let path = std::env::temp_dir().join("ye-container-io-blanket-impl.tmp");
        let mut file = std::fs::File::create(&path).expect("create temp file");
        generic_read(&mut file);
        drop(file);
        let _ = std::fs::remove_file(&path);

        let mut cursor = Cursor::new(Vec::<u8>::new());
        generic_read(&mut cursor);
        generic_write(&mut cursor);
    }
}

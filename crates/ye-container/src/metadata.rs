//! 容器元数据字典。

use std::collections::BTreeMap;

/// 元数据字典：容器 tag（title/language 等）统一走这里，不逐字段建模。
///
/// ## 不变式与语义
///
/// - 键值按字节串原样保留，**不做任何规范化**（大小写折叠、语言码转换、
///   编码清洗等归具体容器实现负责）；
/// - 迭代顺序为键的字节序（[`BTreeMap`]），确定且可复现；
/// - 空字典与 [`Default`] 等价。
///
/// ## derive 清单
///
/// `Debug, Clone, PartialEq, Eq` + `Default`（纯数据载体，含 String 字段
/// 加 PartialEq/Eq；无不变式、无 Copy）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Metadata(BTreeMap<String, String>);

impl Metadata {
    /// 空字典。
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// 取键对应的值。
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(|v| v.as_str())
    }

    /// 插入/覆盖：返回被覆盖的旧值（首次插入返回 `None`）。
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) -> Option<String> {
        self.0.insert(key.into(), value.into())
    }

    /// 移除键：返回被移除的值（键不存在返回 `None`）。
    pub fn remove(&mut self, key: &str) -> Option<String> {
        self.0.remove(key)
    }

    /// 条目数。
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// 按键的字节序迭代 `(key, value)`。
    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.0.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_overwrite_returns_old_value() {
        let mut m = Metadata::new();
        assert_eq!(m.insert("title", "first"), None);
        assert_eq!(m.insert("title", "second"), Some("first".to_string()));
        assert_eq!(m.get("title"), Some("second"));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn get_remove_len_iter() {
        let mut m = Metadata::new();
        assert!(m.is_empty());
        assert_eq!(m.get("missing"), None);
        assert_eq!(m.remove("missing"), None);

        m.insert("language", "und");
        m.insert("title", "demo");
        assert_eq!(m.len(), 2);

        // BTreeMap 键序：language < title
        let pairs: Vec<(&String, &String)> = m.iter().collect();
        assert_eq!(
            pairs,
            vec![
                (&"language".to_string(), &"und".to_string()),
                (&"title".to_string(), &"demo".to_string())
            ]
        );

        assert_eq!(m.remove("language"), Some("und".to_string()));
        assert_eq!(m.get("language"), None);
        assert_eq!(m.len(), 1);
        assert!(!m.is_empty());
    }

    /// 键值按字节串原样保留：大小写不同视为不同键，值不改写。
    #[test]
    fn keys_values_kept_verbatim() {
        let mut m = Metadata::new();
        m.insert("TiTle", "  Raw Value \u{e9} ");
        m.insert("title", "lower");
        assert_eq!(m.len(), 2);
        assert_eq!(m.get("TiTle"), Some("  Raw Value \u{e9} "));
        assert_eq!(m.get("title"), Some("lower"));
        // 克隆与相等按内容比较
        let cloned = m.clone();
        assert_eq!(m, cloned);
        assert_eq!(Metadata::new(), Metadata::default());
    }
}

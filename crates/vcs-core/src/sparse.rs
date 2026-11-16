//! 稀疏检出规则。
//!
//! 规则是「要检出的路径前缀」白名单，相对仓库根目录、统一用 `/` 分隔：
//! `src/` 表示 `src` 目录整棵子树，`docs/api.md` 也可以作为精确的单文件规则。
//! **空规则 = 全量检出**，这是所有老工作副本的默认状态，读不到 meta 时不能凭空变稀疏。
//!
//! 规则持久化在 `wc.db` 的 `meta` 表里（JSON 字符串数组），格式版本不变；
//! 之所以不新开表，是为了让老副本读到新代码时自然降级为全量，不需要数据迁移。

use serde::{Deserialize, Serialize};

use crate::{normalize_path, Error, ErrorCode, Result};

/// `wc.db` 的 `meta` 键，值是 `SparseRules::to_json()` 的结果。
pub const SPARSE_META_KEY: &str = "sparse_rules";

/// 一组已规范化、已去重的稀疏路径前缀。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SparseRules {
    /// 空表示全量检出。
    pub paths: Vec<String>,
}

impl SparseRules {
    /// 全量检出（不稀疏）。
    pub fn all() -> Self {
        Self::default()
    }

    /// 从用户给的路径构造规则：防目录穿越、去重、排序，并去掉被更短前缀覆盖的冗余项。
    pub fn from_paths<I, S>(paths: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut out = Vec::new();
        for raw in paths {
            let trimmed = raw.as_ref().trim();
            if trimmed.is_empty() {
                continue;
            }
            let normalized = normalize_path(trimmed)?;
            if !normalized.is_empty() {
                out.push(normalized);
            }
        }
        out.sort();
        out.dedup();
        // `src` 已经覆盖 `src/api`，留着只会让规则列表越改越长。
        let mut kept = Vec::with_capacity(out.len());
        for candidate in &out {
            if !out
                .iter()
                .any(|other| other.len() < candidate.len() && under(other, candidate))
            {
                kept.push(candidate.clone());
            }
        }
        Ok(Self { paths: kept })
    }

    /// 读 `meta` 里持久化的规则；空值 / 损坏值按全量处理，绝不让旧副本突然「少文件」。
    pub fn from_json(text: &str) -> Result<Self> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(Self::all());
        }
        let raw: Vec<String> = serde_json::from_str(trimmed).map_err(|e| {
            Error::new(ErrorCode::InvalidArgument, format!("稀疏规则格式错误：{e}"))
        })?;
        Self::from_paths(raw)
    }

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(&self.paths)
            .map_err(|e| Error::new(ErrorCode::IoError, format!("序列化稀疏规则失败：{e}")))
    }

    pub fn is_sparse(&self) -> bool {
        !self.paths.is_empty()
    }

    /// 某路径是否落在规则内。全量规则恒为 true。
    pub fn includes(&self, path: &str) -> bool {
        if self.paths.is_empty() {
            return true;
        }
        let path = path.trim_start_matches('/');
        self.paths.iter().any(|prefix| under(prefix, path))
    }

    /// 新规则相对当前规则「多出来」的前缀：调整规则时只需要补拉这些路径的对象。
    /// 旧规则是全量时没有需要补的路径。
    pub fn added_prefixes(&self, next: &SparseRules) -> Vec<String> {
        if !self.is_sparse() {
            return Vec::new();
        }
        next.paths
            .iter()
            .filter(|p| !self.paths.iter().any(|q| under(q, p)))
            .cloned()
            .collect()
    }
}

/// `path == prefix` 或 `path` 位于 `prefix` 目录之下，边界必须是一个 `/`。
/// `2026100/a.txt` 不会被 `202610` 误命中。
pub fn under(prefix: &str, path: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    if !path.starts_with(prefix) {
        return false;
    }
    path.len() == prefix.len() || path.as_bytes()[prefix.len()] == b'/'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_dedupes_and_prunes_redundant_prefixes() {
        let rules = SparseRules::from_paths([
            "/src/api/",
            "src",
            "docs\\api.md",
            "  ",
            "src/api",
            "docs/api.md",
        ])
        .unwrap();
        assert_eq!(rules.paths, vec!["docs/api.md", "src"]);
    }

    #[test]
    fn rejects_path_traversal() {
        assert!(SparseRules::from_paths(["../etc"]).is_err());
    }

    #[test]
    fn empty_rules_mean_full_checkout() {
        let rules = SparseRules::all();
        assert!(!rules.is_sparse());
        assert!(rules.includes("anything/at/all.txt"));
        assert_eq!(rules.to_json().unwrap(), "[]");
        assert_eq!(SparseRules::from_json("").unwrap(), rules);
    }

    #[test]
    fn matches_prefix_boundaries_and_exact_files() {
        let rules = SparseRules::from_paths(["src", "README.md", "202610"]).unwrap();
        assert!(rules.includes("src/main.rs"));
        assert!(rules.includes("src/api/v1.rs"));
        assert!(rules.includes("src"));
        assert!(rules.includes("README.md"));
        assert!(rules.includes("202610/a.txt"));
        assert!(!rules.includes("src2/main.rs"));
        assert!(!rules.includes("2026100/a.txt"));
        assert!(!rules.includes("docs/README.md"));
    }

    #[test]
    fn added_prefixes_only_cover_new_area() {
        let old = SparseRules::from_paths(["src"]).unwrap();
        let next = SparseRules::from_paths(["src/api", "docs", "src"]).unwrap();
        assert_eq!(old.added_prefixes(&next), vec!["docs"]);
        assert!(SparseRules::all().added_prefixes(&next).is_empty());
    }

    #[test]
    fn json_round_trip_keeps_rules() {
        let rules = SparseRules::from_paths(["src", "docs/api.md"]).unwrap();
        assert_eq!(
            SparseRules::from_json(&rules.to_json().unwrap()).unwrap(),
            rules
        );
        assert!(SparseRules::from_json("not json").is_err());
    }
}

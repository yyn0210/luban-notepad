//! Include/exclude glob filters shared by workspace search and the index.

use std::path::Path;

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};

use crate::error::{Error, Result};

/// A compiled include/exclude filter.
///
/// Patterns are separated by commas, semicolons or whitespace. `*`, `?` and
/// `**` are supported. A pattern without a separator also matches a file name
/// at any depth, so `*.rs` includes `src/main.rs`. Excludes win over includes.
#[derive(Clone, Debug, Default)]
pub struct GlobFilter {
    include: Option<PatternSet>,
    exclude: Option<PatternSet>,
}

#[derive(Clone, Debug)]
struct PatternSet {
    full: GlobSet,
    /// Patterns without a separator also apply to the file name, so `*.rs`
    /// includes `src/main.rs`.
    basename: Option<GlobSet>,
}

impl PatternSet {
    fn matches(&self, relative: &str) -> bool {
        if self.full.is_match(relative) {
            return true;
        }
        match &self.basename {
            Some(set) => relative
                .rsplit('/')
                .next()
                .is_some_and(|name| set.is_match(name)),
            None => false,
        }
    }
}

impl GlobFilter {
    /// Compile the raw include and exclude strings. Empty input disables the
    /// corresponding side.
    pub fn new(include: &str, exclude: &str) -> Result<Self> {
        Ok(Self {
            include: compile(include)?,
            exclude: compile(exclude)?,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.include.is_none() && self.exclude.is_none()
    }

    /// Whether `relative` (using `/` separators) passes the filter.
    pub fn matches(&self, relative: &str) -> bool {
        let relative = relative.trim_start_matches("./");
        if self
            .exclude
            .as_ref()
            .is_some_and(|set| set.matches(relative))
        {
            return false;
        }
        match &self.include {
            Some(set) => set.matches(relative),
            None => true,
        }
    }

    /// Whether `path` passes the filter relative to `root`.
    pub fn matches_path(&self, root: &Path, path: &Path) -> bool {
        let relative = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        self.matches(&relative)
    }
}

fn compile(raw: &str) -> Result<Option<PatternSet>> {
    let mut full = GlobSetBuilder::new();
    let mut basename = GlobSetBuilder::new();
    let mut any = false;
    let mut any_basename = false;
    for pattern in split_patterns(raw) {
        full.add(build_glob(pattern)?);
        if !pattern.contains('/') && !pattern.contains('\\') {
            basename.add(build_glob(pattern)?);
            any_basename = true;
        }
        any = true;
    }
    if !any {
        return Ok(None);
    }
    let full = full
        .build()
        .map_err(|error| Error::Other(error.to_string()))?;
    let basename = if any_basename {
        Some(
            basename
                .build()
                .map_err(|error| Error::Other(error.to_string()))?,
        )
    } else {
        None
    };
    Ok(Some(PatternSet { full, basename }))
}

fn build_glob(pattern: &str) -> Result<Glob> {
    let mut builder = GlobBuilder::new(pattern);
    builder.literal_separator(true);
    #[cfg(windows)]
    builder.case_insensitive(true);
    builder
        .build()
        .map_err(|error| Error::Other(format!("invalid glob '{pattern}': {error}")))
}

/// Split a raw filter string into non-empty patterns.
pub fn split_patterns(raw: &str) -> impl Iterator<Item = &str> {
    raw.split([',', ';', ' ', '\t', '\n', '\r'])
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
}

/// Compile include/exclude patterns for a workspace index.
pub fn compile_optional(include: Option<&str>, exclude: Option<&str>) -> Result<GlobFilter> {
    GlobFilter::new(include.unwrap_or(""), exclude.unwrap_or(""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_filter_matches_everything() {
        let filter = GlobFilter::new("", "").unwrap();
        assert!(filter.is_empty());
        assert!(filter.matches("src/main.rs"));
    }

    #[test]
    fn include_limits_and_excludes_win() {
        let filter = GlobFilter::new("*.rs, *.toml", "src/generated.rs").unwrap();
        assert!(filter.matches("src/main.rs"));
        assert!(filter.matches("Cargo.toml"));
        assert!(!filter.matches("README.md"));
        assert!(!filter.matches("src/generated.rs"));
        assert!(!filter.matches("src/generated.rs"));
    }

    #[test]
    fn recursive_and_question_mark_patterns() {
        let filter = GlobFilter::new("**/tests/*.rs", "").unwrap();
        assert!(filter.matches("crates/a/tests/one.rs"));
        assert!(filter.matches("crates/a/src/tests/one.rs"));
        assert!(!filter.matches("crates/a/src/one.rs"));
        let filter = GlobFilter::new("read?e.md", "").unwrap();
        assert!(filter.matches("readme.md"));
        assert_eq!(filter.matches("readme.MD"), cfg!(windows));
    }

    #[test]
    fn separators_split_patterns() {
        let parts: Vec<_> = split_patterns(" a,b;c  d ").collect();
        assert_eq!(parts, ["a", "b", "c", "d"]);
    }

    #[test]
    fn invalid_patterns_are_errors() {
        assert!(GlobFilter::new("[abc", "").is_err());
    }

    #[test]
    fn matches_path_uses_forward_slashes() {
        let filter = GlobFilter::new("src/*.rs", "").unwrap();
        assert!(filter.matches_path(Path::new("/w"), Path::new("/w/src/a.rs")));
        assert!(!filter.matches_path(Path::new("/w"), Path::new("/other/src/a.rs")));
    }
}

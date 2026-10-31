//! User `.sublime-syntax` packages merged into the bundled syntax set.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};

use syntect::parsing::{SyntaxDefinition, SyntaxReference, SyntaxSet};
use two_face::syntax::extra_newlines;
use walkdir::WalkDir;

pub const MAX_SYNTAX_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_SYNTAX_FILES: usize = 1_000;

static ACTIVE_SET: LazyLock<RwLock<Arc<SyntaxSet>>> =
    LazyLock::new(|| RwLock::new(Arc::new(extra_newlines())));

/// The syntax set currently used by the editor and exports.
pub fn active_set() -> Arc<SyntaxSet> {
    ACTIVE_SET
        .read()
        .map(|set| set.clone())
        .unwrap_or_else(|_| Arc::new(extra_newlines()))
}

/// Replace the active syntax set after a package reload.
pub fn set_active_set(set: SyntaxSet) -> Arc<SyntaxSet> {
    let set = Arc::new(set);
    if let Ok(mut active) = ACTIVE_SET.write() {
        *active = set.clone();
    }
    set
}

/// Restore the bundled syntax set (tests and recovery paths).
pub fn reset_active_set() {
    let set = Arc::new(extra_newlines());
    if let Ok(mut active) = ACTIVE_SET.write() {
        *active = set;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserSyntaxInfo {
    pub name: String,
    pub extensions: Vec<String>,
    pub first_line_match: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SyntaxBuild {
    pub set: SyntaxSet,
    pub languages: Vec<UserSyntaxInfo>,
    pub errors: Vec<String>,
}

/// Collect `.sublime-syntax` files below a package directory.
pub fn collect_syntax_files(dir: &Path) -> (Vec<PathBuf>, Vec<String>) {
    let mut files = Vec::new();
    let mut errors = Vec::new();
    if !dir.is_dir() {
        return (files, errors);
    }
    for entry in WalkDir::new(dir).follow_links(false).sort_by_file_name() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                errors.push(format!("cannot walk syntax packages: {error}"));
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        if !entry
            .file_name()
            .to_string_lossy()
            .to_ascii_lowercase()
            .ends_with(".sublime-syntax")
        {
            continue;
        }
        if files.len() >= MAX_SYNTAX_FILES {
            errors.push(format!("syntax file limit of {MAX_SYNTAX_FILES} reached"));
            break;
        }
        match fs::metadata(entry.path()) {
            Ok(metadata) if metadata.len() > MAX_SYNTAX_BYTES => {
                errors.push(format!(
                    "{} is larger than {MAX_SYNTAX_BYTES} bytes",
                    entry.path().display()
                ));
            }
            Ok(_) => files.push(entry.path().to_path_buf()),
            Err(error) => errors.push(format!("cannot read {}: {error}", entry.path().display())),
        }
    }
    files.sort();
    (files, errors)
}

/// Build the bundled syntax set plus the user syntax files.
pub fn build(syntax_paths: &[PathBuf]) -> SyntaxBuild {
    let mut builder = extra_newlines().into_builder();
    let mut languages = Vec::new();
    let mut errors = Vec::new();
    for path in syntax_paths {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                errors.push(format!("cannot read {}: {error}", path.display()));
                continue;
            }
        };
        match SyntaxDefinition::load_from_str(&text, true, None) {
            Ok(definition) => {
                languages.push(UserSyntaxInfo {
                    name: definition.name.clone(),
                    extensions: definition.file_extensions.clone(),
                    first_line_match: definition.first_line_match.clone(),
                });
                builder.add(definition);
            }
            Err(error) => errors.push(format!("{}: {error}", path.display())),
        }
    }
    SyntaxBuild {
        set: builder.build(),
        languages,
        errors,
    }
}

/// Build from a package directory and return both the set and the collected
/// file/error information.
pub fn build_from_dir(dir: &Path) -> SyntaxBuild {
    let (files, mut errors) = collect_syntax_files(dir);
    let build = build(&files);
    errors.extend(build.errors);
    SyntaxBuild {
        set: build.set,
        languages: build.languages,
        errors,
    }
}

/// Find a syntax by token or display name.
pub fn find_syntax<'a>(
    set: &'a SyntaxSet,
    language: &str,
    token: &str,
) -> Option<&'a SyntaxReference> {
    set.find_syntax_by_token(token)
        .or_else(|| set.find_syntax_by_name(language))
        .or_else(|| {
            let lower = token.to_ascii_lowercase();
            set.syntaxes()
                .iter()
                .find(|syntax| syntax.name.eq_ignore_ascii_case(&lower))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYNTAX: &str = r#"%YAML 1.2
---
name: MyLang
file_extensions: [myl]
scope: source.myl
contexts:
  main:
    - match: '\b(if|else)\b'
      scope: keyword.control.myl
"#;

    #[test]
    fn loads_user_syntax_and_reports_languages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mylang.sublime-syntax");
        fs::write(&path, SYNTAX).unwrap();
        let build = build(&[path]);
        assert!(build.errors.is_empty(), "{:?}", build.errors);
        assert_eq!(build.languages.len(), 1);
        assert_eq!(build.languages[0].name, "MyLang");
        assert_eq!(build.languages[0].extensions, ["myl"]);
        let syntax = build.set.find_syntax_by_extension("myl").unwrap();
        assert_eq!(syntax.name, "MyLang");
    }

    #[test]
    fn isolates_invalid_syntax_files() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.sublime-syntax");
        let bad = dir.path().join("bad.sublime-syntax");
        fs::write(&good, SYNTAX).unwrap();
        fs::write(&bad, "not: [valid").unwrap();
        let build = build_from_dir(dir.path());
        assert_eq!(build.languages.len(), 1);
        assert_eq!(build.errors.len(), 1);
        assert!(build.set.find_syntax_by_extension("myl").is_some());
    }
}

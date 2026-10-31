//! User package directories for syntaxes, themes and snippets.

use std::fs;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::snippet::{self, SnippetSet};
use crate::user_syntax;

pub const MAX_THEME_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_THEMES: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackagePaths {
    pub root: PathBuf,
    pub syntaxes: PathBuf,
    pub themes: PathBuf,
    pub snippets: PathBuf,
}

impl PackagePaths {
    pub fn under(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            syntaxes: root.join("syntaxes"),
            themes: root.join("themes"),
            snippets: root.join("snippets"),
            root,
        }
    }

    pub fn system() -> Self {
        Self::under(crate::settings::config_dir().join("packages"))
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.syntaxes)?;
        fs::create_dir_all(&self.themes)?;
        fs::create_dir_all(&self.snippets)?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemePackage {
    /// Stable id stored in `settings.syntax_theme`.
    pub id: String,
    pub name: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct PackageReport {
    pub syntax_files: Vec<PathBuf>,
    pub themes: Vec<ThemePackage>,
    pub snippets: SnippetSet,
    pub errors: Vec<String>,
}

impl PackageReport {
    pub fn theme(&self, id: &str) -> Option<&ThemePackage> {
        self.themes.iter().find(|theme| theme.id == id)
    }
}

/// Scan the package directories. Missing directories are not errors.
pub fn load(paths: &PackagePaths) -> PackageReport {
    let mut report = PackageReport::default();
    let (syntax_files, syntax_errors) = user_syntax::collect_syntax_files(&paths.syntaxes);
    report.syntax_files = syntax_files;
    report.errors.extend(syntax_errors);
    report.snippets = snippet::load_snippets(&paths.snippets);
    report.errors.extend(
        report
            .snippets
            .errors
            .iter()
            .map(|error| format!("{}: {}", error.path.display(), error.message)),
    );
    report.themes = load_themes(&paths.themes, &mut report.errors);
    report
}

fn load_themes(dir: &Path, errors: &mut Vec<String>) -> Vec<ThemePackage> {
    let mut themes = Vec::new();
    if !dir.is_dir() {
        return themes;
    }
    let mut paths = Vec::new();
    for entry in WalkDir::new(dir).follow_links(false).sort_by_file_name() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                errors.push(format!("cannot walk theme packages: {error}"));
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
            .ends_with(".tmtheme")
        {
            continue;
        }
        if paths.len() >= MAX_THEMES {
            errors.push(format!("theme file limit of {MAX_THEMES} reached"));
            break;
        }
        paths.push(entry.path().to_path_buf());
    }
    paths.sort();
    for path in paths {
        match fs::metadata(&path) {
            Ok(metadata) if metadata.len() > MAX_THEME_BYTES => {
                errors.push(format!(
                    "{} is larger than {MAX_THEME_BYTES} bytes",
                    path.display()
                ));
                continue;
            }
            Err(error) => {
                errors.push(format!("cannot read {}: {error}", path.display()));
                continue;
            }
            _ => {}
        }
        match syntect::highlighting::ThemeSet::get_theme(&path) {
            Ok(theme) => {
                let relative = path
                    .strip_prefix(dir)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let name = theme
                    .name
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| {
                        path.file_stem()
                            .map(|stem| stem.to_string_lossy().to_string())
                            .unwrap_or_else(|| "Theme".to_string())
                    });
                themes.push(ThemePackage {
                    id: format!("pkg:{relative}:{name}"),
                    name,
                    path,
                });
            }
            Err(error) => errors.push(format!("{}: {error}", path.display())),
        }
    }
    themes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_package_directories() {
        let root = tempfile::tempdir().unwrap();
        let paths = PackagePaths::under(root.path().join("packages"));
        paths.ensure().unwrap();
        assert!(paths.syntaxes.is_dir());
        assert!(paths.themes.is_dir());
        assert!(paths.snippets.is_dir());
    }

    #[test]
    fn loads_a_valid_theme_package() {
        let root = tempfile::tempdir().unwrap();
        let paths = PackagePaths::under(root.path());
        paths.ensure().unwrap();
        fs::write(paths.themes.join("test.tmTheme"), r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>name</key><string>Test Theme</string>
  <key>settings</key>
  <array>
    <dict>
      <key>settings</key>
      <dict>
        <key>background</key><string>#ffffff</string>
        <key>foreground</key><string>#000000</string>
      </dict>
    </dict>
  </array>
</dict>
</plist>"#).unwrap();
        let report = load(&paths);
        assert_eq!(report.themes.len(), 1, "{:?}", report.errors);
        assert_eq!(report.themes[0].name, "Test Theme");
        assert_eq!(report.themes[0].id, "pkg:test.tmTheme:Test Theme");
    }

    #[test]
    fn loads_snippets_and_isolates_bad_themes() {
        let root = tempfile::tempdir().unwrap();
        let paths = PackagePaths::under(root.path());
        paths.ensure().unwrap();
        fs::write(
            paths.snippets.join("one.json"),
            r#"[{ "name": "One", "body": "hello" }]"#,
        )
        .unwrap();
        fs::write(paths.themes.join("bad.tmTheme"), "not a plist").unwrap();
        let report = load(&paths);
        assert_eq!(report.snippets.snippets.len(), 1);
        assert_eq!(report.themes.len(), 0);
        assert!(!report.errors.is_empty());
    }
}

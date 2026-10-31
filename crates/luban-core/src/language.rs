//! Language detection and editing metadata.
//!
//! Highlighting itself lives in the GUI crate (which embeds syntect's syntax
//! definitions). This module provides a deterministic extension map for the
//! status bar, the language menu, and comment toggling, including languages
//! that syntect does not ship with.

use std::path::Path;
use std::sync::{LazyLock, RwLock};

use fancy_regex::Regex as FancyRegex;

/// A language known to the editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LanguageInfo {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    pub filenames: &'static [&'static str],
    pub line_comment: &'static str,
    pub block_comment: Option<(&'static str, &'static str)>,
}

pub const PLAIN_TEXT: &str = "Plain Text";

/// A user-defined language derived from a `.sublime-syntax` package.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserLanguageSpec {
    pub name: String,
    pub extensions: Vec<String>,
    pub line_comment: Option<String>,
    pub block_comment: Option<(String, String)>,
    /// Optional regex matched against the first line of a file.
    pub first_line_match: Option<String>,
}

static USER_LANGUAGES: LazyLock<RwLock<Vec<&'static LanguageInfo>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));
static USER_FIRST_LINE: LazyLock<RwLock<Vec<(&'static str, FancyRegex)>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Replace the runtime user language registry.
///
/// The entries are leaked so callers can keep using `&'static LanguageInfo`
/// throughout the application. Reloads are rare and bounded by the package
/// directory size, so the small retained allocation is intentional.
pub fn set_user_languages(specs: Vec<UserLanguageSpec>) {
    let mut leaked = Vec::with_capacity(specs.len());
    let mut first_line = Vec::new();
    for spec in specs {
        if spec.name.trim().is_empty() {
            continue;
        }
        let pattern = spec.first_line_match.clone();
        let info = leak_language(spec);
        if let Some(pattern) = pattern {
            match FancyRegex::new(&pattern) {
                Ok(regex) => first_line.push((info.name, regex)),
                Err(error) => log::warn!(
                    "invalid first_line_match for user language '{}': {error}",
                    info.name
                ),
            }
        }
        leaked.push(info);
    }
    if let Ok(mut languages) = USER_LANGUAGES.write() {
        *languages = leaked;
    }
    if let Ok(mut patterns) = USER_FIRST_LINE.write() {
        *patterns = first_line;
    }
}

pub fn clear_user_languages() {
    if let Ok(mut languages) = USER_LANGUAGES.write() {
        languages.clear();
    }
    if let Ok(mut patterns) = USER_FIRST_LINE.write() {
        patterns.clear();
    }
}

/// Detect a user language from the first line of a document.
///
/// Built-in languages are extension/filename driven; first-line matching is a
/// fallback for files such as extensionless scripts.
pub fn detect_from_content(text: &str) -> Option<&'static str> {
    let first_line: String = text
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(4096)
        .collect();
    let patterns = USER_FIRST_LINE.read().ok()?;
    patterns
        .iter()
        .find(|(_, regex)| regex.is_match(&first_line).unwrap_or(false))
        .map(|(name, _)| *name)
}

pub fn user_languages() -> Vec<&'static LanguageInfo> {
    USER_LANGUAGES
        .read()
        .map(|languages| languages.clone())
        .unwrap_or_default()
}

fn leak_language(spec: UserLanguageSpec) -> &'static LanguageInfo {
    let name: &'static str = Box::leak(spec.name.into_boxed_str());
    let extensions: &'static [&'static str] = leak_strings(&spec.extensions);
    let line_comment: &'static str = spec
        .line_comment
        .map(|comment| Box::leak(comment.into_boxed_str()) as &'static str)
        .unwrap_or("");
    let block_comment = spec.block_comment.map(|(start, end)| {
        (
            Box::leak(start.into_boxed_str()) as &'static str,
            Box::leak(end.into_boxed_str()) as &'static str,
        )
    });
    Box::leak(Box::new(LanguageInfo {
        name,
        extensions,
        filenames: &[],
        line_comment,
        block_comment,
    }))
}

fn leak_strings(values: &[String]) -> &'static [&'static str] {
    let strings: Vec<&'static str> = values
        .iter()
        .map(|value| Box::leak(value.clone().into_boxed_str()) as &'static str)
        .collect();
    Box::leak(strings.into_boxed_slice())
}

const LANGUAGES: &[LanguageInfo] = &[
    LanguageInfo {
        name: "Assembly",
        extensions: &["asm", "s", "S"],
        filenames: &[],
        line_comment: ";",
        block_comment: None,
    },
    LanguageInfo {
        name: "Batch",
        extensions: &["bat", "cmd"],
        filenames: &[],
        line_comment: "REM ",
        block_comment: None,
    },
    LanguageInfo {
        name: "C",
        extensions: &["c", "h"],
        filenames: &[],
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
    },
    LanguageInfo {
        name: "C++",
        extensions: &["cpp", "cxx", "cc", "hpp", "hxx", "hh", "ino"],
        filenames: &[],
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
    },
    LanguageInfo {
        name: "C#",
        extensions: &["cs"],
        filenames: &[],
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
    },
    LanguageInfo {
        name: "CSS",
        extensions: &["css"],
        filenames: &[],
        line_comment: "/* ",
        block_comment: Some(("/*", "*/")),
    },
    LanguageInfo {
        name: "Clojure",
        extensions: &["clj", "cljs", "cljc", "edn"],
        filenames: &[],
        line_comment: ";",
        block_comment: None,
    },
    LanguageInfo {
        name: "CMake",
        extensions: &["cmake"],
        filenames: &["CMakeLists.txt"],
        line_comment: "#",
        block_comment: None,
    },
    LanguageInfo {
        name: "Dart",
        extensions: &["dart"],
        filenames: &[],
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
    },
    LanguageInfo {
        name: "Diff",
        extensions: &["diff", "patch"],
        filenames: &[],
        line_comment: "#",
        block_comment: None,
    },
    LanguageInfo {
        name: "Dockerfile",
        extensions: &["dockerfile"],
        filenames: &["Dockerfile", "Containerfile"],
        line_comment: "#",
        block_comment: None,
    },
    LanguageInfo {
        name: "Elixir",
        extensions: &["ex", "exs"],
        filenames: &[],
        line_comment: "#",
        block_comment: None,
    },
    LanguageInfo {
        name: "Erlang",
        extensions: &["erl", "hrl"],
        filenames: &[],
        line_comment: "%",
        block_comment: None,
    },
    LanguageInfo {
        name: "Go",
        extensions: &["go"],
        filenames: &[],
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
    },
    LanguageInfo {
        name: "Haskell",
        extensions: &["hs", "lhs"],
        filenames: &[],
        line_comment: "--",
        block_comment: Some(("{-", "-}")),
    },
    LanguageInfo {
        name: "HTML",
        extensions: &["html", "htm", "xhtml", "shtml"],
        filenames: &[],
        line_comment: "",
        block_comment: Some(("<!--", "-->")),
    },
    LanguageInfo {
        name: "INI",
        extensions: &["ini", "cfg", "conf", "inf"],
        filenames: &[".editorconfig"],
        line_comment: ";",
        block_comment: None,
    },
    LanguageInfo {
        name: "Java",
        extensions: &["java"],
        filenames: &[],
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
    },
    LanguageInfo {
        name: "JavaScript",
        extensions: &["js", "mjs", "cjs", "jsx"],
        filenames: &[],
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
    },
    LanguageInfo {
        name: "JSON",
        extensions: &["json", "jsonc", "json5"],
        filenames: &[".eslintrc", ".babelrc"],
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
    },
    LanguageInfo {
        name: "Kotlin",
        extensions: &["kt", "kts"],
        filenames: &[],
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
    },
    LanguageInfo {
        name: "Lisp",
        extensions: &["lisp", "lsp", "el"],
        filenames: &[],
        line_comment: ";",
        block_comment: None,
    },
    LanguageInfo {
        name: "Lua",
        extensions: &["lua"],
        filenames: &[],
        line_comment: "--",
        block_comment: Some(("--[[", "]]")),
    },
    LanguageInfo {
        name: "Makefile",
        extensions: &["mak", "mk"],
        filenames: &["Makefile", "makefile", "GNUmakefile"],
        line_comment: "#",
        block_comment: None,
    },
    LanguageInfo {
        name: "Markdown",
        extensions: &["md", "markdown", "mdown", "mkd"],
        filenames: &[],
        line_comment: "",
        block_comment: Some(("<!--", "-->")),
    },
    LanguageInfo {
        name: "Nix",
        extensions: &["nix"],
        filenames: &[],
        line_comment: "#",
        block_comment: Some(("/*", "*/")),
    },
    LanguageInfo {
        name: "Objective-C",
        extensions: &["m", "mm"],
        filenames: &[],
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
    },
    LanguageInfo {
        name: "Perl",
        extensions: &["pl", "pm", "pod", "t"],
        filenames: &[],
        line_comment: "#",
        block_comment: None,
    },
    LanguageInfo {
        name: "PHP",
        extensions: &["php", "php3", "php4", "php5", "phtml"],
        filenames: &[],
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
    },
    LanguageInfo {
        name: "PowerShell",
        extensions: &["ps1", "psm1", "psd1"],
        filenames: &[],
        line_comment: "#",
        block_comment: Some(("<#", "#>")),
    },
    LanguageInfo {
        name: "Properties",
        extensions: &["properties"],
        filenames: &[],
        line_comment: "#",
        block_comment: None,
    },
    LanguageInfo {
        name: "Python",
        extensions: &["py", "pyw", "pyi"],
        filenames: &[],
        line_comment: "#",
        block_comment: None,
    },
    LanguageInfo {
        name: "R",
        extensions: &["r", "R", "rmd"],
        filenames: &[],
        line_comment: "#",
        block_comment: None,
    },
    LanguageInfo {
        name: "Ruby",
        extensions: &["rb", "rake", "gemspec"],
        filenames: &["Rakefile", "Gemfile"],
        line_comment: "#",
        block_comment: Some(("=begin", "=end")),
    },
    LanguageInfo {
        name: "Rust",
        extensions: &["rs"],
        filenames: &[],
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
    },
    LanguageInfo {
        name: "Scala",
        extensions: &["scala", "sc"],
        filenames: &[],
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
    },
    LanguageInfo {
        name: "SCSS",
        extensions: &["scss", "sass", "less"],
        filenames: &[],
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
    },
    LanguageInfo {
        name: "Shell",
        extensions: &["sh", "bash", "zsh", "ksh", "fish"],
        filenames: &[".bashrc", ".zshrc", ".profile", ".bash_profile"],
        line_comment: "#",
        block_comment: None,
    },
    LanguageInfo {
        name: "SQL",
        extensions: &["sql"],
        filenames: &[],
        line_comment: "--",
        block_comment: Some(("/*", "*/")),
    },
    LanguageInfo {
        name: "Swift",
        extensions: &["swift"],
        filenames: &[],
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
    },
    LanguageInfo {
        name: "TOML",
        extensions: &["toml"],
        filenames: &["Cargo.lock"],
        line_comment: "#",
        block_comment: None,
    },
    LanguageInfo {
        name: "TeX",
        extensions: &["tex", "latex", "sty", "cls"],
        filenames: &[],
        line_comment: "%",
        block_comment: None,
    },
    LanguageInfo {
        name: "TypeScript",
        extensions: &["ts", "tsx", "mts", "cts"],
        filenames: &[],
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
    },
    LanguageInfo {
        name: "Verilog",
        extensions: &["v", "vh", "sv", "svh"],
        filenames: &[],
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
    },
    LanguageInfo {
        name: "VHDL",
        extensions: &["vhd", "vhdl"],
        filenames: &[],
        line_comment: "--",
        block_comment: None,
    },
    LanguageInfo {
        name: "XML",
        extensions: &[
            "xml", "xsd", "xsl", "xslt", "svg", "plist", "csproj", "vcxproj",
        ],
        filenames: &[],
        line_comment: "",
        block_comment: Some(("<!--", "-->")),
    },
    LanguageInfo {
        name: "YAML",
        extensions: &["yaml", "yml"],
        filenames: &[],
        line_comment: "#",
        block_comment: None,
    },
    LanguageInfo {
        name: "Zig",
        extensions: &["zig"],
        filenames: &[],
        line_comment: "//",
        block_comment: None,
    },
];

pub fn all() -> Vec<&'static LanguageInfo> {
    let mut languages: Vec<&'static LanguageInfo> = LANGUAGES.iter().collect();
    languages.extend(user_languages());
    languages
}

pub fn info(name: &str) -> Option<&'static LanguageInfo> {
    LANGUAGES
        .iter()
        .find(|l| l.name == name)
        .or_else(|| user_languages().into_iter().find(|l| l.name == name))
}

/// A curated list of language names for menus, sorted alphabetically and
/// starting with Plain Text.
pub fn names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = LANGUAGES.iter().map(|l| l.name).collect();
    for language in user_languages() {
        if !names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(language.name))
        {
            names.push(language.name);
        }
    }
    names.sort_by_key(|n| n.to_ascii_lowercase());
    names.insert(0, PLAIN_TEXT);
    names
}

/// Detect a language from a filename / path.
pub fn detect_from_path(path: &Path) -> &'static str {
    let users = user_languages();
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if let Some(info) = users
            .iter()
            .copied()
            .find(|l| l.filenames.iter().any(|f| f.eq_ignore_ascii_case(name)))
            .or_else(|| {
                LANGUAGES
                    .iter()
                    .find(|l| l.filenames.iter().any(|f| f.eq_ignore_ascii_case(name)))
            })
        {
            return info.name;
        }
        // Files such as `foo.d.ts` or `.bashrc`.
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".d.ts") {
            return "TypeScript";
        }
        if lower.starts_with(".env") {
            return "Shell";
        }
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    if ext.is_empty() {
        return PLAIN_TEXT;
    }
    users
        .iter()
        .copied()
        .find(|l| l.extensions.iter().any(|e| e.eq_ignore_ascii_case(ext)))
        .or_else(|| {
            LANGUAGES
                .iter()
                .find(|l| l.extensions.iter().any(|e| e.eq_ignore_ascii_case(ext)))
        })
        .map(|l| l.name)
        .unwrap_or(PLAIN_TEXT)
}

/// The line comment prefix for a language, if it has one.
pub fn line_comment(name: &str) -> Option<&'static str> {
    info(name).and_then(|l| {
        if l.line_comment.is_empty() {
            None
        } else {
            Some(l.line_comment)
        }
    })
}

pub fn block_comment(name: &str) -> Option<(&'static str, &'static str)> {
    info(name).and_then(|l| l.block_comment)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner())
    }

    #[test]
    fn detects_common_extensions() {
        let _guard = lock();
        assert_eq!(detect_from_path(&PathBuf::from("a.rs")), "Rust");
        assert_eq!(detect_from_path(&PathBuf::from("a.RS")), "Rust");
        assert_eq!(detect_from_path(&PathBuf::from("a.py")), "Python");
        assert_eq!(detect_from_path(&PathBuf::from("a.tsx")), "TypeScript");
        assert_eq!(detect_from_path(&PathBuf::from("types.d.ts")), "TypeScript");
        assert_eq!(detect_from_path(&PathBuf::from("Cargo.toml")), "TOML");
        assert_eq!(detect_from_path(&PathBuf::from("Makefile")), "Makefile");
        assert_eq!(detect_from_path(&PathBuf::from("unknown.xyz")), PLAIN_TEXT);
        assert_eq!(detect_from_path(&PathBuf::from("noext")), PLAIN_TEXT);
    }

    #[test]
    fn comment_metadata() {
        let _guard = lock();
        assert_eq!(line_comment("Rust"), Some("//"));
        assert_eq!(line_comment("Python"), Some("#"));
        assert_eq!(line_comment("HTML"), None);
        assert_eq!(block_comment("Rust"), Some(("/*", "*/")));
        assert_eq!(block_comment("Python"), None);
    }

    #[test]
    fn names_are_sorted_with_plain_text_first() {
        let _guard = lock();
        let names = names();
        assert_eq!(names[0], PLAIN_TEXT);
        assert!(names.len() > 30);
        let tail = &names[1..];
        let mut sorted = tail.to_vec();
        sorted.sort_by_key(|n| n.to_ascii_lowercase());
        assert_eq!(tail, sorted.as_slice());
    }

    #[test]
    fn user_languages_extend_detection_and_names() {
        let _guard = lock();
        set_user_languages(vec![UserLanguageSpec {
            name: "MyLang".into(),
            extensions: vec!["myl".into()],
            line_comment: Some("//".into()),
            block_comment: None,
            first_line_match: Some("^#!/usr/bin/env mylang".into()),
        }]);
        assert_eq!(
            detect_from_content("#!/usr/bin/env mylang\nprint('x')"),
            Some("MyLang")
        );
        assert_eq!(detect_from_path(&PathBuf::from("a.myl")), "MyLang");
        assert_eq!(line_comment("MyLang"), Some("//"));
        assert!(names().contains(&"MyLang"));
        let info = info("MyLang").unwrap();
        assert_eq!(info.extensions, ["myl"]);
        clear_user_languages();
        assert_eq!(detect_from_path(&PathBuf::from("a.myl")), PLAIN_TEXT);
    }
}

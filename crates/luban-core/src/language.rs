//! Language detection and editing metadata.
//!
//! Highlighting itself lives in the GUI crate (which embeds syntect's syntax
//! definitions). This module provides a deterministic extension map for the
//! status bar, the language menu, and comment toggling, including languages
//! that syntect does not ship with.

use std::path::Path;

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

pub fn all() -> &'static [LanguageInfo] {
    LANGUAGES
}

pub fn info(name: &str) -> Option<&'static LanguageInfo> {
    LANGUAGES.iter().find(|l| l.name == name)
}

/// A curated list of language names for menus, sorted alphabetically and
/// starting with Plain Text.
pub fn names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = LANGUAGES.iter().map(|l| l.name).collect();
    names.sort_by_key(|n| n.to_ascii_lowercase());
    names.insert(0, PLAIN_TEXT);
    names
}

/// Detect a language from a filename / path.
pub fn detect_from_path(path: &Path) -> &'static str {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if let Some(info) = LANGUAGES
            .iter()
            .find(|l| l.filenames.iter().any(|f| f.eq_ignore_ascii_case(name)))
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
    LANGUAGES
        .iter()
        .find(|l| l.extensions.iter().any(|e| e.eq_ignore_ascii_case(ext)))
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

    #[test]
    fn detects_common_extensions() {
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
        assert_eq!(line_comment("Rust"), Some("//"));
        assert_eq!(line_comment("Python"), Some("#"));
        assert_eq!(line_comment("HTML"), None);
        assert_eq!(block_comment("Rust"), Some(("/*", "*/")));
        assert_eq!(block_comment("Python"), None);
    }

    #[test]
    fn names_are_sorted_with_plain_text_first() {
        let names = names();
        assert_eq!(names[0], PLAIN_TEXT);
        assert!(names.len() > 30);
        let tail = &names[1..];
        let mut sorted = tail.to_vec();
        sorted.sort_by_key(|n| n.to_ascii_lowercase());
        assert_eq!(tail, sorted.as_slice());
    }
}

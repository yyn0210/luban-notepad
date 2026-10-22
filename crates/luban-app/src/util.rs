use std::path::{Component, Path, PathBuf};

/// Map a Notepad++-style language name to the token used by GPUI Component's
/// syntect highlighter.
pub fn language_key(name: &str) -> &str {
    match name {
        "Plain Text" => "text",
        "C" => "c",
        "C++" => "cpp",
        "C#" => "csharp",
        "Objective-C" => "objective-c",
        "Assembly" => "asm",
        "Batch" => "bat",
        "Shell" => "bash",
        "PowerShell" => "powershell",
        "JavaScript" => "javascript",
        "TypeScript" => "typescript",
        "SCSS" => "scss",
        "Diff" => "diff",
        "Properties" => "ini",
        "Makefile" => "make",
        "CMake" => "cmake",
        "Dockerfile" => "dockerfile",
        "Markdown" => "markdown",
        "TeX" => "tex",
        "VHDL" => "vhdl",
        "Verilog" => "verilog",
        "Haskell" => "haskell",
        "Elixir" => "elixir",
        "Clojure" => "clojure",
        "Lisp" => "lisp",
        "Nix" => "nix",
        "Zig" => "zig",
        other => other,
    }
}

/// Absolute, normalized path for display and duplicate detection.
pub fn absolutize(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    normalize(&absolute)
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component.as_os_str());
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub fn same_path(a: &Path, b: &Path) -> bool {
    let a = absolutize(a);
    let b = absolutize(b);
    if cfg!(windows) {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    } else {
        a == b
    }
}

pub fn default_save_directory(doc_path: Option<&Path>) -> PathBuf {
    doc_path
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .or_else(dirs::document_dir)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_aliases_are_stable() {
        assert_eq!(language_key("C++"), "cpp");
        assert_eq!(language_key("C#"), "csharp");
        assert_eq!(language_key("Plain Text"), "text");
        assert_eq!(language_key("Rust"), "Rust");
    }

    #[test]
    fn default_save_directory_uses_document_parent() {
        let path = PathBuf::from("/tmp/project/file.rs");
        assert_eq!(
            default_save_directory(Some(&path)),
            PathBuf::from("/tmp/project")
        );
        assert!(!default_save_directory(None).as_os_str().is_empty());
    }

    #[test]
    fn same_path_compares_normalized_paths() {
        let a = PathBuf::from("/tmp/a/../b.txt");
        let b = PathBuf::from("/tmp/b.txt");
        assert!(same_path(&a, &b));
    }

    #[test]
    fn paths_normalize() {
        let base = if cfg!(windows) { "C:\\tmp" } else { "/tmp" };
        let p = PathBuf::from(base).join("a").join("..").join("b");
        let abs = absolutize(&p);
        assert!(abs.ends_with("b"));
        assert!(!abs.to_string_lossy().contains(".."));
    }
}

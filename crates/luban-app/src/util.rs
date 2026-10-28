use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// A platform launcher invocation, kept separate from running it so the
/// argument construction can be tested.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenCommand {
    pub program: String,
    pub args: Vec<String>,
}

impl OpenCommand {
    pub fn spawn(&self) -> std::io::Result<()> {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        command.spawn().map(|_| ())
    }
}

/// Open a directory in the platform file manager.
pub fn open_directory_command(directory: &Path) -> OpenCommand {
    #[cfg(windows)]
    {
        OpenCommand {
            program: "explorer".to_string(),
            args: vec![directory.to_string_lossy().into_owned()],
        }
    }
    #[cfg(target_os = "macos")]
    {
        OpenCommand {
            program: "open".to_string(),
            args: vec![directory.to_string_lossy().into_owned()],
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        OpenCommand {
            program: "xdg-open".to_string(),
            args: vec![directory.to_string_lossy().into_owned()],
        }
    }
}

/// Reveal a file in the platform file manager, selecting it when supported.
pub fn reveal_file_command(path: &Path) -> OpenCommand {
    #[cfg(windows)]
    {
        OpenCommand {
            program: "explorer".to_string(),
            args: vec!["/select,".to_string(), path.to_string_lossy().into_owned()],
        }
    }
    #[cfg(target_os = "macos")]
    {
        OpenCommand {
            program: "open".to_string(),
            args: vec!["-R".to_string(), path.to_string_lossy().into_owned()],
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let parent = path.parent().unwrap_or(path);
        OpenCommand {
            program: "xdg-open".to_string(),
            args: vec![parent.to_string_lossy().into_owned()],
        }
    }
}

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

    #[test]
    fn open_directory_command_uses_the_platform_launcher() {
        let dir = PathBuf::from("/tmp/example");
        let command = open_directory_command(&dir);
        #[cfg(windows)]
        {
            assert_eq!(command.program, "explorer");
            assert_eq!(command.args, vec!["/tmp/example".to_string()]);
        }
        #[cfg(target_os = "macos")]
        {
            assert_eq!(command.program, "open");
            assert_eq!(command.args, vec!["/tmp/example".to_string()]);
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            assert_eq!(command.program, "xdg-open");
            assert_eq!(command.args, vec!["/tmp/example".to_string()]);
        }
    }

    #[test]
    fn reveal_file_command_selects_or_opens_the_parent() {
        let path = PathBuf::from("/tmp/example/note.txt");
        let command = reveal_file_command(&path);
        #[cfg(windows)]
        {
            assert_eq!(command.program, "explorer");
            assert_eq!(command.args[0], "/select,");
        }
        #[cfg(target_os = "macos")]
        {
            assert_eq!(command.program, "open");
            assert_eq!(command.args[0], "-R");
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            assert_eq!(command.program, "xdg-open");
            assert_eq!(command.args, vec!["/tmp/example".to_string()]);
        }
    }
}

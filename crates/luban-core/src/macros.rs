//! Command-level macros.
//!
//! A macro stores an ordered list of registered command IDs. Recording raw
//! keystrokes is deliberately out of scope for 0.15: replaying command IDs is
//! deterministic and can be validated against the command registry.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedMacro {
    pub name: String,
    pub commands: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MacroSet {
    pub macros: Vec<SavedMacro>,
    /// Shortcut text per macro name.
    pub bindings: BTreeMap<String, String>,
}

impl MacroSet {
    pub fn find(&self, name: &str) -> Option<&SavedMacro> {
        self.macros.iter().find(|saved| saved.name == name)
    }

    pub fn find_mut(&mut self, name: &str) -> Option<&mut SavedMacro> {
        self.macros.iter_mut().find(|saved| saved.name == name)
    }

    /// Insert or replace a macro by name. An empty name is rejected.
    pub fn upsert(&mut self, name: &str, commands: Vec<String>) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("macro name is empty".to_string());
        }
        let commands = normalize_commands(commands);
        if commands.is_empty() {
            return Err("macro has no commands".to_string());
        }
        match self.find_mut(name) {
            Some(saved) => saved.commands = commands,
            None => self.macros.push(SavedMacro {
                name: name.to_string(),
                commands,
            }),
        }
        Ok(())
    }

    pub fn remove(&mut self, name: &str) {
        self.macros.retain(|saved| saved.name != name);
        self.bindings.remove(name);
    }

    /// Rename a macro, moving its shortcut with it.
    pub fn rename(&mut self, old: &str, new: &str) -> Result<(), String> {
        let new = new.trim();
        if new.is_empty() {
            return Err("macro name is empty".to_string());
        }
        if old == new {
            return Ok(());
        }
        if self.find(new).is_some() {
            return Err(format!("macro '{new}' already exists"));
        }
        let Some(saved) = self.find_mut(old) else {
            return Err(format!("macro '{old}' does not exist"));
        };
        saved.name = new.to_string();
        if let Some(binding) = self.bindings.remove(old) {
            self.bindings.insert(new.to_string(), binding);
        }
        Ok(())
    }

    pub fn binding(&self, name: &str) -> Option<&str> {
        self.bindings.get(name).map(String::as_str)
    }

    pub fn set_binding(&mut self, name: &str, binding: String) {
        self.bindings.insert(name.to_string(), binding);
    }

    pub fn remove_binding(&mut self, name: &str) {
        self.bindings.remove(name);
    }

    pub fn is_empty(&self) -> bool {
        self.macros.is_empty() && self.bindings.is_empty()
    }

    /// Drop malformed entries after loading settings.
    pub fn validate(&mut self) {
        let mut seen = std::collections::HashSet::new();
        self.macros.retain_mut(|saved| {
            saved.name = saved.name.trim().to_string();
            if saved.name.is_empty() || !seen.insert(saved.name.clone()) {
                return false;
            }
            saved.commands = normalize_commands(std::mem::take(&mut saved.commands));
            !saved.commands.is_empty()
        });
        let names: std::collections::HashSet<String> =
            self.macros.iter().map(|saved| saved.name.clone()).collect();
        self.bindings
            .retain(|name, binding| names.contains(name) && !binding.trim().is_empty());
        for binding in self.bindings.values_mut() {
            *binding = binding.trim().to_lowercase();
        }
    }
}

fn normalize_commands(commands: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for command in commands {
        let command = command.trim();
        if !command.is_empty() && recordable(command) {
            out.push(command.to_string());
        }
    }
    out
}

/// Whether a command may appear in a recorded macro.
///
/// Macro control commands, palette overlays, help dialogs and commands whose
/// purpose is to open a modal file/settings dialog are excluded, so replaying a
/// macro cannot recurse into itself or require user interaction.
pub fn recordable(id: &str) -> bool {
    !(id.starts_with("macro.")
        || id.starts_with("palette.")
        || id.starts_with("help.")
        || matches!(
            id,
            "file.open"
                | "file.open-folder"
                | "file.save-as"
                | "file.local-history"
                | "settings.preferences"
                | "view.choose-language"
                | "search.find-in-files"
                | "macro.play-saved"
                | "macro.save-recording"
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upserts_and_removes_macros() {
        let mut set = MacroSet::default();
        set.upsert("first", vec!["edit.duplicate-line".into()])
            .unwrap();
        set.upsert("first", vec!["edit.delete-line".into()])
            .unwrap();
        assert_eq!(set.macros.len(), 1);
        assert_eq!(set.find("first").unwrap().commands, ["edit.delete-line"]);
        set.set_binding("first", "ctrl+alt+1".into());
        assert_eq!(set.binding("first"), Some("ctrl+alt+1"));
        set.remove("first");
        assert!(set.is_empty());
        assert!(set.binding("first").is_none());
    }

    #[test]
    fn rejects_empty_macros_and_filters_control_commands() {
        let mut set = MacroSet::default();
        assert!(set.upsert("", vec!["edit.delete-line".into()]).is_err());
        assert!(set
            .upsert("x", vec!["macro.toggle-recording".into()])
            .is_err());
        set.upsert(
            "x",
            vec![
                "edit.delete-line".into(),
                "palette.commands".into(),
                "edit.undo".into(),
            ],
        )
        .unwrap();
        assert_eq!(
            set.find("x").unwrap().commands,
            ["edit.delete-line", "edit.undo"]
        );
    }

    #[test]
    fn renames_macros_and_moves_bindings() {
        let mut set = MacroSet::default();
        set.upsert("old", vec!["edit.undo".into()]).unwrap();
        set.set_binding("old", "ctrl+alt+1".into());
        set.upsert("taken", vec!["edit.redo".into()]).unwrap();
        assert!(set.rename("old", "taken").is_err());
        set.rename("old", "new").unwrap();
        assert!(set.find("old").is_none());
        assert_eq!(set.binding("new"), Some("ctrl+alt+1"));
        assert!(set.rename("missing", "x").is_err());
    }

    #[test]
    fn validate_drops_bad_entries_and_orphan_bindings() {
        let mut set = MacroSet {
            macros: vec![
                SavedMacro {
                    name: "  ".into(),
                    commands: vec!["edit.undo".into()],
                },
                SavedMacro {
                    name: "keep".into(),
                    commands: vec![" edit.undo ".into(), "".into()],
                },
                SavedMacro {
                    name: "keep".into(),
                    commands: vec!["edit.redo".into()],
                },
            ],
            bindings: BTreeMap::from([
                ("missing".to_string(), "ctrl+alt+9".to_string()),
                ("keep".to_string(), " Ctrl+Alt+8 ".to_string()),
            ]),
        };
        set.validate();
        assert_eq!(set.macros.len(), 1);
        assert_eq!(set.macros[0].name, "keep");
        assert_eq!(
            set.bindings.get("keep").map(String::as_str),
            Some("ctrl+alt+8")
        );
        assert!(!set.bindings.contains_key("missing"));
    }
}

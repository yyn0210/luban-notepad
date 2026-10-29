//! Static command registry and keyboard binding model.
//!
//! Menus, the toolbar, the shortcut help overlay and the command palette all
//! read labels and bindings from this table. Command execution itself lives in
//! `LubanApp::run_command`, which keeps the menu and palette behaviour on the
//! same path.

use std::collections::BTreeMap;

/// Where a command is grouped in the palette and shortcut help.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CommandCategory {
    File,
    Edit,
    Search,
    View,
    Tabs,
    Navigation,
    Bookmarks,
    Macros,
    Settings,
    Help,
}

impl CommandCategory {
    pub const ALL: [Self; 10] = [
        Self::File,
        Self::Edit,
        Self::Search,
        Self::View,
        Self::Tabs,
        Self::Navigation,
        Self::Bookmarks,
        Self::Macros,
        Self::Settings,
        Self::Help,
    ];

    pub fn label_key(self) -> &'static str {
        match self {
            Self::File => "command.category.file",
            Self::Edit => "command.category.edit",
            Self::Search => "command.category.search",
            Self::View => "command.category.view",
            Self::Tabs => "command.category.tabs",
            Self::Navigation => "command.category.navigation",
            Self::Bookmarks => "command.category.bookmarks",
            Self::Macros => "command.category.macros",
            Self::Settings => "command.category.settings",
            Self::Help => "command.category.help",
        }
    }
}

/// State required for a command to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandWhen {
    Always,
    HasDocument,
    HasPath,
    CanSave,
    CanUndo,
    CanRedo,
    CanEditFullText,
}

/// Registry entry.
#[derive(Clone, Copy, Debug)]
pub struct CommandSpec {
    pub id: &'static str,
    pub category: CommandCategory,
    pub label_key: &'static str,
    pub default_binding: Option<&'static str>,
    pub when: CommandWhen,
}

pub struct CommandContext {
    pub has_document: bool,
    pub has_path: bool,
    pub can_save: bool,
    pub can_undo: bool,
    pub can_redo: bool,
    /// The active tab holds complete, editable text (not a disk-engine window,
    /// range view, reduced large-file view or non-text buffer).
    pub can_edit_full_text: bool,
}

impl CommandSpec {
    pub fn enabled(&self, context: &CommandContext) -> bool {
        match self.when {
            CommandWhen::Always => true,
            CommandWhen::HasDocument => context.has_document,
            CommandWhen::HasPath => context.has_path,
            CommandWhen::CanSave => context.can_save,
            CommandWhen::CanUndo => context.can_undo,
            CommandWhen::CanRedo => context.can_redo,
            CommandWhen::CanEditFullText => context.can_edit_full_text,
        }
    }
}

macro_rules! command {
    ($id:literal, $category:ident, $label:literal, $binding:expr, $when:ident) => {
        CommandSpec {
            id: $id,
            category: CommandCategory::$category,
            label_key: $label,
            default_binding: $binding,
            when: CommandWhen::$when,
        }
    };
}

/// All registered commands, in menu and palette order.
pub const COMMANDS: &[CommandSpec] = &[
    command!("file.new", File, "menu.file.new", Some("ctrl+n"), Always),
    command!("file.open", File, "menu.file.open", Some("ctrl+o"), Always),
    command!(
        "file.open-folder",
        File,
        "menu.file.open_folder",
        Some("ctrl+shift+o"),
        Always
    ),
    command!("file.save", File, "menu.file.save", Some("ctrl+s"), CanSave),
    command!(
        "file.save-as",
        File,
        "menu.file.save_as",
        Some("ctrl+shift+s"),
        CanSave
    ),
    command!("file.save-all", File, "menu.file.save_all", None, CanSave),
    command!("file.reload", File, "menu.file.reload", None, HasPath),
    command!(
        "file.local-history",
        File,
        "menu.file.local_history",
        None,
        HasPath
    ),
    command!(
        "file.export-html",
        File,
        "menu.file.export_html",
        None,
        HasDocument
    ),
    command!(
        "file.export-rtf",
        File,
        "menu.file.export_rtf",
        None,
        HasDocument
    ),
    command!(
        "file.close",
        File,
        "menu.file.close",
        Some("ctrl+w"),
        HasDocument
    ),
    command!(
        "file.close-all",
        File,
        "menu.file.close_all",
        Some("ctrl+shift+w"),
        HasDocument
    ),
    command!("file.exit", File, "menu.file.exit", Some("ctrl+q"), Always),
    command!("edit.undo", Edit, "menu.edit.undo", Some("ctrl+z"), CanUndo),
    command!(
        "edit.redo",
        Edit,
        "menu.edit.redo",
        Some("ctrl+shift+z"),
        CanRedo
    ),
    command!(
        "edit.cut",
        Edit,
        "menu.edit.cut",
        Some("ctrl+x"),
        HasDocument
    ),
    command!(
        "edit.copy",
        Edit,
        "menu.edit.copy",
        Some("ctrl+c"),
        HasDocument
    ),
    command!(
        "edit.paste",
        Edit,
        "menu.edit.paste",
        Some("ctrl+v"),
        HasDocument
    ),
    command!(
        "edit.select-all",
        Edit,
        "menu.edit.select_all",
        Some("ctrl+a"),
        HasDocument
    ),
    command!("edit.indent", Edit, "menu.edit.indent", None, HasDocument),
    command!("edit.outdent", Edit, "menu.edit.outdent", None, HasDocument),
    command!(
        "edit.duplicate-line",
        Edit,
        "menu.edit.duplicate_line",
        Some("ctrl+d"),
        HasDocument
    ),
    command!(
        "edit.delete-line",
        Edit,
        "menu.edit.delete_line",
        Some("ctrl+shift+k"),
        HasDocument
    ),
    command!(
        "edit.toggle-comment",
        Edit,
        "menu.edit.toggle_comment",
        Some("ctrl+/"),
        HasDocument
    ),
    command!(
        "edit.trim-whitespace",
        Edit,
        "menu.edit.trim_whitespace",
        None,
        HasDocument
    ),
    command!(
        "edit.uppercase",
        Edit,
        "menu.edit.uppercase",
        None,
        HasDocument
    ),
    command!(
        "edit.lowercase",
        Edit,
        "menu.edit.lowercase",
        None,
        HasDocument
    ),
    command!(
        "edit.title-case",
        Edit,
        "menu.edit.title_case",
        None,
        HasDocument
    ),
    command!(
        "edit.sort-asc",
        Edit,
        "menu.edit.sort_asc",
        None,
        HasDocument
    ),
    command!(
        "edit.sort-desc",
        Edit,
        "menu.edit.sort_desc",
        None,
        HasDocument
    ),
    command!(
        "edit.sort-ignore-case-asc",
        Edit,
        "menu.edit.sort_ignore_case_asc",
        None,
        HasDocument
    ),
    command!(
        "edit.sort-ignore-case-desc",
        Edit,
        "menu.edit.sort_ignore_case_desc",
        None,
        HasDocument
    ),
    command!(
        "edit.sort-numeric-asc",
        Edit,
        "menu.edit.sort_numeric_asc",
        None,
        HasDocument
    ),
    command!(
        "edit.sort-numeric-desc",
        Edit,
        "menu.edit.sort_numeric_desc",
        None,
        HasDocument
    ),
    command!(
        "edit.sort-by-column",
        Edit,
        "menu.edit.sort_by_column",
        None,
        HasDocument
    ),
    command!(
        "edit.move-line-up",
        Edit,
        "menu.edit.move_line_up",
        Some("ctrl+shift+up"),
        HasDocument
    ),
    command!(
        "edit.move-line-down",
        Edit,
        "menu.edit.move_line_down",
        Some("ctrl+shift+down"),
        HasDocument
    ),
    command!(
        "edit.join-lines",
        Edit,
        "menu.edit.join_lines",
        Some("ctrl+j"),
        HasDocument
    ),
    command!(
        "edit.split-lines",
        Edit,
        "menu.edit.split_lines",
        None,
        HasDocument
    ),
    command!(
        "edit.remove-duplicate-lines",
        Edit,
        "menu.edit.remove_duplicate_lines",
        None,
        HasDocument
    ),
    command!(
        "edit.remove-empty-lines",
        Edit,
        "menu.edit.remove_empty_lines",
        None,
        HasDocument
    ),
    command!(
        "edit.filter-lines",
        Edit,
        "menu.edit.filter_lines",
        None,
        HasDocument
    ),
    command!(
        "edit.column-editor",
        Edit,
        "menu.edit.column_editor",
        Some("alt+c"),
        HasDocument
    ),
    command!(
        "edit.add-cursor-above",
        Edit,
        "menu.edit.add_cursor_above",
        Some("ctrl+alt+up"),
        CanEditFullText
    ),
    command!(
        "edit.add-cursor-below",
        Edit,
        "menu.edit.add_cursor_below",
        Some("ctrl+alt+down"),
        CanEditFullText
    ),
    command!(
        "edit.select-next-occurrence",
        Edit,
        "menu.edit.select_next_occurrence",
        Some("ctrl+shift+d"),
        CanEditFullText
    ),
    command!(
        "edit.select-all-occurrences",
        Edit,
        "menu.edit.select_all_occurrences",
        Some("ctrl+shift+l"),
        CanEditFullText
    ),
    command!(
        "edit.unfold-current",
        Edit,
        "menu.edit.unfold_current",
        None,
        HasDocument
    ),
    command!(
        "edit.unfold-all",
        Edit,
        "menu.edit.unfold_all",
        None,
        HasDocument
    ),
    command!(
        "search.find",
        Search,
        "menu.search.find",
        Some("ctrl+f"),
        HasDocument
    ),
    command!(
        "search.replace",
        Search,
        "menu.search.replace",
        Some("ctrl+h"),
        HasDocument
    ),
    command!(
        "search.find-next",
        Search,
        "menu.search.find_next",
        Some("f3"),
        HasDocument
    ),
    command!(
        "search.find-previous",
        Search,
        "menu.search.find_previous",
        Some("shift+f3"),
        HasDocument
    ),
    command!(
        "search.go-to-line",
        Search,
        "menu.search.go_to_line",
        Some("ctrl+g"),
        HasDocument
    ),
    command!(
        "search.find-in-files",
        Search,
        "menu.search.find_in_files",
        Some("ctrl+shift+f"),
        Always
    ),
    command!(
        "search.clear-highlights",
        Search,
        "menu.search.clear_highlights",
        None,
        Always
    ),
    command!(
        "search.mark-all",
        Search,
        "menu.search.mark_all",
        None,
        HasDocument
    ),
    command!(
        "search.clear-history",
        Search,
        "menu.search.clear_history",
        None,
        Always
    ),
    command!("view.word-wrap", View, "menu.view.word_wrap", None, Always),
    command!(
        "view.line-numbers",
        View,
        "menu.view.line_numbers",
        None,
        Always
    ),
    command!("view.toolbar", View, "menu.view.toolbar", None, Always),
    command!(
        "view.status-bar",
        View,
        "menu.view.status_bar",
        None,
        Always
    ),
    command!("view.explorer", View, "menu.view.explorer", None, Always),
    command!(
        "view.large-file-controller",
        View,
        "menu.view.large_file_controller",
        None,
        HasDocument
    ),
    command!(
        "view.zoom-in",
        View,
        "menu.view.zoom_in",
        Some("ctrl++"),
        HasDocument
    ),
    command!(
        "view.zoom-out",
        View,
        "menu.view.zoom_out",
        Some("ctrl+-"),
        HasDocument
    ),
    command!(
        "view.zoom-reset",
        View,
        "menu.view.zoom_reset",
        Some("ctrl+0"),
        HasDocument
    ),
    command!(
        "view.theme-light",
        View,
        "menu.view.light_theme",
        None,
        Always
    ),
    command!(
        "view.theme-dark",
        View,
        "menu.view.dark_theme",
        None,
        Always
    ),
    command!(
        "view.theme-system",
        View,
        "menu.view.system_theme",
        None,
        Always
    ),
    command!("view.toggle-theme", View, "toolbar.theme", None, Always),
    command!(
        "view.fullscreen",
        View,
        "menu.view.fullscreen",
        Some("f11"),
        Always
    ),
    command!(
        "view.choose-language",
        View,
        "menu.view.choose_language",
        None,
        HasDocument
    ),
    command!(
        "tabs.close-others",
        Tabs,
        "menu.tabs.close_others",
        None,
        HasDocument
    ),
    command!(
        "tabs.close-right",
        Tabs,
        "menu.tabs.close_right",
        None,
        HasDocument
    ),
    command!("tabs.copy-path", Tabs, "menu.tabs.copy_path", None, HasPath),
    command!("tabs.reveal", Tabs, "menu.tabs.reveal", None, HasPath),
    command!(
        "tabs.reopen-closed",
        Tabs,
        "menu.tabs.reopen_closed",
        Some("ctrl+shift+t"),
        Always
    ),
    command!(
        "tabs.next",
        Tabs,
        "menu.tabs.next",
        Some("ctrl+tab"),
        HasDocument
    ),
    command!(
        "tabs.previous",
        Tabs,
        "menu.tabs.previous",
        Some("ctrl+shift+tab"),
        HasDocument
    ),
    command!(
        "bookmarks.toggle",
        Bookmarks,
        "menu.bookmarks.toggle",
        Some("ctrl+f2"),
        HasDocument
    ),
    command!(
        "bookmarks.next",
        Bookmarks,
        "menu.bookmarks.next",
        Some("f2"),
        HasDocument
    ),
    command!(
        "bookmarks.previous",
        Bookmarks,
        "menu.bookmarks.previous",
        Some("shift+f2"),
        HasDocument
    ),
    command!(
        "bookmarks.clear",
        Bookmarks,
        "menu.bookmarks.clear",
        Some("ctrl+shift+f2"),
        HasDocument
    ),
    command!(
        "bookmarks.cut-lines",
        Bookmarks,
        "menu.bookmarks.cut_lines",
        None,
        HasDocument
    ),
    command!(
        "bookmarks.copy-lines",
        Bookmarks,
        "menu.bookmarks.copy_lines",
        None,
        HasDocument
    ),
    command!(
        "bookmarks.delete-lines",
        Bookmarks,
        "menu.bookmarks.delete_lines",
        None,
        HasDocument
    ),
    command!(
        "macro.toggle-recording",
        Macros,
        "menu.macro.toggle_recording",
        Some("ctrl+shift+r"),
        Always
    ),
    command!(
        "macro.play-last",
        Macros,
        "menu.macro.play_last",
        Some("ctrl+shift+m"),
        Always
    ),
    command!(
        "macro.play-saved",
        Macros,
        "menu.macro.play_saved",
        None,
        Always
    ),
    command!(
        "macro.save-recording",
        Macros,
        "menu.macro.save_recording",
        None,
        Always
    ),
    command!(
        "palette.commands",
        Navigation,
        "command.palette",
        Some("ctrl+shift+p"),
        Always
    ),
    command!(
        "palette.quick-open",
        Navigation,
        "command.quick_open",
        Some("ctrl+p"),
        Always
    ),
    command!(
        "settings.preferences",
        Settings,
        "menu.settings.preferences",
        None,
        Always
    ),
    command!("help.shortcuts", Help, "menu.help.shortcuts", None, Always),
    command!("help.about", Help, "menu.help.about", None, Always),
    command!(
        "help.open-log-dir",
        Help,
        "menu.help.open_logs",
        None,
        Always
    ),
];

pub fn spec(id: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|command| command.id == id)
}

/// A parsed shortcut.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Binding {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: String,
}

impl Binding {
    /// Whether this binding matches a keystroke. `ctrl` is the platform's
    /// secondary modifier: Command on macOS, Control elsewhere.
    pub fn matches(&self, key: &str, ctrl: bool, shift: bool, alt: bool) -> bool {
        self.key.eq_ignore_ascii_case(key)
            && self.ctrl == ctrl
            && self.shift == shift
            && self.alt == alt
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindingError {
    Empty,
    UnknownModifier(String),
    MissingKey,
    UnknownKey(String),
    UnknownCommand(String),
    Duplicate(String),
    SingleKey,
}

impl std::fmt::Display for BindingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "shortcut is empty"),
            Self::UnknownModifier(part) => write!(f, "unknown modifier '{part}'"),
            Self::MissingKey => write!(f, "shortcut has no key"),
            Self::UnknownKey(key) => write!(f, "unknown key '{key}'"),
            Self::UnknownCommand(id) => write!(f, "unknown command '{id}'"),
            Self::Duplicate(id) => write!(f, "shortcut is already used by {id}"),
            Self::SingleKey => write!(f, "single-key shortcuts are not supported"),
        }
    }
}

const NAMED_KEYS: &[&str] = &[
    "tab",
    "escape",
    "space",
    "enter",
    "backspace",
    "delete",
    "insert",
    "home",
    "end",
    "pageup",
    "pagedown",
    "up",
    "down",
    "left",
    "right",
    "plus",
    "minus",
    "slash",
    "comma",
    "period",
    "semicolon",
    "quote",
    "backslash",
    "bracketleft",
    "bracketright",
    "backquote",
];

/// Parse a shortcut such as `ctrl+shift+p`, `f3`, `shift+f3` or `ctrl++`.
pub fn parse_binding(text: &str) -> Result<Binding, BindingError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(BindingError::Empty);
    }
    let mut rest = text;
    let mut ctrl = false;
    let mut shift = false;
    let mut alt = false;
    while let Some((part, tail)) = rest.split_once('+') {
        let part = part.trim();
        if part.is_empty() {
            // `ctrl++` means Control plus the plus key.
            rest = &rest[rest.len() - 1..];
            break;
        }
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "cmd" | "command" | "super" => ctrl = true,
            "shift" => shift = true,
            "alt" | "option" => alt = true,
            other => return Err(BindingError::UnknownModifier(other.to_string())),
        }
        rest = tail;
    }
    let key = rest.trim();
    if key.is_empty() {
        return Err(BindingError::MissingKey);
    }
    let key = normalize_key(key).ok_or_else(|| BindingError::UnknownKey(key.to_string()))?;
    Ok(Binding {
        ctrl,
        shift,
        alt,
        key,
    })
}

fn normalize_key(key: &str) -> Option<String> {
    let lower = key.to_ascii_lowercase();
    let lower = match lower.as_str() {
        "+" | "=" => "plus".to_string(),
        "-" | "_" => "minus".to_string(),
        "/" => "slash".to_string(),
        "," => "comma".to_string(),
        "." => "period".to_string(),
        ";" => "semicolon".to_string(),
        "'" => "quote".to_string(),
        "\\" => "backslash".to_string(),
        "[" => "bracketleft".to_string(),
        "]" => "bracketright".to_string(),
        "`" => "backquote".to_string(),
        other => other.to_string(),
    };
    if lower.chars().count() == 1 && lower.chars().all(|ch| ch.is_ascii_alphanumeric()) {
        return Some(lower);
    }
    if let Some(number) = lower.strip_prefix('f') {
        if let Ok(index) = number.parse::<u8>() {
            if (1..=24).contains(&index) {
                return Some(lower);
            }
        }
    }
    NAMED_KEYS.contains(&lower.as_str()).then_some(lower)
}

/// Display form, e.g. `Ctrl+Shift+P`, `F3`, `Ctrl++`.
pub fn format_binding(binding: &Binding) -> String {
    let mut parts = Vec::new();
    if binding.ctrl {
        parts.push("Ctrl".to_string());
    }
    if binding.shift {
        parts.push("Shift".to_string());
    }
    if binding.alt {
        parts.push("Alt".to_string());
    }
    parts.push(match binding.key.as_str() {
        "plus" => "+".to_string(),
        "minus" => "-".to_string(),
        "slash" => "/".to_string(),
        "comma" => ",".to_string(),
        "period" => ".".to_string(),
        "semicolon" => ";".to_string(),
        "quote" => "'".to_string(),
        "backslash" => "\\".to_string(),
        "bracketleft" => "[".to_string(),
        "bracketright" => "]".to_string(),
        "backquote" => "`".to_string(),
        "tab" => "Tab".to_string(),
        "escape" => "Esc".to_string(),
        "space" => "Space".to_string(),
        "enter" => "Enter".to_string(),
        "backspace" => "Backspace".to_string(),
        "delete" => "Delete".to_string(),
        "insert" => "Insert".to_string(),
        "home" => "Home".to_string(),
        "end" => "End".to_string(),
        "pageup" => "PageUp".to_string(),
        "pagedown" => "PageDown".to_string(),
        "up" => "↑".to_string(),
        "down" => "↓".to_string(),
        "left" => "←".to_string(),
        "right" => "→".to_string(),
        other if other.len() == 1 => other.to_ascii_uppercase(),
        other if other.starts_with('f') => other.to_ascii_uppercase(),
        other => other.to_string(),
    });
    parts.join("+")
}

/// Whether a binding is allowed as a user override. Bare keys are rejected
/// except for function keys, so a typed letter can never shadow editor input.
pub fn binding_allowed(binding: &Binding) -> bool {
    binding.ctrl || binding.shift || binding.alt || is_function_key(&binding.key)
}

fn is_function_key(key: &str) -> bool {
    key.strip_prefix('f')
        .and_then(|number| number.parse::<u8>().ok())
        .is_some_and(|index| (1..=24).contains(&index))
}

/// The effective keymap for a set of user overrides.
pub struct Keymap {
    /// Binding text per command ID.
    pub bindings: BTreeMap<&'static str, String>,
    /// Parsed bindings used for matching.
    entries: Vec<(Binding, &'static str)>,
    /// Defaults belonging to commands the user rebound. They are consumed so
    /// the old shortcut cannot run the command through the legacy handler.
    suppressed: Vec<(Binding, &'static str)>,
}

impl Keymap {
    pub fn build(overrides: &BTreeMap<String, String>) -> Self {
        let mut bindings = BTreeMap::new();
        let mut entries = Vec::new();
        let mut suppressed = Vec::new();
        for command in COMMANDS {
            let default = command
                .default_binding
                .and_then(|text| parse_binding(text).ok());
            let binding = match overrides.get(command.id) {
                Some(text) => parse_binding(text).ok(),
                None => default.clone(),
            };
            if let Some(binding) = &binding {
                bindings.insert(command.id, format_binding(binding));
                entries.push((binding.clone(), command.id));
            }
            if let Some(default) = default {
                let overridden = overrides
                    .get(command.id)
                    .is_some_and(|text| parse_binding(text).ok().as_ref() != Some(&default));
                if overridden {
                    suppressed.push((default, command.id));
                }
            }
        }
        Self {
            bindings,
            entries,
            suppressed,
        }
    }

    pub fn lookup(&self, key: &str, ctrl: bool, shift: bool, alt: bool) -> Option<&'static str> {
        self.entries
            .iter()
            .find(|(binding, _)| binding.matches(key, ctrl, shift, alt))
            .map(|(_, id)| *id)
    }

    /// The command whose replaced default uses this keystroke, if any.
    pub fn suppressed_binding(
        &self,
        key: &str,
        ctrl: bool,
        shift: bool,
        alt: bool,
    ) -> Option<&'static str> {
        self.suppressed
            .iter()
            .find(|(binding, _)| binding.matches(key, ctrl, shift, alt))
            .map(|(_, id)| *id)
    }

    pub fn binding_text(&self, id: &str) -> Option<&str> {
        self.bindings.get(id).map(String::as_str)
    }
}

/// A shortcut claimed by two commands, considering user overrides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingConflict {
    pub binding: String,
    pub first: &'static str,
    pub second: &'static str,
}

/// Find duplicate effective bindings.
pub fn conflicts(overrides: &BTreeMap<String, String>) -> Vec<BindingConflict> {
    let keymap = Keymap::build(overrides);
    let mut seen: Vec<(String, &'static str)> = Vec::new();
    let mut conflicts = Vec::new();
    for command in COMMANDS {
        let Some(text) = keymap.binding_text(command.id) else {
            continue;
        };
        if let Some((_, first)) = seen.iter().find(|(existing, _)| existing == text) {
            conflicts.push(BindingConflict {
                binding: text.to_string(),
                first,
                second: command.id,
            });
        } else {
            seen.push((text.to_string(), command.id));
        }
    }
    conflicts
}

/// Validate a proposed override before it is stored.
pub fn validate_override(
    id: &str,
    text: &str,
    overrides: &BTreeMap<String, String>,
) -> Result<String, BindingError> {
    if spec(id).is_none() {
        return Err(BindingError::UnknownCommand(id.to_string()));
    }
    let binding = parse_binding(text)?;
    if !binding_allowed(&binding) {
        return Err(BindingError::SingleKey);
    }
    let formatted = format_binding(&binding);
    let mut proposed = overrides.clone();
    proposed.insert(id.to_string(), text.to_string());
    if let Some(conflict) = conflicts(&proposed)
        .into_iter()
        .find(|conflict| conflict.first == id || conflict.second == id)
    {
        let other = if conflict.first == id {
            conflict.second
        } else {
            conflict.first
        };
        return Err(BindingError::Duplicate(other.to_string()));
    }
    Ok(formatted)
}

/// Parse macro shortcuts into matchable entries, skipping invalid values.
pub fn macro_keymap(bindings: &BTreeMap<String, String>) -> Vec<(Binding, String)> {
    let mut out = Vec::new();
    for (name, text) in bindings {
        if let Ok(binding) = parse_binding(text) {
            out.push((binding, name.clone()));
        }
    }
    out
}

/// Validate a shortcut assigned to a macro.
///
/// A macro binding may not use a bare key, may not collide with an effective
/// command binding, and may not collide with another macro. Default command
/// bindings that the user suppressed are free for macros.
pub fn validate_macro_binding(
    name: &str,
    text: &str,
    overrides: &BTreeMap<String, String>,
    macro_bindings: &BTreeMap<String, String>,
) -> Result<String, BindingError> {
    let binding = parse_binding(text)?;
    if !binding_allowed(&binding) {
        return Err(BindingError::SingleKey);
    }
    let keymap = Keymap::build(overrides);
    if let Some(command) = keymap.lookup(&binding.key, binding.ctrl, binding.shift, binding.alt) {
        return Err(BindingError::Duplicate(command.to_string()));
    }
    for (other, text) in macro_bindings {
        if other == name {
            continue;
        }
        if let Ok(existing) = parse_binding(text) {
            if existing == binding {
                return Err(BindingError::Duplicate(other.clone()));
            }
        }
    }
    Ok(format_binding(&binding))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_ids_are_unique() {
        let mut ids: Vec<_> = COMMANDS.iter().map(|command| command.id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count);
    }

    #[test]
    fn every_default_binding_parses_and_is_unique() {
        let keymap = Keymap::build(&BTreeMap::new());
        let mut defaults: Vec<_> = keymap.bindings.values().cloned().collect();
        defaults.sort();
        let count = defaults.len();
        defaults.dedup();
        assert_eq!(defaults.len(), count, "duplicate default bindings");
        assert!(conflicts(&BTreeMap::new()).is_empty());
    }

    #[test]
    fn macro_bindings_reject_command_and_macro_conflicts() {
        let overrides = BTreeMap::new();
        let mut macros = BTreeMap::new();
        macros.insert("one".to_string(), "ctrl+alt+1".to_string());
        assert!(validate_macro_binding("two", "ctrl+alt+1", &overrides, &macros).is_err());
        assert!(validate_macro_binding("one", "ctrl+alt+1", &overrides, &macros).is_ok());
        assert!(validate_macro_binding("two", "ctrl+s", &overrides, &macros).is_err());
        assert!(validate_macro_binding("two", "a", &overrides, &macros).is_err());
        assert_eq!(
            validate_macro_binding("two", "Ctrl+Alt+2", &overrides, &macros).unwrap(),
            "Ctrl+Alt+2"
        );
    }

    #[test]
    fn parses_and_formats_common_bindings() {
        assert_eq!(
            format_binding(&parse_binding("ctrl+shift+p").unwrap()),
            "Ctrl+Shift+P"
        );
        assert_eq!(format_binding(&parse_binding("F3").unwrap()), "F3");
        assert_eq!(
            format_binding(&parse_binding("shift+f3").unwrap()),
            "Shift+F3"
        );
        assert_eq!(format_binding(&parse_binding("ctrl++").unwrap()), "Ctrl++");
        assert_eq!(format_binding(&parse_binding("ctrl+-").unwrap()), "Ctrl+-");
        assert_eq!(format_binding(&parse_binding("ctrl+/").unwrap()), "Ctrl+/");
        assert_eq!(
            format_binding(&parse_binding("ctrl+tab").unwrap()),
            "Ctrl+Tab"
        );
    }

    #[test]
    fn rejects_invalid_bindings() {
        assert_eq!(parse_binding(""), Err(BindingError::Empty));
        assert!(matches!(
            parse_binding("hyper+x"),
            Err(BindingError::UnknownModifier(_))
        ));
        assert!(matches!(
            parse_binding("ctrl+"),
            Err(BindingError::MissingKey) | Err(BindingError::UnknownKey(_))
        ));
        assert!(matches!(
            parse_binding("ctrl+nonsense"),
            Err(BindingError::UnknownKey(_))
        ));
    }

    #[test]
    fn overrides_replace_defaults_and_suppress_the_old_key() {
        let mut overrides = BTreeMap::new();
        overrides.insert("file.save".to_string(), "ctrl+alt+s".to_string());
        let keymap = Keymap::build(&overrides);
        assert_eq!(keymap.lookup("s", true, false, true), Some("file.save"));
        assert_eq!(keymap.lookup("s", true, false, false), None);
        assert!(keymap.suppressed_binding("s", true, false, false).is_some());
    }

    #[test]
    fn detects_conflicts_and_validates_overrides() {
        let mut overrides = BTreeMap::new();
        overrides.insert("file.save".to_string(), "ctrl+o".to_string());
        let conflicts = conflicts(&overrides);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].binding, "Ctrl+O");

        let mut clean = BTreeMap::new();
        assert!(validate_override("file.save", "ctrl+shift+x", &clean).is_ok());
        assert!(matches!(
            validate_override("file.save", "ctrl+o", &clean),
            Err(BindingError::Duplicate(_))
        ));
        assert!(matches!(
            validate_override("unknown", "ctrl+x", &clean),
            Err(BindingError::UnknownCommand(_))
        ));
        assert!(matches!(
            validate_override("file.save", "a", &clean),
            Err(BindingError::SingleKey)
        ));
        assert!(validate_override("file.save", "f5", &clean).is_ok());
        clean.insert("file.save".to_string(), "ctrl+shift+x".to_string());
        assert!(validate_override("file.save", "ctrl+shift+x", &clean).is_ok());
    }
}

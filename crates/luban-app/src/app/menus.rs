use std::collections::HashMap;

use crate::plugin_host::{PluginCommand, PluginPanelSpec};

use super::*;

/// One plugin's entry in the Plugins menu: the manifest name plus the
/// commands and panels the plugin contributes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PluginMenuGroup {
    plugin_id: String,
    name: String,
    commands: Vec<PluginMenuCommand>,
    panels: Vec<PluginMenuPanel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PluginMenuCommand {
    id: &'static str,
    label: String,
    enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PluginMenuPanel {
    id: String,
    title: String,
}

/// Normalize a menu label for duplicate detection: lowercase and keep only
/// letters, digits and CJK characters, so "JSON: Preview" matches "JSON Preview".
fn normalize_menu_label(label: &str) -> String {
    label
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(|character| character.to_lowercase())
        .collect()
}

/// Group menu-contributed plugin commands and panels by plugin, preserving
/// command/panel order and skipping plugins with nothing to show. Panels whose
/// title duplicates a shown command label are skipped.
fn plugin_menu_groups(
    commands: &[PluginCommand],
    panels: &[PluginPanelSpec],
    command_context: &CommandContext,
    plugin_name: impl Fn(&str) -> Option<String>,
) -> Vec<PluginMenuGroup> {
    let mut groups: Vec<PluginMenuGroup> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut group_index = |plugin_id: &str, groups: &mut Vec<PluginMenuGroup>| {
        if let Some(index) = index.get(plugin_id) {
            return *index;
        }
        let group = groups.len();
        groups.push(PluginMenuGroup {
            plugin_id: plugin_id.to_string(),
            name: plugin_name(plugin_id).unwrap_or_else(|| plugin_id.to_string()),
            commands: Vec::new(),
            panels: Vec::new(),
        });
        index.insert(plugin_id.to_string(), group);
        group
    };

    for command in commands.iter().filter(|command| command.in_menu) {
        let group = group_index(&command.plugin_id, &mut groups);
        groups[group].commands.push(PluginMenuCommand {
            id: command.id,
            label: plugin_text(&command.label),
            enabled: command.when.enabled(command_context),
        });
    }
    for panel in panels {
        let group = group_index(&panel.plugin_id, &mut groups);
        let panel_title = plugin_text(&panel.title);
        let title = normalize_menu_label(&panel_title);
        if groups[group]
            .commands
            .iter()
            .any(|command| normalize_menu_label(&command.label) == title)
        {
            continue;
        }
        groups[group].panels.push(PluginMenuPanel {
            id: panel.id.clone(),
            title: panel_title,
        });
    }
    groups
}

impl LubanApp {
    // ------------------------------------------------------------------
    // Menus, window chrome and status bar
    // ------------------------------------------------------------------

    fn menu_item<F>(
        weak: &WeakEntity<Self>,
        label: impl Into<SharedString>,
        disabled: bool,
        checked: Option<bool>,
        action: F,
    ) -> PopupMenuItem
    where
        F: Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    {
        let weak = weak.clone();
        let mut item = PopupMenuItem::new(label.into()).disabled(disabled);
        if let Some(checked) = checked {
            item = item.checked(checked);
        }
        item.on_click(move |_, window, cx| {
            let _ = weak.update(cx, |this, cx| action(this, window, cx));
        })
    }

    /// Bound a menu to the height that fits below the menu row and let the
    /// rest scroll. Without a cap, a menu taller than the window is clamped to
    /// the top edge, covering the menu bar, while its last entries still run
    /// past the bottom edge and become unreachable.
    pub(super) fn fit_menu(menu: PopupMenu, window: &Window) -> PopupMenu {
        // Title bar/menu row (~40px), dropdown offset (6px) and the positioner's
        // viewport margin (8px), rounded up.
        let available = (window.viewport_size().height - px(64.)).max(px(120.));
        menu.scrollable(true).max_h(available)
    }

    fn render_menu_buttons(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.weak_entity();
        let settings = self.settings.clone();
        let recent = self.settings.recent_files.clone();
        let recent_workspaces = self.settings.recent_workspaces.clone();
        let large_file_open = self.large_file.open;
        let tab_count = self.tabs.len();
        let has_doc = tab_count > 0;
        let encoded = self
            .active_doc()
            .map(|d| d.encoding)
            .unwrap_or(TextEncoding::Utf8);
        let current_eol = self.active_doc().map(|d| d.eol).unwrap_or(Eol::Lf);
        let current_language = self
            .active_doc()
            .map(|d| d.language.clone())
            .unwrap_or_else(|| language::PLAIN_TEXT.to_string());
        let active_partial = self.active_doc().is_some_and(Document::is_partial);
        let active_huge = self.active_doc().is_some_and(Document::is_huge);
        let active_large = self.active_tab().is_some_and(|tab| tab.large_file);
        let active_non_text = self.active_doc().is_some_and(Document::is_non_text);
        let can_edit_full_text =
            has_doc && !active_huge && !active_partial && !active_large && !active_non_text;
        let closed_tab_count = self.closed_tabs.len();
        let can_go_back = self.nav.can_back();
        let can_go_forward = self.nav.can_forward();
        let diff_running = !self.diff_tasks.is_empty();

        let weak_file = weak.clone();
        let weak_edit = weak.clone();
        let weak_search = weak.clone();
        let weak_view = weak.clone();
        let weak_settings = weak.clone();
        let weak_help = weak.clone();
        let weak_plugins = weak.clone();
        let command_context = self.command_context();
        let plugin_groups = plugin_menu_groups(
            &self.plugin_host.commands,
            &self.plugin_host.panels,
            &command_context,
            |plugin_id| {
                self.plugin_host
                    .manifest(plugin_id)
                    .map(|manifest| plugin_text(&manifest.localized_name()))
            },
        );
        let recent_items = recent.clone();

        let file_menu = Button::new("menu-file")
            .label(tr("menu.file"))
            .ghost()
            .compact()
            .dropdown_menu(move |mut menu, window, _cx| {
                menu = menu
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.new"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("file.new", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.new-window"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("file.new-window", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.open"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("file.open", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.open-folder"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("file.open-folder", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.open-workspace"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("file.open-workspace", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.save-workspace"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("file.save-workspace", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.save-workspace-as"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("file.save-workspace-as", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.add-folder"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("file.add-folder", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.close-workspace"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("file.close-workspace", window, cx);
                        },
                    ))
                    .separator();
                if recent_items.is_empty() {
                    menu = menu.item(Self::menu_item(
                        &weak_file,
                        tr("menu.file.no_recent"),
                        true,
                        None,
                        |_, _, _| {},
                    ));
                } else {
                    for path in &recent_items {
                        let path = path.clone();
                        let label = tr_args(
                            "menu.file.recent",
                            &[(
                                "path",
                                truncate_middle(&path.display().to_string(), RECENT_PATH_COLUMNS),
                            )],
                        );
                        menu = menu.item(Self::menu_item(
                            &weak_file,
                            label,
                            false,
                            None,
                            move |this, window, cx| {
                                this.open_path(&path, window, cx);
                            },
                        ));
                    }
                }
                if !recent_workspaces.is_empty() {
                    menu = menu.separator();
                    for path in &recent_workspaces {
                        let path = path.clone();
                        let label = tr_args(
                            "menu.file.recent_workspace",
                            &[(
                                "path",
                                truncate_middle(&path.display().to_string(), RECENT_PATH_COLUMNS),
                            )],
                        );
                        menu = menu.item(Self::menu_item(
                            &weak_file,
                            label,
                            false,
                            None,
                            move |this, window, cx| {
                                this.open_workspace(&path, window, cx);
                            },
                        ));
                    }
                    menu = menu.item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.clear-recent-workspaces"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("file.clear-recent-workspaces", window, cx);
                        },
                    ));
                }
                menu = menu
                    .separator()
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.save"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("file.save", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.save-as"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("file.save-as", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.save-all"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("file.save-all", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.reload"),
                        !has_doc || (active_partial && !active_huge),
                        None,
                        |this, window, cx| {
                            this.run_command("file.reload", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.local-history"),
                        !has_doc || active_huge || active_partial,
                        None,
                        |this, window, cx| {
                            this.run_command("file.local-history", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.export-html"),
                        !has_doc || active_huge || active_partial,
                        None,
                        |this, window, cx| {
                            this.run_command("file.export-html", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.export-rtf"),
                        !has_doc || active_huge || active_partial,
                        None,
                        |this, window, cx| {
                            this.run_command("file.export-rtf", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.close"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("file.close", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.close-all"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("file.close-all", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("tabs.reopen-closed"),
                        closed_tab_count == 0,
                        None,
                        |this, window, cx| {
                            this.run_command("tabs.reopen-closed", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.exit"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("file.exit", window, cx);
                        },
                    ));
                Self::fit_menu(menu, window)
            });

        let edit_menu = Button::new("menu-edit")
            .label(tr("menu.edit"))
            .ghost()
            .compact()
            .dropdown_menu(move |menu, window, _cx| {
                let mut menu = menu
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.undo"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.undo", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.redo"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.redo", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.cut"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.cut", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.copy"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.copy", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.paste"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.paste", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.select-all"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.select-all", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.indent"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.indent", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.outdent"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.outdent", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.duplicate-line"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.duplicate-line", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.delete-line"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.delete-line", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.toggle-comment"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.toggle-comment", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.trim-whitespace"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.trim-whitespace", window, cx);
                        },
                    ))
                    .separator();
                menu = menu
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.uppercase"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.uppercase", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.lowercase"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.lowercase", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.title-case"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.title-case", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.sort-asc"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.sort-asc", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.sort-desc"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.sort-desc", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.sort-ignore-case-asc"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.sort-ignore-case-asc", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.sort-ignore-case-desc"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.sort-ignore-case-desc", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.sort-numeric-asc"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.sort-numeric-asc", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.sort-numeric-desc"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.sort-numeric-desc", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.sort-by-column"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.sort-by-column", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.filter-lines"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.filter-lines", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.move-line-up"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.move-line-up", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.move-line-down"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.move-line-down", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.join-lines"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.join-lines", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.split-lines"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.split-lines", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.remove-duplicate-lines"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.remove-duplicate-lines", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.remove-empty-lines"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.remove-empty-lines", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("bookmarks.cut-lines"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("bookmarks.cut-lines", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("bookmarks.copy-lines"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("bookmarks.copy-lines", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("bookmarks.delete-lines"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("bookmarks.delete-lines", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.column-editor"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.column-editor", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.select-next-occurrence"),
                        !can_edit_full_text,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.select-next-occurrence", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.select-all-occurrences"),
                        !can_edit_full_text,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.select-all-occurrences", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.add-cursor-above"),
                        !can_edit_full_text,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.add-cursor-above", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.add-cursor-below"),
                        !can_edit_full_text,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.add-cursor-below", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.insert-snippet"),
                        !can_edit_full_text,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.insert-snippet", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.unfold-current"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.unfold-current", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.unfold-all"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.unfold-all", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("macro.toggle-recording"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("macro.toggle-recording", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("macro.play-last"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("macro.play-last", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("macro.save-recording"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("macro.save-recording", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("macro.play-saved"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("macro.play-saved", window, cx);
                        },
                    ))
                    .separator();
                for eol in Eol::ALL {
                    let label = tr_args("menu.edit.eol", &[("eol", eol.long_label())]);
                    menu = menu.item(Self::menu_item(
                        &weak_edit,
                        label,
                        !has_doc,
                        Some(current_eol == eol),
                        move |this, _, cx| this.convert_eol(eol, cx),
                    ));
                }
                menu = menu.separator();
                for encoding in TextEncoding::ALL {
                    let label = tr_args("menu.edit.encoding", &[("encoding", encoding.label())]);
                    menu = menu.item(Self::menu_item(
                        &weak_edit,
                        label,
                        !has_doc,
                        Some(encoded == encoding),
                        move |this, _, cx| this.set_encoding(encoding, cx),
                    ));
                }
                Self::fit_menu(menu, window)
            });

        let search_menu = Button::new("menu-search")
            .label(tr("menu.search"))
            .ghost()
            .compact()
            .dropdown_menu(move |menu, _window, _cx| {
                menu.item(Self::menu_item(
                    &weak_search,
                    Self::command_label("search.find"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.run_command("search.find", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("search.replace"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.run_command("search.replace", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("search.find-next"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.run_command("search.find-next", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("search.find-previous"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.run_command("search.find-previous", window, cx);
                    },
                ))
                .separator()
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("search.go-to-line"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.run_command("search.go-to-line", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("search.find-in-files"),
                    false,
                    None,
                    |this, window, cx| {
                        this.run_command("search.find-in-files", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("search.workspace-replace"),
                    false,
                    None,
                    |this, window, cx| {
                        this.run_command("search.workspace-replace", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("search.undo-workspace-replace"),
                    false,
                    None,
                    |this, window, cx| {
                        this.run_command("search.undo-workspace-replace", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("search.clear-highlights"),
                    false,
                    None,
                    |this, window, cx| {
                        this.run_command("search.clear-highlights", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("search.mark-all"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.run_command("search.mark-all", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("search.clear-history"),
                    false,
                    None,
                    |this, window, cx| {
                        this.run_command("search.clear-history", window, cx);
                    },
                ))
                .separator()
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("bookmarks.toggle"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.run_command("bookmarks.toggle", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("bookmarks.next"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.run_command("bookmarks.next", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("bookmarks.previous"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.run_command("bookmarks.previous", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("bookmarks.clear"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.run_command("bookmarks.clear", window, cx);
                    },
                ))
            });

        let view_menu = Button::new("menu-view")
            .label(tr("menu.view"))
            .ghost()
            .compact()
            .dropdown_menu(move |menu, window, _cx| {
                let mut menu = menu
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.word-wrap"),
                        false,
                        Some(settings.word_wrap),
                        |this, window, cx| {
                            this.settings.word_wrap = !this.settings.word_wrap;
                            this.apply_settings(window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.line-numbers"),
                        false,
                        Some(settings.show_line_numbers),
                        |this, window, cx| {
                            this.settings.show_line_numbers = !this.settings.show_line_numbers;
                            this.apply_settings(window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.toolbar"),
                        false,
                        Some(settings.show_toolbar),
                        |this, _window, cx| {
                            this.settings.show_toolbar = !this.settings.show_toolbar;
                            this.commit_settings();
                            cx.notify();
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.status-bar"),
                        false,
                        Some(settings.show_status_bar),
                        |this, _window, cx| {
                            this.settings.show_status_bar = !this.settings.show_status_bar;
                            this.commit_settings();
                            cx.notify();
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.explorer"),
                        false,
                        Some(settings.show_file_explorer),
                        |this, _window, cx| {
                            this.settings.show_file_explorer = !this.settings.show_file_explorer;
                            this.commit_settings();
                            cx.notify();
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.large-file-controller"),
                        !has_doc,
                        Some(large_file_open),
                        |this, window, cx| {
                            this.toggle_large_file_controller(window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.whitespace"),
                        false,
                        Some(settings.show_whitespace),
                        |this, window, cx| {
                            this.run_command("view.whitespace", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.indent-guides"),
                        false,
                        Some(settings.show_indent_guides),
                        |this, window, cx| {
                            this.run_command("view.indent-guides", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.ruler"),
                        false,
                        Some(settings.show_ruler),
                        |this, window, cx| {
                            this.run_command("view.ruler", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.document-map"),
                        false,
                        Some(settings.show_document_map),
                        |this, window, cx| {
                            this.run_command("view.document-map", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("diff.cancel"),
                        !diff_running,
                        None,
                        |this, window, cx| {
                            this.run_command("diff.cancel", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.bracket-colors"),
                        false,
                        Some(settings.bracket_colors),
                        |this, window, cx| {
                            this.run_command("view.bracket-colors", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("navigation.toggle-outline"),
                        false,
                        Some(settings.show_outline),
                        |this, window, cx| {
                            this.run_command("navigation.toggle-outline", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("navigation.toggle-breadcrumbs"),
                        false,
                        Some(settings.show_breadcrumbs),
                        |this, window, cx| {
                            this.run_command("navigation.toggle-breadcrumbs", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("navigation.back"),
                        !can_go_back,
                        None,
                        |this, window, cx| {
                            this.run_command("navigation.back", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("navigation.forward"),
                        !can_go_forward,
                        None,
                        |this, window, cx| {
                            this.run_command("navigation.forward", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("window.split-right"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("window.split-right", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("window.split-down"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("window.split-down", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("window.unsplit"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("window.unsplit", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("window.switch-group"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("window.switch-group", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("window.move-tab-to-other-group"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("window.move-tab-to-other-group", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("window.move-tab-to-new-window"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("window.move-tab-to-new-window", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.zoom-in"),
                        !has_doc,
                        None,
                        |this, _, cx| this.zoom(1.0, cx),
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.zoom-out"),
                        !has_doc,
                        None,
                        |this, _, cx| this.zoom(-1.0, cx),
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.zoom-reset"),
                        !has_doc,
                        None,
                        |this, _, cx| this.reset_zoom(cx),
                    ))
                    .separator();
                menu = menu
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.theme-light"),
                        false,
                        Some(settings.theme == ThemeChoice::Light),
                        |this, window, cx| {
                            this.settings.theme = ThemeChoice::Light;
                            this.apply_settings(window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.theme-dark"),
                        false,
                        Some(settings.theme == ThemeChoice::Dark),
                        |this, window, cx| {
                            this.settings.theme = ThemeChoice::Dark;
                            this.apply_settings(window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.theme-system"),
                        false,
                        Some(settings.theme == ThemeChoice::System),
                        |this, window, cx| {
                            this.settings.theme = ThemeChoice::System;
                            this.apply_settings(window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("palette.commands"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("palette.commands", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("palette.quick-open"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("palette.quick-open", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.fullscreen"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("view.fullscreen", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.choose-language"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.open_language_picker(window, cx);
                        },
                    ));
                for name in language::names() {
                    let selected = current_language == name;
                    let name = name.to_string();
                    menu = menu.item(Self::menu_item(
                        &weak_view,
                        tr_args(
                            "menu.view.language",
                            &[("language", language_display_name(&name))],
                        ),
                        !has_doc,
                        Some(selected),
                        move |this, window, cx| this.set_language(name.clone(), window, cx),
                    ));
                }
                Self::fit_menu(menu, window)
            });

        let settings_menu = Button::new("menu-settings")
            .label(tr("menu.settings"))
            .ghost()
            .compact()
            .dropdown_menu(move |menu, _window, _cx| {
                menu.item(Self::menu_item(
                    &weak_settings,
                    Self::command_label("settings.preferences"),
                    false,
                    None,
                    |this, window, cx| this.open_preferences(window, cx),
                ))
                .separator()
                .item(Self::menu_item(
                    &weak_settings,
                    Self::command_label("settings.import"),
                    false,
                    None,
                    |this, window, cx| {
                        this.run_command("settings.import", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_settings,
                    Self::command_label("settings.export"),
                    false,
                    None,
                    |this, window, cx| {
                        this.run_command("settings.export", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_settings,
                    Self::command_label("settings.reset"),
                    false,
                    None,
                    |this, window, cx| {
                        this.run_command("settings.reset", window, cx);
                    },
                ))
            });

        let help_menu = Button::new("menu-help")
            .label(tr("menu.help"))
            .ghost()
            .compact()
            .dropdown_menu(move |menu, _window, _cx| {
                menu.item(Self::menu_item(
                    &weak_help,
                    Self::command_label("help.shortcuts"),
                    false,
                    None,
                    |this, _, cx| {
                        this.shortcuts_open = true;
                        cx.notify();
                    },
                ))
                .item(Self::menu_item(
                    &weak_help,
                    Self::command_label("help.open-log-dir"),
                    false,
                    None,
                    |this, window, cx| {
                        this.run_command("help.open-log-dir", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_help,
                    Self::command_label("help.about"),
                    false,
                    None,
                    |this, _, cx| {
                        this.about_open = true;
                        cx.notify();
                    },
                ))
            });

        let plugin_menu = Button::new("menu-plugins")
            .label(tr("menu.plugins"))
            .ghost()
            .compact()
            .dropdown_menu(move |mut menu, window, cx| {
                menu = menu.item(Self::menu_item(
                    &weak_plugins,
                    Self::command_label("plugins.manage"),
                    false,
                    None,
                    |this, window, cx| {
                        this.run_command("plugins.manage", window, cx);
                    },
                ));
                menu = menu.item(Self::menu_item(
                    &weak_plugins,
                    Self::command_label("plugins.store"),
                    false,
                    None,
                    |this, window, cx| {
                        this.run_command("plugins.store", window, cx);
                    },
                ));
                if !plugin_groups.is_empty() {
                    menu = menu.separator();
                    for group in &plugin_groups {
                        let plugin_id = group.plugin_id.clone();
                        let commands = group.commands.clone();
                        let panels = group.panels.clone();
                        let weak_submenu = weak_plugins.clone();
                        menu = menu.submenu(
                            group.name.clone(),
                            window,
                            cx,
                            move |mut submenu, _window, _cx| {
                                for command in &commands {
                                    let weak_item = weak_submenu.clone();
                                    let id = command.id;
                                    let enabled = command.enabled;
                                    submenu = submenu.item(Self::menu_item(
                                        &weak_item,
                                        command.label.clone(),
                                        !enabled,
                                        None,
                                        move |this, window, cx| {
                                            this.run_command(id, window, cx);
                                        },
                                    ));
                                }
                                if !commands.is_empty() && !panels.is_empty() {
                                    submenu = submenu.separator();
                                }
                                for panel in &panels {
                                    let weak_item = weak_submenu.clone();
                                    let plugin_id = plugin_id.clone();
                                    let panel_id = panel.id.clone();
                                    submenu = submenu.item(Self::menu_item(
                                        &weak_item,
                                        panel.title.clone(),
                                        false,
                                        None,
                                        move |this, window, cx| {
                                            this.open_plugin_panel(
                                                &plugin_id, &panel_id, window, cx,
                                            );
                                        },
                                    ));
                                }
                                submenu
                            },
                        );
                    }
                    menu = menu.separator();
                }
                menu.item(Self::menu_item(
                    &weak_plugins,
                    Self::command_label("plugins.reload"),
                    false,
                    None,
                    |this, window, cx| {
                        this.run_command("plugins.reload", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_plugins,
                    Self::command_label("packages.reload"),
                    false,
                    None,
                    |this, window, cx| {
                        this.run_command("packages.reload", window, cx);
                    },
                ))
            });

        div()
            .flex()
            .items_center()
            .gap_1()
            .child(file_menu)
            .child(edit_menu)
            .child(search_menu)
            .child(view_menu)
            .child(settings_menu)
            .child(plugin_menu)
            .child(help_menu)
            .into_any_element()
    }

    /// Standalone menu row used when the platform falls back to server-side
    /// decorations and the application must not draw its own title bar.
    pub(super) fn render_menu_bar(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let menus = self.render_menu_buttons(cx);
        div()
            .flex()
            .items_center()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().colors.border)
            .bg(cx.theme().colors.background)
            .child(menus)
            .into_any_element()
    }

    /// The application title bar owns the window chrome: application name,
    /// menus, and (on Windows/Linux) the window control buttons. The document
    /// name stays in the tab bar; the native window title is set separately.
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    pub(super) fn render_title_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let menus = self.render_menu_buttons(cx);
        let weak = cx.weak_entity();

        #[cfg(target_os = "linux")]
        let manual_controls = self
            .manual_window_chrome
            .then(|| Self::render_manual_window_controls(&weak, window, cx));
        #[cfg(not(target_os = "linux"))]
        let manual_controls: Option<AnyElement> = None;

        TitleBar::new()
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w(px(0.))
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child("Luban Notepad"),
                    )
                    .child(div().flex_shrink_0().child(menus)),
            )
            .when_some(manual_controls, |title_bar, controls| {
                title_bar.child(controls)
            })
            // `on_close_window` only takes effect on Linux, where the custom
            // close button must go through the same unsaved-changes flow as
            // the window manager's close request.
            .on_close_window(move |_, window, cx| {
                let should_close = weak
                    .update(cx, |this, cx| this.on_should_close(window, cx))
                    .unwrap_or(true);
                if should_close {
                    cx.quit();
                }
            })
            .into_any_element()
    }

    /// Window controls for the manually decorated X11 window. `TitleBar` omits
    /// its own controls whenever the platform reports server decorations, which
    /// is the state the window is created in to avoid the shadow margin, so the
    /// application draws them with the same sizes and hover colours.
    #[cfg(target_os = "linux")]
    fn render_manual_window_controls(
        weak: &gpui::WeakEntity<Self>,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        use gpui_kit::component::{
            Icon, IconName as ComponentIcon, Sizable as _, TITLE_BAR_HEIGHT,
        };

        #[derive(Clone, Copy)]
        enum Action {
            Minimize,
            Maximize,
            Close,
        }

        let supported = window.window_controls();
        let theme = cx.theme();
        let foreground = theme.colors.foreground;

        let control = |id: &'static str, icon: ComponentIcon, action: Action, close: bool| {
            let weak = weak.clone();
            let hover_bg = if close {
                theme.danger
            } else {
                theme.colors.secondary_hover
            };
            let hover_fg = if close {
                theme.danger_foreground
            } else {
                theme.colors.secondary_foreground
            };

            div()
                .id(id)
                .flex()
                .w(TITLE_BAR_HEIGHT)
                .h_full()
                .flex_shrink_0()
                .justify_center()
                .items_center()
                .text_color(foreground)
                .hover(move |style| style.bg(hover_bg).text_color(hover_fg))
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    match action {
                        Action::Minimize => window.minimize_window(),
                        Action::Maximize => window.zoom_window(),
                        Action::Close => {
                            let should_close = weak
                                .update(cx, |this, cx| this.on_should_close(window, cx))
                                .unwrap_or(true);
                            if should_close {
                                cx.quit();
                            }
                        }
                    }
                })
                .child(Icon::new(icon).small())
        };

        let mut controls = div().flex().flex_shrink_0().items_center().h_full();
        if supported.minimize {
            controls = controls.child(control(
                "window-control-minimize",
                ComponentIcon::WindowMinimize,
                Action::Minimize,
                false,
            ));
        }
        if supported.maximize {
            let icon = if window.is_maximized() {
                ComponentIcon::WindowRestore
            } else {
                ComponentIcon::WindowMaximize
            };
            controls = controls.child(control(
                "window-control-maximize",
                icon,
                Action::Maximize,
                false,
            ));
        }
        controls
            .child(control(
                "window-control-close",
                ComponentIcon::WindowClose,
                Action::Close,
                true,
            ))
            .into_any_element()
    }

    // ------------------------------------------------------------------
    // Toolbar / tabs / status
    // ------------------------------------------------------------------

    pub(super) fn render_toolbar(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let weak = cx.weak_entity();
        let wrap_enabled =
            self.settings.word_wrap && !self.active_tab().is_some_and(|tab| tab.large_file);
        let (theme_icon, theme_label) = if self.settings.theme == ThemeChoice::Light {
            (IconName::Moon, Self::command_label("view.theme-dark"))
        } else {
            (IconName::Sun, Self::command_label("view.theme-light"))
        };

        let mut file_group = ToolbarGroup::new("tb-file");
        let mut history_group = ToolbarGroup::new("tb-history");
        let mut search_group = ToolbarGroup::new("tb-search");
        let mut view_group = ToolbarGroup::new("tb-view");
        macro_rules! tool {
            ($group:ident, $id:literal, $icon:expr, $label:expr, $action:expr) => {{
                let weak = weak.clone();
                let label = $label;
                $group = $group.child(
                    Button::new($id)
                        .icon($icon)
                        .tooltip(label.clone())
                        .accessibility_label(label)
                        .on_click(move |_, window, cx| {
                            let _ = weak.update(cx, |this, cx| $action(this, window, cx));
                        }),
                );
            }};
        }
        tool!(
            file_group,
            "tb-new",
            IconName::FilePlus,
            Self::command_label("file.new"),
            |this: &mut Self, window, cx| {
                this.run_command("file.new", window, cx);
            }
        );
        tool!(
            file_group,
            "tb-open",
            IconName::FolderOpen,
            Self::command_label("file.open"),
            |this: &mut Self, window, cx| {
                this.run_command("file.open", window, cx);
            }
        );
        tool!(
            file_group,
            "tb-save",
            IconName::Save,
            Self::command_label("file.save"),
            |this: &mut Self, window, cx| {
                this.run_command("file.save", window, cx);
            }
        );
        tool!(
            history_group,
            "tb-undo",
            IconName::Undo,
            Self::command_label("edit.undo"),
            |this: &mut Self, window, cx| {
                this.run_command("edit.undo", window, cx);
            }
        );
        tool!(
            history_group,
            "tb-redo",
            IconName::Redo,
            Self::command_label("edit.redo"),
            |this: &mut Self, window, cx| {
                this.run_command("edit.redo", window, cx);
            }
        );
        tool!(
            search_group,
            "tb-find",
            IconName::Search,
            Self::command_label("search.find"),
            |this: &mut Self, window, cx| {
                this.run_command("search.find", window, cx);
            }
        );
        tool!(
            search_group,
            "tb-replace",
            IconName::Replace,
            Self::command_label("search.replace"),
            |this: &mut Self, window, cx| {
                this.run_command("search.replace", window, cx);
            }
        );
        tool!(
            search_group,
            "tb-findfiles",
            IconName::FolderSearch,
            Self::command_label("search.find-in-files"),
            |this: &mut Self, window, cx| {
                this.run_command("search.find-in-files", window, cx);
            }
        );
        let weak_wrap = weak.clone();
        let wrap_label = Self::command_label("view.word-wrap");
        view_group = view_group.child(
            Button::new("tb-wrap")
                .icon(IconName::TextWrap)
                .selected(wrap_enabled)
                .toggled(wrap_enabled)
                .tooltip(wrap_label.clone())
                .accessibility_label(wrap_label)
                .on_click(move |_, window, cx| {
                    let _ = weak_wrap.update(cx, |this, cx| {
                        this.run_command("view.word-wrap", window, cx);
                    });
                }),
        );
        tool!(
            view_group,
            "tb-theme",
            theme_icon,
            theme_label,
            |this: &mut Self, window, cx| {
                this.run_command("view.toggle-theme", window, cx);
            }
        );

        let border = cx.theme().colors.border;
        let background = cx.theme().colors.background;
        let separator = || {
            div()
                .w(px(1.))
                .h(px(20.))
                .bg(border)
                .mx_1()
                .into_any_element()
        };
        Toolbar::new("app-toolbar")
            .child(file_group)
            .content(separator())
            .child(history_group)
            .content(separator())
            .child(search_group)
            .content(separator())
            .child(view_group)
            .border_b_1()
            .border_color(border)
            .bg(background)
            .px_2()
            .into_any_element()
    }

    pub(super) fn render_status_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.weak_entity();
        let left_plugin: Vec<(String, Option<&'static str>)> = self
            .plugin_host
            .status_items(StatusAlignment::Left)
            .into_iter()
            .map(|item| {
                let text = self
                    .plugin_host
                    .dynamic_status_text(&item.plugin_id, &item.id)
                    .map(str::to_string)
                    .unwrap_or_else(|| plugin_text(&item.text));
                (text, item.command)
            })
            .collect();
        let right_plugin: Vec<(String, Option<&'static str>)> = self
            .plugin_host
            .status_items(StatusAlignment::Right)
            .into_iter()
            .map(|item| {
                let text = self
                    .plugin_host
                    .dynamic_status_text(&item.plugin_id, &item.id)
                    .map(str::to_string)
                    .unwrap_or_else(|| plugin_text(&item.text));
                (text, item.command)
            })
            .collect();
        let plugin_item = |weak: WeakEntity<Self>, text: String, command: Option<&'static str>| {
            let item = div().child(text);
            if let Some(command) = command {
                let weak_right = weak.clone();
                item.on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.run_command(command, window, cx);
                    });
                })
                .on_mouse_down(MouseButton::Right, move |_, window, cx| {
                    let _ = weak_right.update(cx, |this, cx| {
                        this.run_command(command, window, cx);
                    });
                })
            } else {
                item
            }
        };
        let left_items = left_plugin
            .into_iter()
            .map(|(text, command)| plugin_item(weak.clone(), text, command));
        let right_items = right_plugin
            .into_iter()
            .map(|(text, command)| plugin_item(weak.clone(), text, command));
        let (mut line, mut column) = (1, 1);
        let (mut sel_len, mut chars, mut lines) = (0, 0, 0);
        let mut large_file = false;
        let mut huge_label = String::new();
        let mut window_label = String::new();
        let mut save_label = String::new();
        if let Some(tab) = self.active_tab() {
            large_file = tab.large_file;
            if let Some(ui) = &tab.huge {
                let state = tab.editor.read(cx);
                let rope = state.text();
                chars = tab.doc.char_count();
                lines = tab.doc.line_count();
                let _ = rope;
                let (global_line, global_column) = tab
                    .doc
                    .huge()
                    .map(|huge| (huge.cursor_line, huge.cursor_column))
                    .unwrap_or((1, 1));
                line = usize::try_from(global_line).unwrap_or(usize::MAX);
                column = usize::try_from(global_column).unwrap_or(usize::MAX);
                let range = state.selected_range();
                let start = rope.byte_to_char_idx(range.start.min(rope.len()));
                let end = rope.byte_to_char_idx(range.end.min(rope.len()));
                sel_len = end.saturating_sub(start);
                huge_label = tr("status.huge_file");
                window_label = tr_args(
                    "status.huge_window",
                    &[
                        ("start", ui.window_line.to_string()),
                        ("end", ui.window_end_line().to_string()),
                        ("total", lines.to_string()),
                    ],
                );
                if ui.starts_mid_line {
                    window_label.push_str(&tr("status.huge_continuation"));
                }
                if let Some(save) = &ui.save {
                    let total = save.total.max(1);
                    let percent = (save
                        .progress
                        .load(std::sync::atomic::Ordering::Relaxed)
                        .saturating_mul(100)
                        / total)
                        .min(100);
                    save_label = tr_args(
                        "status.huge_saving_percent",
                        &[("percent", percent.to_string())],
                    );
                } else if let Some(search) = &ui.search {
                    let percent = search
                        .progress
                        .load(std::sync::atomic::Ordering::Relaxed)
                        .min(search.total)
                        .saturating_mul(100)
                        .checked_div(search.total)
                        .unwrap_or(0);
                    save_label = match search.kind {
                        SearchKind::ReplaceAll => tr("find.huge_replace_all_running"),
                        _ => tr_args(
                            "find.huge_searching_percent",
                            &[("percent", percent.to_string())],
                        ),
                    };
                }
            } else {
                let state = tab.editor.read(cx);
                let rope = state.text();
                chars = rope.len_chars();
                lines = rope.lines_len();
                let position = rope.offset_to_position(state.cursor());
                line = position.line as usize + 1;
                column = position.character as usize + 1;
                let range = state.selected_range();
                let start = rope.byte_to_char_idx(range.start.min(rope.len()));
                let end = rope.byte_to_char_idx(range.end.min(rope.len()));
                sel_len = end.saturating_sub(start);
            }
        }
        let eol_label = self.active_doc().map(|d| d.eol.label()).unwrap_or("LF");
        let encoding = self
            .active_doc()
            .map(|d| d.encoding.short_label())
            .unwrap_or("UTF-8");
        let language = self
            .active_doc()
            .map(|d| d.language.clone())
            .unwrap_or_else(|| language::PLAIN_TEXT.to_string());
        let language = language_display_name(&language);
        let zoom = (self.settings.font_size / 14.0 * 100.0).round() as i32;
        let modified = self.active_doc().is_some_and(Document::is_modified);
        let non_text = self.active_doc().is_some_and(Document::is_non_text);
        let partial_label = self
            .active_doc()
            .and_then(Document::partial_range)
            .map(|range| {
                let unit = self.settings.large_file_display_unit;
                tr_args(
                    "status.partial_file",
                    &[
                        (
                            "loaded",
                            format_bytes(range.end.saturating_sub(range.start), unit),
                        ),
                        ("total", format_bytes(range.total, unit)),
                        ("start", format_bytes(range.start, unit)),
                    ],
                )
            })
            .unwrap_or_default();
        let line_col = tr_args(
            "status.line_col",
            &[("line", line.to_string()), ("column", column.to_string())],
        );
        let selection = tr_args("status.selection", &[("count", sel_len.to_string())]);
        let chars_label = if chars == 1 {
            tr("status.chars.one")
        } else {
            tr_args("status.chars.other", &[("count", chars.to_string())])
        };
        let lines_label = if lines == 1 {
            tr("status.lines.one")
        } else {
            tr_args("status.lines.other", &[("count", lines.to_string())])
        };
        let state_label = if modified {
            tr("status.modified")
        } else {
            tr("status.saved_state")
        };
        let zoom_label = tr_args("status.zoom", &[("percent", zoom.to_string())]);
        let unit = self.settings.large_file_display_unit;
        let size_label = match self.active_doc() {
            Some(doc) if doc.path.is_some() => tr_args(
                "status.file_size",
                &[("size", format_bytes(doc.file_size, unit))],
            ),
            Some(_) => tr("status.file_size_unavailable"),
            None => String::new(),
        };
        let words = self.active_tab_mut().and_then(|tab| {
            if tab.large_file || tab.doc.is_huge() || tab.doc.is_partial() || tab.doc.is_non_text()
            {
                return None;
            }
            let revision = tab.doc.revision();
            match tab.word_count {
                Some((cached, count)) if cached == revision => Some(count),
                _ => {
                    let count = count_words(&tab.doc.text);
                    tab.word_count = Some((revision, count));
                    Some(count)
                }
            }
        });
        let words_label = match words {
            Some(count) => tr_args("status.words", &[("count", count.to_string())]),
            None if self.active_doc().is_some() => tr("status.words_unavailable"),
            None => String::new(),
        };
        let (indent_size, indent_spaces) = self
            .active_tab()
            .map(|tab| {
                if Self::language_indent_applies(&tab.doc, tab.large_file) {
                    (
                        self.settings.tab_size_for(&tab.doc.language),
                        self.settings.use_spaces_for(&tab.doc.language),
                    )
                } else {
                    (self.settings.tab_size, self.settings.use_spaces)
                }
            })
            .unwrap_or((self.settings.tab_size, self.settings.use_spaces));
        let indent_label = if indent_spaces {
            tr_args("status.indent_spaces", &[("size", indent_size.to_string())])
        } else {
            tr_args("status.indent_tab", &[("size", indent_size.to_string())])
        };
        let bookmark_label = self
            .active_tab()
            .filter(|tab| !tab.bookmarks.is_empty())
            .map(|tab| {
                tr_args(
                    "status.bookmarks",
                    &[("count", tab.bookmarks.len().to_string())],
                )
            })
            .unwrap_or_default();
        let macro_label = if self.macro_recording {
            tr_args(
                "status.macro_recording_count",
                &[("count", self.macro_recorded.len().to_string())],
            )
        } else {
            String::new()
        };
        let read_only_label = if self.active_doc().is_some_and(|doc| doc.read_only) {
            tr("status.read_only")
        } else {
            String::new()
        };

        div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(24.))
            .px_3()
            .border_t_1()
            .border_color(cx.theme().colors.border)
            .bg(cx.theme().colors.secondary)
            .text_xs()
            .text_color(cx.theme().colors.muted_foreground)
            .child(div().flex().items_center().gap_4().children([
                div().child(line_col),
                div().child(selection),
                div().child(chars_label),
                div().child(lines_label),
                div().child(size_label),
                div().child(words_label),
                div().child(indent_label),
                div().child(bookmark_label),
                div().child(macro_label),
                div().child(read_only_label),
                div().child(if large_file {
                    tr("status.large_file")
                } else {
                    String::new()
                }),
                div().child(huge_label),
                div().child(window_label),
                div().child(save_label),
                div().child(partial_label),
                div().child(if non_text {
                    tr("status.non_text")
                } else {
                    String::new()
                }),
                div().child(state_label),
                div().child(if self.status.is_empty() {
                    String::new()
                } else {
                    self.status.clone()
                }),
            ]))
            .children(left_items)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .children([
                        div().child(eol_label),
                        div().child(encoding),
                        div().child(language),
                        div().child(zoom_label),
                    ])
                    .children(right_items),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::plugin_menu_groups;
    use crate::commands::{CommandContext, CommandWhen};
    use crate::plugin_host::{PluginCommand, PluginPanelSpec};
    use luban_plugin::localized::LocalizedText;
    use luban_plugin::manifest::{PanelPlacement, PanelRefresh};

    fn command(plugin_id: &str, id: &'static str, in_menu: bool) -> PluginCommand {
        PluginCommand {
            id,
            label: LocalizedText::new(format!("{plugin_id}: {id}")),
            plugin_id: plugin_id.to_string(),
            entry: "main".to_string(),
            when: CommandWhen::Always,
            default_binding: None,
            in_menu,
        }
    }

    fn panel(plugin_id: &str, id: &str, title: &str) -> PluginPanelSpec {
        PluginPanelSpec {
            plugin_id: plugin_id.to_string(),
            id: id.to_string(),
            title: LocalizedText::new(title),
            entry: "main".to_string(),
            placement: PanelPlacement::Right,
            width: None,
            refresh: PanelRefresh::Manual,
        }
    }

    fn context() -> CommandContext {
        CommandContext {
            has_document: true,
            has_path: false,
            can_save: false,
            can_undo: false,
            can_redo: false,
            can_edit_full_text: true,
            can_go_back: false,
            can_go_forward: false,
            diff_running: false,
        }
    }

    #[test]
    fn groups_contributed_commands_and_panels_by_plugin() {
        let commands = vec![
            command("json", "json.preview", true),
            command("json", "json.fold", false),
            command("markdown", "markdown.preview", true),
        ];
        let panels = vec![
            panel("json", "json.preview.panel", "JSON Preview"),
            panel("markdown", "markdown.preview.panel", "Markdown Preview"),
        ];
        let groups = plugin_menu_groups(
            &commands,
            &panels,
            &context(),
            |plugin_id| match plugin_id {
                "json" => Some("JSON Preview".to_string()),
                "markdown" => Some("Markdown Preview".to_string()),
                _ => None,
            },
        );

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].plugin_id, "json");
        assert_eq!(groups[0].name, "JSON Preview");
        assert_eq!(
            groups[0]
                .commands
                .iter()
                .map(|command| command.id)
                .collect::<Vec<_>>(),
            ["json.preview"]
        );
        assert_eq!(groups[0].panels[0].title, "JSON Preview");
        assert_eq!(groups[1].plugin_id, "markdown");
        assert_eq!(groups[1].name, "Markdown Preview");
        assert_eq!(groups[1].commands[0].id, "markdown.preview");
        assert_eq!(groups[1].panels[0].title, "Markdown Preview");
    }

    #[test]
    fn skips_plugins_without_menu_entries_and_keeps_panel_only_plugins() {
        let commands = vec![command("empty", "empty.hidden", false)];
        let panels = vec![panel("panels-only", "panels-only.panel", "Panels Only")];
        let groups = plugin_menu_groups(&commands, &panels, &context(), |_| None);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].plugin_id, "panels-only");
        assert_eq!(groups[0].name, "panels-only");
        assert!(groups[0].commands.is_empty());
        assert_eq!(groups[0].panels[0].title, "Panels Only");
    }

    #[test]
    fn plugin_menu_labels_follow_the_active_locale() {
        let _guard = crate::i18n::test_lock();
        let mut preview = command("json", "json.preview", true);
        preview.label = LocalizedText::with_translations(
            "JSON: Preview",
            [("zh-CN".to_string(), "JSON：预览".to_string())],
        );
        let panels = vec![PluginPanelSpec {
            plugin_id: "json".to_string(),
            id: "json.inspector.panel".to_string(),
            title: LocalizedText::with_translations(
                "JSON Inspector",
                [("zh-CN".to_string(), "JSON 检查器".to_string())],
            ),
            entry: "main".to_string(),
            placement: PanelPlacement::Right,
            width: None,
            refresh: PanelRefresh::Manual,
        }];
        let name = LocalizedText::with_translations(
            "JSON Preview",
            [("zh-CN".to_string(), "JSON 预览".to_string())],
        );

        rust_i18n::set_locale("zh-CN");
        let groups = plugin_menu_groups(&[preview.clone()], &panels, &context(), |_| {
            Some(crate::i18n::plugin_text(&name))
        });
        assert_eq!(groups[0].commands[0].label, "JSON：预览");
        assert_eq!(groups[0].panels[0].title, "JSON 检查器");

        rust_i18n::set_locale("en");
        let groups = plugin_menu_groups(&[preview], &panels, &context(), |_| {
            Some(crate::i18n::plugin_text(&name))
        });
        assert_eq!(groups[0].name, "JSON Preview");
        assert_eq!(groups[0].commands[0].label, "JSON: Preview");
        assert_eq!(groups[0].panels[0].title, "JSON Inspector");
    }

    #[test]
    fn command_enabled_state_follows_when_condition() {
        let mut save = command("demo", "demo.save", true);
        save.when = CommandWhen::CanSave;
        let groups = plugin_menu_groups(&[save], &[], &context(), |_| Some("Demo".to_string()));

        assert!(!groups[0].commands[0].enabled);
    }

    #[test]
    fn hides_panels_that_duplicate_shown_command_labels() {
        let mut json_preview = command("json", "json.preview", true);
        json_preview.label = LocalizedText::new("JSON: Preview");
        let mut markdown_preview = command("markdown", "markdown.preview", true);
        markdown_preview.label = LocalizedText::new("Markdown: Preview");
        let commands = vec![json_preview, markdown_preview];
        let panels = vec![
            panel("json", "json.preview.panel", "JSON Preview"),
            panel("json", "json.inspector.panel", "JSON Inspector"),
            panel("markdown", "markdown.preview.panel", "Markdown Preview"),
        ];
        let groups = plugin_menu_groups(&commands, &panels, &context(), |_| None);

        assert_eq!(groups.len(), 2);
        assert_eq!(
            groups[0]
                .panels
                .iter()
                .map(|panel| panel.id.as_str())
                .collect::<Vec<_>>(),
            ["json.inspector.panel"]
        );
        assert!(groups[1].panels.is_empty());
    }

    #[test]
    fn duplicate_panel_detection_ignores_case_and_punctuation() {
        let mut preview = command("demo", "demo.preview", true);
        preview.label = LocalizedText::new("DEMO — preview!");
        let panels = vec![panel("demo", "demo.panel", "demo preview.")];
        let groups = plugin_menu_groups(&[preview], &panels, &context(), |_| None);

        assert!(groups[0].panels.is_empty());
    }
}

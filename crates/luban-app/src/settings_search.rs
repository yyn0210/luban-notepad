//! Settings search metadata for the preferences dialog.
//!
//! Search only filters what is displayed: no setting changes when the query
//! changes, and clearing the field restores every group.

use std::collections::HashSet;

use crate::i18n::tr;

#[derive(Clone, Copy, Debug)]
pub struct SettingsGroup {
    pub key: &'static str,
    pub label_key: &'static str,
    /// Extra English and Chinese search terms for the group.
    pub keywords: &'static [&'static str],
}

pub const SETTINGS_GROUPS: &[SettingsGroup] = &[
    SettingsGroup {
        key: "language",
        label_key: "prefs.language",
        keywords: &["language", "locale", "ui", "英文", "中文", "语言"],
    },
    SettingsGroup {
        key: "appearance",
        label_key: "prefs.appearance",
        keywords: &[
            "theme", "syntax", "color", "light", "dark", "主题", "配色", "语法",
        ],
    },
    SettingsGroup {
        key: "font",
        label_key: "prefs.font",
        keywords: &["font", "family", "size", "monospace", "字体", "字号"],
    },
    SettingsGroup {
        key: "editor",
        label_key: "prefs.editor",
        keywords: &[
            "editor",
            "tab",
            "indent",
            "spaces",
            "line numbers",
            "wrap",
            "bracket",
            "auto indent",
            "trim",
            "缩进",
            "制表符",
            "行号",
            "自动换行",
            "括号",
        ],
    },
    SettingsGroup {
        key: "view",
        label_key: "prefs.view",
        keywords: &[
            "view",
            "whitespace",
            "indent guides",
            "ruler",
            "document map",
            "minimap",
            "bracket colors",
            "breadcrumbs",
            "outline",
            "空白字符",
            "参考线",
            "标尺",
            "文档地图",
            "括号配色",
            "面包屑",
            "大纲",
        ],
    },
    SettingsGroup {
        key: "workspace",
        label_key: "prefs.workspace",
        keywords: &[
            "workspace",
            "folder",
            "roots",
            "recent",
            "gitignore",
            "include",
            "exclude",
            "工作区",
            "文件夹",
            "忽略",
        ],
    },
    SettingsGroup {
        key: "session",
        label_key: "prefs.session",
        keywords: &[
            "session",
            "restore",
            "autosave",
            "interval",
            "会话",
            "恢复",
            "自动保存",
        ],
    },
    SettingsGroup {
        key: "history",
        label_key: "prefs.local_history",
        keywords: &["history", "snapshot", "backup", "历史", "快照", "备份"],
    },
    SettingsGroup {
        key: "files",
        label_key: "prefs.files",
        keywords: &[
            "files",
            "large file",
            "encoding",
            "line ending",
            "size",
            "文件",
            "大文件",
            "编码",
            "换行",
        ],
    },
    SettingsGroup {
        key: "keybindings",
        label_key: "prefs.keybindings",
        keywords: &["keybindings", "shortcuts", "keys", "快捷键", "按键"],
    },
    SettingsGroup {
        key: "macros",
        label_key: "prefs.macros",
        keywords: &["macros", "recording", "宏", "录制"],
    },
    SettingsGroup {
        key: "backup",
        label_key: "prefs.import_export",
        keywords: &[
            "import", "export", "reset", "settings", "导入", "导出", "重置", "设置",
        ],
    },
];

/// Whether one group matches every whitespace-separated term.
pub fn group_visible(query: &str, group: &SettingsGroup) -> bool {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|term| term.to_lowercase())
        .collect();
    if terms.is_empty() {
        return true;
    }
    let title = tr(group.label_key).to_lowercase();
    terms.iter().all(|term| {
        title.contains(term)
            || group
                .keywords
                .iter()
                .any(|keyword| keyword.to_lowercase().contains(term))
    })
}

/// Keys of the groups visible for a query.
pub fn visible_group_keys(query: &str) -> HashSet<&'static str> {
    SETTINGS_GROUPS
        .iter()
        .filter(|group| group_visible(query, group))
        .map(|group| group.key)
        .collect()
}

/// Whether any group is visible. An empty query always shows all groups.
pub fn has_matches(query: &str) -> bool {
    query.split_whitespace().next().is_none() || !visible_group_keys(query).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_shows_every_group() {
        assert_eq!(visible_group_keys("").len(), SETTINGS_GROUPS.len());
        assert_eq!(visible_group_keys("   ").len(), SETTINGS_GROUPS.len());
        assert!(has_matches(""));
    }

    #[test]
    fn english_terms_match_case_insensitively() {
        assert!(group_visible("THEME", &SETTINGS_GROUPS[1]));
        assert!(group_visible("indent guides", &SETTINGS_GROUPS[4]));
        assert!(group_visible("document map", &SETTINGS_GROUPS[4]));
        assert!(!group_visible("document map", &SETTINGS_GROUPS[3]));
    }

    #[test]
    fn chinese_terms_match() {
        assert!(group_visible("主题", &SETTINGS_GROUPS[1]));
        assert!(group_visible("文档地图", &SETTINGS_GROUPS[4]));
        assert!(group_visible("工作区", &SETTINGS_GROUPS[5]));
    }

    #[test]
    fn every_term_must_match() {
        assert!(group_visible("session autosave", &SETTINGS_GROUPS[6]));
        assert!(!group_visible("session theme", &SETTINGS_GROUPS[6]));
    }

    #[test]
    fn no_match_reports_empty() {
        assert!(!has_matches("zzzz-not-a-setting"));
        assert!(visible_group_keys("zzzz-not-a-setting").is_empty());
    }
}

use std::path::PathBuf;

use gpui_kit::component::input::InputState;
use gpui_kit::{AppContext as _, Context, Entity, Subscription, Window};
use luban_core::workspace::FileEntry;

use crate::app::LubanApp;
use crate::i18n::tr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileDialogMode {
    Open,
    Save,
    Folder,
}

#[derive(Clone, Debug)]
pub enum FileDialogPurpose {
    OpenFiles,
    OpenFolder,
    FindInFilesFolder,
    SaveAs { tab_id: u64, then: SaveThen },
}

pub struct FileDialogState {
    pub mode: FileDialogMode,
    pub purpose: FileDialogPurpose,
    pub dir: PathBuf,
    pub entries: Vec<FileEntry>,
    pub selected: Option<PathBuf>,
    pub error: Option<String>,
}

impl FileDialogState {
    pub fn new(
        mode: FileDialogMode,
        purpose: FileDialogPurpose,
        dir: PathBuf,
        entries: Vec<FileEntry>,
    ) -> Self {
        Self {
            mode,
            purpose,
            dir,
            entries,
            selected: None,
            error: None,
        }
    }

    pub fn title(&self) -> String {
        tr(match self.mode {
            FileDialogMode::Open => "dialog.open_file",
            FileDialogMode::Save => "dialog.save_as",
            FileDialogMode::Folder => match self.purpose {
                FileDialogPurpose::OpenFolder => "dialog.open_folder",
                FileDialogPurpose::FindInFilesFolder => "dialog.choose_search_folder",
                _ => "dialog.choose_folder",
            },
        })
    }

    pub fn confirm_label(&self) -> String {
        tr(match self.mode {
            FileDialogMode::Open => "dialog.open",
            FileDialogMode::Save => "dialog.save",
            FileDialogMode::Folder => "dialog.choose_folder",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseIntent {
    Tab(u64),
    AllTabs,
    Exit,
}

pub struct ConfirmState {
    pub intent: CloseIntent,
    pub title: String,
    pub detail: String,
}

impl ConfirmState {
    pub fn new(intent: CloseIntent, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            intent,
            title: title.into(),
            detail: detail.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveThen {
    Nothing,
    CloseTab(u64),
    Exit,
    CloseAll,
}

pub struct SaveAsRequest {
    pub tab_id: u64,
    pub path: PathBuf,
    pub then: SaveThen,
}

pub struct GoToLineState {
    pub input: Entity<InputState>,
    pub subscription: Option<Subscription>,
}

impl GoToLineState {
    pub fn new(window: &mut Window, cx: &mut Context<LubanApp>) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("dialog.line_placeholder")));
        Self {
            input,
            subscription: None,
        }
    }
}

pub struct LanguagePickerState {
    pub filter: Entity<InputState>,
}

impl LanguagePickerState {
    pub fn new(window: &mut Window, cx: &mut Context<LubanApp>) -> Self {
        let filter =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("dialog.filter_languages")));
        Self { filter }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_dialog_labels_follow_the_locale() {
        let _guard = crate::i18n::test_lock();
        let open = FileDialogState::new(
            FileDialogMode::Open,
            FileDialogPurpose::OpenFiles,
            PathBuf::from("/tmp"),
            Vec::new(),
        );
        let save = FileDialogState::new(
            FileDialogMode::Save,
            FileDialogPurpose::SaveAs {
                tab_id: 1,
                then: SaveThen::Nothing,
            },
            PathBuf::from("/tmp"),
            Vec::new(),
        );
        let folder = FileDialogState::new(
            FileDialogMode::Folder,
            FileDialogPurpose::FindInFilesFolder,
            PathBuf::from("/tmp"),
            Vec::new(),
        );

        rust_i18n::set_locale("en");
        assert_eq!(open.title(), "Open File");
        assert_eq!(open.confirm_label(), "Open");
        assert_eq!(save.title(), "Save As");
        assert_eq!(save.confirm_label(), "Save");
        assert_eq!(folder.title(), "Choose Search Folder");
        assert_eq!(folder.confirm_label(), "Choose Folder");

        rust_i18n::set_locale("zh-CN");
        assert_eq!(open.title(), "打开文件");
        assert_eq!(open.confirm_label(), "打开");
        assert_eq!(save.title(), "另存为");
        assert_eq!(save.confirm_label(), "保存");
        assert_eq!(folder.title(), "选择搜索文件夹");
        assert_eq!(folder.confirm_label(), "选择文件夹");
        rust_i18n::set_locale("en");
    }
}

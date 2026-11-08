use std::path::PathBuf;

use gpui_kit::component::input::InputState;
use gpui_kit::{AppContext as _, Context, Entity, Subscription, Window};

use crate::app::LubanApp;
use crate::i18n::tr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloseIntent {
    Tab(u64),
    AllTabs,
    Exit,
    /// A disk-engine save found the file changed on disk; "save" overwrites
    /// and the secondary action reloads.
    OverwriteExternal(u64),
    /// A normal document save found the file changed on disk.
    SaveConflict(u64),
    /// Reload a tab after discarding or saving its edits.
    ReloadTab(u64),
    /// Replace the buffer with a local-history snapshot.
    RestoreHistory,
    /// Save/discard every tab targeted by close-other/close-right.
    TrimTabs,
    /// Delete a file or folder from the tree. `permanent` selects the
    /// irreversible action; the default moves the item to the trash.
    DeletePath {
        path: PathBuf,
        permanent: bool,
    },
    /// Replace the global settings with a parsed file.
    ImportSettings,
    /// Restore the default global settings.
    ResetSettings,
    /// Apply the workspace replace preview.
    WorkspaceReplace,
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
    /// Continue a queued Save All / Close All / Exit sequence.
    NextHugeSave,
    /// Continue the queued normal-document part of a batch save.
    NextNormalSave,
    /// Reload the tab after saving it.
    ReloadTab(u64),
    /// Continue saving the targets of a close-other/close-right plan.
    NextTrimSave,
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

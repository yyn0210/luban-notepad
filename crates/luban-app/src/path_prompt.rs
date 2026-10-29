//! Small name prompt used for new file, new folder and rename.

use std::path::PathBuf;

use gpui_kit::component::input::InputState;
use gpui_kit::{AppContext as _, Context, Entity, Window};

use crate::app::LubanApp;
use crate::i18n::tr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamePromptKind {
    NewFile,
    NewFolder,
    Rename,
}

impl NamePromptKind {
    pub fn title_key(self) -> &'static str {
        match self {
            Self::NewFile => "explorer.new_file_title",
            Self::NewFolder => "explorer.new_folder_title",
            Self::Rename => "explorer.rename_title",
        }
    }

    pub fn action_key(self) -> &'static str {
        match self {
            Self::NewFile | Self::NewFolder => "explorer.create",
            Self::Rename => "explorer.rename",
        }
    }
}

pub struct NamePromptState {
    pub kind: NamePromptKind,
    /// Directory that receives the new entry.
    pub parent: PathBuf,
    /// Existing path for rename.
    pub original: Option<PathBuf>,
    pub input: Entity<InputState>,
    pub error: Option<String>,
}

impl NamePromptState {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<LubanApp>,
        kind: NamePromptKind,
        parent: PathBuf,
        original: Option<PathBuf>,
        initial: String,
    ) -> Self {
        let placeholder = tr("explorer.name_placeholder");
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        input.update(cx, |input, cx| {
            input.set_value(initial, window, cx);
            input.select_all(window, cx);
            input.focus(window, cx);
        });
        Self {
            kind,
            parent,
            original,
            input,
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_follow_the_prompt_kind() {
        assert_eq!(
            NamePromptKind::NewFile.title_key(),
            "explorer.new_file_title"
        );
        assert_eq!(NamePromptKind::NewFolder.action_key(), "explorer.create");
        assert_eq!(NamePromptKind::Rename.title_key(), "explorer.rename_title");
    }
}

//! Plugin manager UI state.

use std::collections::BTreeMap;

use gpui_kit::component::input::InputState;
use gpui_kit::*;

use crate::i18n::tr;

use crate::app::LubanApp;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginManagerTab {
    Installed,
    Store,
}

pub struct PluginManagerState {
    pub open: bool,
    pub tab: PluginManagerTab,
    pub install_source: Entity<InputState>,
    pub message: Option<String>,
    pub error: Option<String>,
    /// Source that the user already asked to install over an existing plugin.
    pub pending_replace: Option<String>,
    /// Plugin whose uninstall is waiting for explicit confirmation.
    pub pending_uninstall: Option<String>,
    /// Remove plugin data and settings when uninstalling.
    pub remove_data: bool,
    /// Text inputs for plugin string settings, keyed by `plugin:key`.
    pub text_inputs: BTreeMap<String, Entity<InputState>>,
    /// Store catalog, search and download state.
    pub store: crate::plugin_store::PluginStoreState,
}

impl PluginManagerState {
    pub fn new(window: &mut Window, cx: &mut Context<LubanApp>, store_url: &str) -> Self {
        let install_source =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("plugins.install_placeholder")));
        let store = crate::plugin_store::PluginStoreState::new(window, cx, store_url);
        Self {
            open: false,
            tab: PluginManagerTab::Installed,
            install_source,
            message: None,
            error: None,
            pending_replace: None,
            pending_uninstall: None,
            remove_data: false,
            text_inputs: BTreeMap::new(),
            store,
        }
    }

    pub fn open(&mut self, window: &mut Window, cx: &mut Context<LubanApp>) {
        self.open = true;
        self.tab = PluginManagerTab::Installed;
        self.message = None;
        self.error = None;
        self.pending_replace = None;
        self.pending_uninstall = None;
        self.install_source =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("plugins.install_placeholder")));
        self.install_source
            .update(cx, |input, cx| input.focus(window, cx));
        self.text_inputs.clear();
    }

    pub fn text_input(
        &mut self,
        key: &str,
        value: &str,
        window: &mut Window,
        cx: &mut Context<LubanApp>,
    ) -> Entity<InputState> {
        if let Some(input) = self.text_inputs.get(key) {
            return input.clone();
        }
        let value = value.to_string();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(value)
                .placeholder(tr("plugins.value_placeholder"))
        });
        self.text_inputs.insert(key.to_string(), input.clone());
        input
    }
}

use super::*;

impl LubanApp {
    // ------------------------------------------------------------------
    // Plugin manager UI: settings, store and installed plugins
    // ------------------------------------------------------------------

    fn apply_plugin_setting(
        &mut self,
        plugin_id: &str,
        key: &str,
        value: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        if !self
            .plugin_host
            .update_setting(plugin_id, key, value.clone())
        {
            self.plugin_manager.error = Some(tr_args(
                "plugins.invalid_setting",
                &[("plugin", plugin_id.to_string()), ("key", key.to_string())],
            ));
            cx.notify();
            return;
        }
        self.settings
            .plugin_settings
            .entry(plugin_id.to_string())
            .or_default()
            .insert(key.to_string(), value);
        self.global_settings.plugin_settings = self.settings.plugin_settings.clone();
        self.commit_settings();
        self.plugin_manager.error = None;
        cx.notify();
    }

    pub(super) fn plugin_action_reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.plugin_manager.pending_uninstall = None;
        self.reload_plugins(window, cx);
        self.commit_settings();
        self.plugin_manager.message = Some(tr("plugins.reloaded"));
        cx.notify();
    }

    fn uninstall_plugin(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let result = luban_plugin::install::uninstall(
            &self.plugin_host.root,
            id,
            self.plugin_manager.remove_data,
        );
        match result {
            Ok(()) => {
                if self.plugin_manager.remove_data {
                    self.settings.plugin_settings.remove(id);
                    self.global_settings.plugin_settings = self.settings.plugin_settings.clone();
                }
                self.reload_plugins(window, cx);
                self.commit_settings();
                self.plugin_manager.pending_uninstall = None;
                self.plugin_manager.error = None;
                self.plugin_manager.message = Some(tr_args(
                    "plugins.uninstalled",
                    &[("plugin", id.to_string())],
                ));
                cx.notify();
            }
            Err(error) => {
                self.plugin_manager.pending_uninstall = None;
                self.plugin_manager.error = Some(error);
                cx.notify();
            }
        }
    }

    fn plugin_state_label(state: &luban_plugin::registry::PluginState) -> String {
        use luban_plugin::registry::PluginState;
        match state {
            PluginState::Enabled => tr("plugins.state.enabled"),
            PluginState::DisabledByUser => tr("plugins.state.disabled"),
            PluginState::NeedsApproval => tr("plugins.state.needs_approval"),
            PluginState::PluginsDisabled => tr("plugins.state.plugins_disabled"),
            PluginState::MissingDependency(dependency) => tr_args(
                "plugins.state.missing_dependency",
                &[("dependency", dependency.clone())],
            ),
            PluginState::IncompatibleDependency(dependency) => tr_args(
                "plugins.state.incompatible_dependency",
                &[("dependency", dependency.clone())],
            ),
            PluginState::DependencyCycle(ids) => {
                tr_args("plugins.state.dependency_cycle", &[("ids", ids.clone())])
            }
            PluginState::LoadError(error) => error.clone(),
        }
    }

    pub(super) fn open_plugin_store(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.plugin_manager.open {
            self.plugin_manager.open(window, cx);
        }
        self.show_plugin_store_tab(window, cx);
    }

    fn show_plugin_store_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.plugin_manager.tab = crate::plugin_manager::PluginManagerTab::Store;
        let source = self.settings.plugin_store_url.clone();
        self.plugin_manager
            .store
            .url_input
            .update(cx, |input, cx| input.set_value(source, window, cx));
        self.ensure_plugin_store_catalog(cx);
        self.plugin_manager
            .store
            .search
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn show_installed_plugins_tab(&mut self, cx: &mut Context<Self>) {
        self.plugin_manager.tab = crate::plugin_manager::PluginManagerTab::Installed;
        cx.notify();
    }

    fn ensure_plugin_store_catalog(&mut self, cx: &mut Context<Self>) {
        let source = self.settings.plugin_store_url.clone();
        {
            let store = &self.plugin_manager.store;
            if store.is_busy() {
                return;
            }
            if store.catalog.is_some() && store.source_matches(&source) {
                return;
            }
        }
        self.start_plugin_store_fetch(source, cx);
    }

    fn start_plugin_store_fetch(&mut self, source: String, cx: &mut Context<Self>) {
        if luban_plugin::store::validate_catalog_source(&source).is_err() {
            self.plugin_manager.store.error =
                Some(crate::plugin_store::localize_url_error(&source));
            cx.notify();
            return;
        }
        match self.plugin_manager.store.begin_fetch(source) {
            Ok(()) => {
                self.plugin_manager.store.message = None;
            }
            Err(error) => {
                self.plugin_manager.store.error = Some(error);
            }
        }
        cx.notify();
    }

    fn refresh_plugin_store(&mut self, cx: &mut Context<Self>) {
        let source = self.settings.plugin_store_url.clone();
        self.plugin_manager.store.install_error = None;
        self.plugin_manager.store.message = None;
        self.plugin_manager.store.url_error = None;
        self.start_plugin_store_fetch(source, cx);
    }

    fn apply_plugin_store_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let url = self
            .plugin_manager
            .store
            .url_input
            .read(cx)
            .value()
            .trim()
            .to_string();
        self.plugin_manager.store.install_error = None;
        self.plugin_manager.store.message = None;
        if luban_plugin::store::validate_catalog_source(&url).is_err() {
            self.plugin_manager.store.url_error =
                Some(crate::plugin_store::localize_url_error(&url));
            cx.notify();
            return;
        }
        self.plugin_manager.store.url_error = None;
        self.settings.plugin_store_url = url.clone();
        self.global_settings.plugin_store_url = url.clone();
        self.commit_settings();
        self.plugin_manager.store.clear_catalog();
        self.plugin_manager.store.error = None;
        self.start_plugin_store_fetch(url, cx);
        let _ = window;
    }

    fn reset_plugin_store_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let url = luban_core::settings::DEFAULT_PLUGIN_STORE_URL.to_string();
        self.plugin_manager.store.install_error = None;
        self.plugin_manager.store.message = None;
        self.plugin_manager
            .store
            .url_input
            .update(cx, |input, cx| input.set_value(url.clone(), window, cx));
        self.plugin_manager.store.url_error = None;
        self.settings.plugin_store_url = url.clone();
        self.global_settings.plugin_store_url = url.clone();
        self.commit_settings();
        self.plugin_manager.store.clear_catalog();
        self.plugin_manager.store.error = None;
        self.start_plugin_store_fetch(url, cx);
    }

    /// Apply finished background store work. Called from the idle timer so a
    /// download can finish while no window event is pending.
    pub fn poll_plugin_store(&mut self, cx: &mut Context<Self>) {
        let root = self.plugin_host.root.clone();
        let Some(event) = self.plugin_manager.store.poll(&root) else {
            return;
        };
        match event {
            crate::plugin_store::StoreEvent::CatalogLoaded { offline } => {
                self.plugin_manager.store.message = if offline {
                    Some(tr("plugins.store.offline_cache_hint"))
                } else {
                    None
                };
            }
            crate::plugin_store::StoreEvent::CatalogFailed(error) => {
                log::warn!("plugin store catalog failed: {error}");
            }
            crate::plugin_store::StoreEvent::DownloadReady { install, path } => {
                let expected = luban_plugin::install::ExpectedPlugin {
                    id: install.plugin_id.clone(),
                    version: install.version.clone(),
                };
                let source = luban_plugin::install::InstallSource::Store {
                    catalog: install.catalog.clone(),
                };
                let result = luban_plugin::install::install_zip_checked_with_source(
                    &root,
                    &path,
                    install.replace,
                    &expected,
                    source,
                );
                let _ = std::fs::remove_file(&path);
                let message = match result {
                    Ok(record) => {
                        self.reload_plugins_now(cx);
                        self.commit_settings();
                        Ok(tr_args(
                            "plugins.store.installed",
                            &[
                                ("plugin", record.id.clone()),
                                ("version", record.version.clone()),
                            ],
                        ))
                    }
                    Err(error) => Err(crate::plugin_store::localize_error(
                        crate::plugin_store::StoreErrorContext::Install,
                        &error,
                    )),
                };
                self.plugin_manager.store.finish_install(message);
            }
            crate::plugin_store::StoreEvent::DownloadFailed { plugin_id, message } => {
                log::warn!("plugin store download for {plugin_id} failed: {message}");
                self.plugin_manager.store.finish_install(Err(message));
            }
        }
        cx.notify();
    }

    fn installed_plugin_version(&self, id: &str) -> Option<String> {
        self.plugin_host
            .registry
            .entries
            .get(id)
            .map(|entry| entry.version().to_string())
            .or_else(|| {
                self.plugin_host
                    .records
                    .plugins
                    .get(id)
                    .map(|record| record.version.clone())
            })
    }

    fn install_store_plugin(&mut self, plugin_id: &str, cx: &mut Context<Self>) {
        let Some(target) = luban_plugin::store::Target::current() else {
            self.plugin_manager.store.install_error =
                Some(tr("plugins.store.unsupported_platform"));
            cx.notify();
            return;
        };
        let host_api = luban_plugin::manifest::API_VERSION;
        let prepared: Result<(String, luban_plugin::store::Artifact), String> = {
            let Some(plugin) = self
                .plugin_manager
                .store
                .catalog
                .as_ref()
                .and_then(|catalog| catalog.find(plugin_id))
            else {
                return;
            };
            match plugin.install_candidate(target, host_api) {
                Some(candidate) => Ok((
                    candidate.version.version.clone(),
                    candidate.artifact.clone(),
                )),
                None if plugin.requires_newer_api(host_api) => Err(tr_args(
                    "plugins.store.incompatible_api",
                    &[("version", plugin.required_api_version().to_string())],
                )),
                None => Err(tr("plugins.store.unavailable_for_platform")),
            }
        };
        let (version, artifact) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.plugin_manager.store.install_error = Some(error);
                cx.notify();
                return;
            }
        };
        let replace = self.plugin_host.root.join(plugin_id).is_dir();
        let root = self.plugin_host.root.clone();
        let catalog = self
            .plugin_manager
            .store
            .source
            .clone()
            .unwrap_or_else(|| self.settings.plugin_store_url.clone());
        let result = self
            .plugin_manager
            .store
            .begin_download(&root, plugin_id, &version, &artifact, replace, &catalog);
        match result {
            Ok(()) => self.plugin_manager.store.install_error = None,
            Err(error) => self.plugin_manager.store.install_error = Some(error),
        }
        cx.notify();
    }

    fn plugin_manager_header(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let weak_close = cx.weak_entity();
        div()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr("plugins.title")),
            )
            .child(
                Button::new("plugin-manager-close")
                    .label(tr("common.close"))
                    .compact()
                    .on_click(move |_, window, cx| {
                        let _ = weak_close.update(cx, |this, cx| {
                            this.plugin_manager.open = false;
                            this.focus_editor(window, cx);
                            cx.notify();
                        });
                    }),
            )
            .into_any_element()
    }

    fn plugin_manager_tabs(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let tab = self.plugin_manager.tab;
        let weak_installed = cx.weak_entity();
        let weak_store = cx.weak_entity();
        let mut installed = Button::new("plugin-tab-installed")
            .label(tr("plugins.tab.installed"))
            .compact()
            .on_click(move |_, _, cx| {
                let _ = weak_installed.update(cx, |this, cx| {
                    this.show_installed_plugins_tab(cx);
                });
            });
        if tab == crate::plugin_manager::PluginManagerTab::Installed {
            installed = installed.primary();
        }
        let mut store = Button::new("plugin-tab-store")
            .label(tr("plugins.tab.store"))
            .compact()
            .on_click(move |_, window, cx| {
                let _ = weak_store.update(cx, |this, cx| {
                    this.show_plugin_store_tab(window, cx);
                });
            });
        if tab == crate::plugin_manager::PluginManagerTab::Store {
            store = store.primary();
        }
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(installed)
            .child(store)
            .into_any_element()
    }

    fn render_plugin_store(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        struct StoreRow {
            id: String,
            name: String,
            description: String,
            publisher: String,
            version: String,
            tags: String,
            permissions: String,
            platforms: String,
            installed: Option<String>,
            update: luban_plugin::store::UpdateStatus,
            compatible: bool,
            unsupported_platform: bool,
            required_api: u32,
        }
        struct StoreDetails {
            id: String,
            name: String,
            description: String,
            version: String,
            release_notes: String,
            source: String,
            homepage: String,
            license: String,
            artifact_size: Option<u64>,
            permissions: String,
            platforms: String,
            installed: Option<String>,
            update: luban_plugin::store::UpdateStatus,
            compatible: bool,
            unsupported_platform: bool,
            required_api: u32,
            verified: bool,
        }

        let locale = rust_i18n::locale().to_string();
        let target = luban_plugin::store::Target::current();
        let host_api = luban_plugin::manifest::API_VERSION;
        let query = self
            .plugin_manager
            .store
            .search
            .read(cx)
            .value()
            .trim()
            .to_string();
        let mut rows = Vec::new();
        if let Some(catalog) = self.plugin_manager.store.catalog.as_ref() {
            let mut plugins: Vec<&luban_plugin::store::StorePlugin> = catalog
                .plugins
                .iter()
                .filter(|plugin| {
                    target.is_none_or(|target| {
                        plugin.requires_newer_api(host_api)
                            || plugin.install_candidate(target, host_api).is_some()
                    })
                })
                .filter(|plugin| plugin.matches_query(&query, &locale))
                .collect();
            plugins.sort_by(|left, right| {
                let left_name = left.localized_name(&locale).to_lowercase();
                let right_name = right.localized_name(&locale).to_lowercase();
                left_name
                    .cmp(&right_name)
                    .then_with(|| left.id.cmp(&right.id))
            });
            for plugin in plugins {
                let installed = self.installed_plugin_version(&plugin.id);
                let candidate =
                    target.and_then(|target| plugin.install_candidate(target, host_api));
                let compatible = candidate.is_some();
                let update = match target {
                    Some(target) => plugin.update_status(installed.as_deref(), target, host_api),
                    None if installed.is_some() => luban_plugin::store::UpdateStatus::Installed,
                    None => luban_plugin::store::UpdateStatus::NotInstalled,
                };
                let latest = candidate
                    .map(|candidate| candidate.version.version.clone())
                    .or_else(|| {
                        plugin
                            .latest_compatible(host_api)
                            .map(|version| version.version.clone())
                    })
                    .unwrap_or_else(|| plugin.versions[0].version.clone());
                let permissions = plugin_permission_summary(&plugin.permissions);
                let platforms = plugin_platform_summary(plugin);
                rows.push(StoreRow {
                    id: plugin.id.clone(),
                    name: plugin.localized_name(&locale).to_string(),
                    description: plugin
                        .localized_description(&locale)
                        .unwrap_or_default()
                        .to_string(),
                    publisher: plugin.publisher.clone(),
                    version: latest,
                    tags: plugin.tags.join(", "),
                    permissions,
                    platforms,
                    installed,
                    update,
                    compatible,
                    unsupported_platform: target.is_none(),
                    required_api: plugin.required_api_version(),
                });
            }
        }

        let mut selected = self.plugin_manager.store.selected.clone();
        if selected
            .as_deref()
            .is_none_or(|id| !rows.iter().any(|row| row.id == id))
        {
            selected = rows.first().map(|row| row.id.clone());
            self.plugin_manager.store.selected = selected.clone();
        }
        let details = selected.as_ref().and_then(|id| {
            let plugin = self.plugin_manager.store.catalog.as_ref()?.find(id)?;
            let candidate = target.and_then(|target| plugin.install_candidate(target, host_api));
            let installed = self.installed_plugin_version(&plugin.id);
            let update = match target {
                Some(target) => plugin.update_status(installed.as_deref(), target, host_api),
                None if installed.is_some() => luban_plugin::store::UpdateStatus::Installed,
                None => luban_plugin::store::UpdateStatus::NotInstalled,
            };
            Some(StoreDetails {
                id: plugin.id.clone(),
                name: plugin.localized_name(&locale).to_string(),
                description: plugin
                    .localized_description(&locale)
                    .unwrap_or_default()
                    .to_string(),
                version: candidate
                    .map(|candidate| candidate.version.version.clone())
                    .or_else(|| {
                        plugin
                            .latest_compatible(host_api)
                            .map(|version| version.version.clone())
                    })
                    .unwrap_or_else(|| plugin.versions[0].version.clone()),
                release_notes: candidate
                    .and_then(|candidate| candidate.version.release_notes.clone())
                    .or_else(|| {
                        plugin
                            .latest_compatible(host_api)
                            .and_then(|version| version.release_notes.clone())
                    })
                    .unwrap_or_else(|| "—".to_string()),
                source: plugin.source.clone().unwrap_or_else(|| "—".to_string()),
                homepage: plugin.homepage.clone().unwrap_or_else(|| "—".to_string()),
                license: plugin.license.clone().unwrap_or_else(|| "—".to_string()),
                artifact_size: candidate.map(|candidate| candidate.artifact.size),
                permissions: plugin_permission_summary(&plugin.permissions),
                platforms: plugin_platform_summary(plugin),
                installed,
                update,
                compatible: candidate.is_some(),
                unsupported_platform: target.is_none(),
                required_api: plugin.required_api_version(),
                verified: plugin.verified,
            })
        });

        let loading = self.plugin_manager.store.loading;
        let busy = self.plugin_manager.store.is_busy();
        let offline = self.plugin_manager.store.offline;
        let fetched_at = self.plugin_manager.store.fetched_at;
        let error = self.plugin_manager.store.error.clone();
        let install_error = self.plugin_manager.store.install_error.clone();
        let url_error = self.plugin_manager.store.url_error.clone();
        let message = self.plugin_manager.store.message.clone();
        let warnings = self.plugin_manager.store.warnings.clone();
        let installing = self.plugin_manager.store.installing.clone();
        let has_catalog = self.plugin_manager.store.catalog.is_some();
        let safe_mode = self.safe_mode;
        let plugins_enabled = self.settings.plugins_enabled;
        let empty_catalog = self
            .plugin_manager
            .store
            .catalog
            .as_ref()
            .is_some_and(|catalog| catalog.plugins.is_empty());
        let search_input = self.plugin_manager.store.search.clone();
        let url_input = self.plugin_manager.store.url_input.clone();
        let weak_refresh = cx.weak_entity();
        let weak_apply = cx.weak_entity();
        let weak_reset = cx.weak_entity();

        let mut status = div().flex().flex_col().gap_1();
        if loading {
            status = status.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr("plugins.store.loading")),
            );
        }
        if offline {
            let fetched = fetched_at
                .map(|seconds| {
                    crate::logging::format_timestamp(
                        std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds),
                    )
                })
                .unwrap_or_else(|| tr("plugins.store.unknown_time"));
            status = status.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors.warning)
                    .child(tr_args("plugins.store.offline_cache", &[("time", fetched)])),
            );
        }
        if safe_mode || !plugins_enabled {
            status = status.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors.warning)
                    .child(tr("plugins.store.plugins_disabled_hint")),
            );
        }
        if let Some(error) = error.clone() {
            status = status.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors.danger)
                    .child(error),
            );
        }
        if let Some(error) = install_error.clone() {
            status = status.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors.danger)
                    .child(error),
            );
        }
        if let Some(message) = message.clone() {
            status = status.child(div().text_xs().child(message));
        }
        if !warnings.is_empty() {
            let mut warning_list = div().flex().flex_col().gap_1().child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr_args(
                        "plugins.store.skipped_entries",
                        &[("count", warnings.len().to_string())],
                    )),
            );
            for warning in warnings.iter().take(5) {
                warning_list = warning_list.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().colors.muted_foreground)
                        .child(warning.clone()),
                );
            }
            status = status.child(warning_list);
        }

        let url_row = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(tr("plugins.store.url_label")),
                    )
                    .child(Input::new(&url_input).h(px(28.)).flex_1())
                    .child(
                        Button::new("plugin-store-url-apply")
                            .label(tr("plugins.store.apply"))
                            .compact()
                            .disabled(busy)
                            .on_click(move |_, window, cx| {
                                let _ = weak_apply.update(cx, |this, cx| {
                                    this.apply_plugin_store_url(window, cx);
                                });
                            }),
                    )
                    .child(
                        Button::new("plugin-store-url-reset")
                            .label(tr("plugins.store.reset"))
                            .compact()
                            .disabled(busy)
                            .on_click(move |_, window, cx| {
                                let _ = weak_reset.update(cx, |this, cx| {
                                    this.reset_plugin_store_url(window, cx);
                                });
                            }),
                    ),
            )
            .when_some(url_error.clone(), |this, error| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().colors.danger)
                        .child(error),
                )
            });

        let mut list = div()
            .id("plugin-store-list")
            .flex()
            .flex_col()
            .gap_2()
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll();
        for row in &rows {
            let id = row.id.clone();
            let selected_here = selected.as_deref() == Some(id.as_str());
            let installing_here = installing.as_deref() == Some(id.as_str());
            let is_update = matches!(
                row.update,
                luban_plugin::store::UpdateStatus::UpdateAvailable { .. }
            );
            let (button_label, button_enabled) = if installing_here {
                (tr("plugins.store.installing"), false)
            } else if row.unsupported_platform {
                (tr("plugins.store.unsupported_platform_short"), false)
            } else if !row.compatible {
                (
                    tr_args(
                        "plugins.store.incompatible_short",
                        &[("version", row.required_api.to_string())],
                    ),
                    false,
                )
            } else if row.installed.is_some() && !is_update {
                (tr("plugins.store.installed_button"), false)
            } else if is_update {
                (tr("plugins.store.update"), !busy)
            } else {
                (tr("plugins.store.install"), !busy)
            };
            let state_text = if installing_here {
                tr("plugins.store.installing")
            } else if row.unsupported_platform {
                tr("plugins.store.unsupported_platform")
            } else if !row.compatible {
                tr_args(
                    "plugins.store.incompatible_api",
                    &[("version", row.required_api.to_string())],
                )
            } else {
                match &row.update {
                    luban_plugin::store::UpdateStatus::UpdateAvailable { installed, latest } => {
                        tr_args(
                            "plugins.store.update_from",
                            &[("installed", installed.clone()), ("latest", latest.clone())],
                        )
                    }
                    luban_plugin::store::UpdateStatus::Installed => row
                        .installed
                        .as_ref()
                        .map(|version| {
                            tr_args(
                                "plugins.store.installed_version",
                                &[("version", version.clone())],
                            )
                        })
                        .unwrap_or_else(|| tr("plugins.store.not_installed")),
                    luban_plugin::store::UpdateStatus::NotInstalled => {
                        tr("plugins.store.not_installed")
                    }
                }
            };
            let weak_row = cx.weak_entity();
            let row_id = id.clone();
            let weak_install = cx.weak_entity();
            let install_id = id.clone();
            let mut install = Button::new(format!("store-row-install-{id}"))
                .label(button_label)
                .compact()
                .disabled(!button_enabled)
                .on_click(move |_, _, cx| {
                    let _ = weak_install.update(cx, |this, cx| {
                        this.install_store_plugin(&install_id, cx);
                    });
                });
            if button_enabled {
                install = install.primary();
            }
            let card = div()
                .id(format!("store-plugin-{id}"))
                .flex()
                .flex_col()
                .min_w(px(0.))
                .gap_1()
                .p_3()
                .rounded_md()
                .border_1()
                .border_color(if selected_here {
                    cx.theme().colors.accent
                } else {
                    cx.theme().colors.border
                })
                .cursor_pointer()
                .hover(|this| this.bg(cx.theme().colors.secondary))
                .on_click(move |_, _, cx| {
                    let _ = weak_row.update(cx, |this, cx| {
                        this.plugin_manager.store.selected = Some(row_id.clone());
                        this.plugin_manager.store.install_error = None;
                        cx.notify();
                    });
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .min_w(px(0.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .min_w(px(0.))
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .truncate()
                                        .child(format!("{} {}", row.name, row.version)),
                                )
                                .when(!row.tags.is_empty(), |this| {
                                    this.child(
                                        div()
                                            .text_xs()
                                            .truncate()
                                            .text_color(cx.theme().colors.muted_foreground)
                                            .child(format!("#{}", row.tags)),
                                    )
                                }),
                        )
                        .child(div().flex_shrink_0().child(install)),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_xs()
                        .text_color(cx.theme().colors.muted_foreground)
                        .child(format!("{} · {}", row.publisher, row.id))
                        .child(div().child(state_text)),
                )
                .when(!row.description.is_empty(), |this| {
                    this.child(div().text_xs().child(row.description.clone()))
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_xs()
                        .text_color(cx.theme().colors.muted_foreground)
                        .child(format!(
                            "{} · {}",
                            tr("plugins.store.platforms_label"),
                            row.platforms
                        ))
                        .child(format!(
                            "{} · {}",
                            tr("plugins.store.permissions_label"),
                            row.permissions
                        )),
                );
            list = list.child(card);
        }
        if rows.is_empty() {
            let empty_text = if !has_catalog && loading {
                tr("plugins.store.loading")
            } else if !has_catalog {
                tr("plugins.store.load_failed")
            } else if !query.is_empty() {
                tr("plugins.store.no_results")
            } else if empty_catalog {
                tr("plugins.store.empty")
            } else if target.is_none() {
                tr("plugins.store.unsupported_platform")
            } else {
                tr("plugins.store.no_compatible")
            };
            list = list.child(
                div()
                    .p_4()
                    .text_sm()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(empty_text),
            );
        }

        let mut details_panel = div()
            .id("plugin-store-details")
            .w(px(300.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_2()
            .overflow_y_scroll()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(tr("plugins.store.details_title")),
            );
        if let Some(details) = &details {
            let weak_details_install = cx.weak_entity();
            let details_install_id = details.id.clone();
            let details_installing = installing.as_deref() == Some(details.id.as_str());
            let details_update = matches!(
                details.update,
                luban_plugin::store::UpdateStatus::UpdateAvailable { .. }
            );
            let details_label = if details_installing {
                tr("plugins.store.installing")
            } else if details.unsupported_platform {
                tr("plugins.store.unsupported_platform_short")
            } else if !details.compatible {
                tr_args(
                    "plugins.store.incompatible_short",
                    &[("version", details.required_api.to_string())],
                )
            } else if details.installed.is_some() && !details_update {
                tr("plugins.store.installed_button")
            } else if details_update {
                tr("plugins.store.update")
            } else {
                tr("plugins.store.install")
            };
            let details_enabled =
                details.compatible && (details.installed.is_none() || details_update) && !busy;
            let size = details
                .artifact_size
                .map(|size| {
                    luban_core::large_file::format_bytes(
                        size,
                        self.settings.large_file_display_unit,
                    )
                })
                .unwrap_or_else(|| "—".to_string());
            let mut install = Button::new(format!("store-detail-install-{}", details.id))
                .label(details_label)
                .disabled(!details_enabled)
                .on_click(move |_, _, cx| {
                    let _ = weak_details_install.update(cx, |this, cx| {
                        this.install_store_plugin(&details_install_id, cx);
                    });
                });
            if details_enabled {
                install = install.primary();
            }
            let mut panel = details_panel
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(format!("{} {}", details.name, details.version)),
                        )
                        .when(details.verified, |this| {
                            this.child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().colors.accent)
                                    .child(tr("plugins.store.verified")),
                            )
                        }),
                )
                .when(!details.description.is_empty(), |this| {
                    this.child(div().text_xs().child(details.description.clone()))
                })
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .text_xs()
                        .child(
                            div()
                                .text_color(cx.theme().colors.muted_foreground)
                                .child(tr("plugins.store.release_notes")),
                        )
                        .child(div().child(details.release_notes.clone())),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .text_xs()
                        .child(format!(
                            "{}: {}",
                            tr("plugins.store.platforms_label"),
                            details.platforms
                        ))
                        .child(format!(
                            "{}: {}",
                            tr("plugins.store.permissions_label"),
                            details.permissions
                        ))
                        .child(format!("{}: {size}", tr("plugins.store.size_label")))
                        .child(format!(
                            "{}: {}",
                            tr("plugins.store.license_label"),
                            details.license
                        ))
                        .child(format!(
                            "{}: {}",
                            tr("plugins.store.source_label"),
                            details.source
                        ))
                        .child(format!(
                            "{}: {}",
                            tr("plugins.store.homepage_label"),
                            details.homepage
                        )),
                )
                .child(install);
            if let Some(installed) = &details.installed {
                panel = panel.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().colors.muted_foreground)
                        .child(tr_args(
                            "plugins.store.installed_version",
                            &[("version", installed.clone())],
                        )),
                );
            }
            if let luban_plugin::store::UpdateStatus::UpdateAvailable { latest, .. } =
                &details.update
            {
                panel = panel.child(div().text_xs().text_color(cx.theme().colors.accent).child(
                    tr_args(
                        "plugins.store.latest_version",
                        &[("version", latest.clone())],
                    ),
                ));
            }
            if !details.compatible {
                let reason = if details.unsupported_platform {
                    tr("plugins.store.unsupported_platform")
                } else {
                    tr_args(
                        "plugins.store.incompatible_api",
                        &[("version", details.required_api.to_string())],
                    )
                };
                panel = panel.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().colors.warning)
                        .child(reason),
                );
            }
            details_panel = panel;
        } else {
            details_panel = details_panel.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr("plugins.store.select_plugin")),
            );
        }

        let content = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .gap_3()
            .p_5()
            .child(self.plugin_manager_header(cx))
            .child(self.plugin_manager_tabs(cx))
            .child(status)
            .child(url_row)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Input::new(&search_input).h(px(30.)).flex_1())
                    .child(
                        Button::new("plugin-store-refresh")
                            .label(if loading {
                                tr("plugins.store.refreshing")
                            } else {
                                tr("plugins.store.refresh")
                            })
                            .compact()
                            .disabled(busy)
                            .on_click(move |_, _, cx| {
                                let _ = weak_refresh.update(cx, |this, cx| {
                                    this.refresh_plugin_store(cx);
                                });
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .flex_1()
                    .min_h(px(0.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .min_h(px(0.))
                            .child(list),
                    )
                    .child(details_panel),
            );
        self.overlay_card(880.0, Some(680.0), cx, content.into_any_element())
    }

    pub(super) fn render_plugin_manager(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.plugin_manager.tab == crate::plugin_manager::PluginManagerTab::Store {
            return self.render_plugin_store(window, cx);
        }
        struct Row {
            id: String,
            name: String,
            version: String,
            author: String,
            description: String,
            state: String,
            permissions: String,
            manual: bool,
            reinstall: bool,
            approved: bool,
            enabled: bool,
        }
        let rows: Vec<Row> = self
            .plugin_host
            .registry
            .entries
            .values()
            .map(|entry| {
                let state = self
                    .plugin_host
                    .state(&entry.id)
                    .map(Self::plugin_state_label)
                    .unwrap_or_else(|| tr("plugins.state.unknown"));
                let source = self
                    .plugin_host
                    .records
                    .plugins
                    .get(&entry.id)
                    .map(|record| record.source.clone());
                let manual = match &source {
                    Some(luban_plugin::install::InstallSource::Local { path }) => {
                        path == &self.plugin_host.root.join(&entry.id)
                    }
                    None => true,
                    _ => false,
                };
                let reinstall = matches!(
                    source,
                    Some(
                        luban_plugin::install::InstallSource::Zip { .. }
                            | luban_plugin::install::InstallSource::Store { .. }
                    )
                );
                Row {
                    id: entry.id.clone(),
                    name: entry.name().to_string(),
                    version: entry.version().to_string(),
                    author: entry
                        .manifest
                        .as_ref()
                        .and_then(|manifest| manifest.author.clone())
                        .unwrap_or_default(),
                    description: entry
                        .manifest
                        .as_ref()
                        .and_then(|manifest| manifest.description.clone())
                        .unwrap_or_default(),
                    state,
                    permissions: {
                        let mut parts = Vec::new();
                        for name in entry.permissions.declared() {
                            let implemented = luban_plugin::permissions::Permission::ALL
                                .iter()
                                .any(|permission| {
                                    permission.name() == name && permission.implemented()
                                });
                            if implemented {
                                parts.push(name.clone());
                            } else {
                                parts.push(format!("{name} ({})", tr("plugins.unavailable")));
                            }
                        }
                        if parts.is_empty() {
                            tr("plugins.no_permissions")
                        } else {
                            parts.join(", ")
                        }
                    },
                    manual,
                    reinstall,
                    approved: self.plugin_host.approved(&entry.id),
                    enabled: self
                        .plugin_host
                        .state(&entry.id)
                        .is_some_and(|state| state.is_enabled()),
                }
            })
            .collect();
        let schemas: Vec<(String, Vec<luban_plugin::manifest::SettingContribution>)> = self
            .plugin_host
            .schemas
            .iter()
            .map(|(id, schema)| (id.clone(), schema.clone()))
            .collect();
        let weak_toggle = cx.weak_entity();
        let weak_reload = cx.weak_entity();
        let weak_install = cx.weak_entity();
        let weak_uninstall_confirm = cx.weak_entity();
        let weak_uninstall_cancel = cx.weak_entity();
        let install_input = self.plugin_manager.install_source.clone();
        let safe_mode = self.safe_mode;
        let plugins_enabled = self.settings.plugins_enabled;
        let remove_data = self.plugin_manager.remove_data;
        let message = self.plugin_manager.message.clone();
        let error = self.plugin_manager.error.clone();
        let host_errors = self.plugin_host.errors.clone();
        let pending_uninstall = self.plugin_manager.pending_uninstall.clone();
        let uninstall_dependents = pending_uninstall
            .as_ref()
            .map(|id| self.plugin_host.dependents_of(id))
            .unwrap_or_default();

        let mut has_rows = false;
        let mut list = div()
            .id("plugin-manager-list")
            .flex()
            .flex_col()
            .gap_2()
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll();
        for row in rows {
            has_rows = true;
            let weak_enable = cx.weak_entity();
            let weak_approve = cx.weak_entity();
            let weak_uninstall = cx.weak_entity();
            let weak_update = cx.weak_entity();
            let id = row.id.clone();
            let id_enable = row.id.clone();
            let id_approve = row.id.clone();
            let id_uninstall = row.id.clone();
            let id_update = row.id.clone();
            let approved = row.approved;
            let enabled = row.enabled;
            let mut actions = div().flex().items_center().gap_2();
            if !enabled {
                actions = actions.child(
                    Button::new(format!("plugin-enable-{id}"))
                        .label(tr("plugins.enable"))
                        .compact()
                        .on_click(move |_, window, cx| {
                            let _ = weak_enable.update(cx, |this, cx| {
                                this.settings
                                    .disabled_plugins
                                    .retain(|item| item != &id_enable);
                                this.global_settings.disabled_plugins =
                                    this.settings.disabled_plugins.clone();
                                this.plugin_action_reload(window, cx);
                            });
                        }),
                );
            } else {
                actions = actions.child(
                    Button::new(format!("plugin-disable-{id}"))
                        .label(tr("plugins.disable"))
                        .compact()
                        .on_click(move |_, window, cx| {
                            let _ = weak_enable.update(cx, |this, cx| {
                                if !this.settings.disabled_plugins.contains(&id_enable) {
                                    this.settings.disabled_plugins.push(id_enable.clone());
                                }
                                this.global_settings.disabled_plugins =
                                    this.settings.disabled_plugins.clone();
                                this.plugin_action_reload(window, cx);
                            });
                        }),
                );
            }
            if !approved {
                actions = actions.child(
                    Button::new(format!("plugin-approve-{id}"))
                        .label(tr("plugins.approve"))
                        .compact()
                        .primary()
                        .on_click(move |_, window, cx| {
                            let _ = weak_approve.update(cx, |this, cx| {
                                let Some(manifest) = this.plugin_host.manifest(&id_approve) else {
                                    return;
                                };
                                match luban_plugin::install::approve(
                                    &this.plugin_host.root,
                                    manifest,
                                ) {
                                    Ok(_) => this.plugin_action_reload(window, cx),
                                    Err(error) => {
                                        this.plugin_manager.error = Some(error);
                                        cx.notify();
                                    }
                                }
                            });
                        }),
                );
            }
            if !row.manual {
                actions = actions.child(
                    Button::new(format!("plugin-update-{id}"))
                        .label(if row.reinstall {
                            tr("plugins.reinstall")
                        } else {
                            tr("plugins.update")
                        })
                        .compact()
                        .on_click(move |_, window, cx| {
                            let _ = weak_update.update(cx, |this, cx| {
                                match luban_plugin::install::update(
                                    &this.plugin_host.root,
                                    &id_update,
                                ) {
                                    Ok(_) => this.plugin_action_reload(window, cx),
                                    Err(error) => {
                                        this.plugin_manager.error = Some(error);
                                        cx.notify();
                                    }
                                }
                            });
                        }),
                );
            }
            actions = actions.child(
                Button::new(format!("plugin-uninstall-{id}"))
                    .label(tr("plugins.uninstall"))
                    .compact()
                    .on_click(move |_, _, cx| {
                        let _ = weak_uninstall.update(cx, |this, cx| {
                            this.plugin_manager.pending_uninstall = Some(id_uninstall.clone());
                            this.plugin_manager.error = None;
                            this.plugin_manager.message = None;
                            cx.notify();
                        });
                    }),
            );
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors.border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(format!("{} {}", row.name, row.version)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().colors.muted_foreground)
                                    .child(row.state),
                            ),
                    )
                    .when(!row.description.is_empty(), |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().colors.muted_foreground)
                                .child(row.description),
                        )
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(format!(
                                "{} · {}{}",
                                row.id,
                                if row.permissions.is_empty() {
                                    tr("plugins.no_permissions")
                                } else {
                                    row.permissions
                                },
                                if row.author.is_empty() {
                                    String::new()
                                } else {
                                    format!(" · {}", row.author)
                                }
                            )),
                    )
                    .child(actions),
            );
        }
        if !has_rows {
            list = list.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr("plugins.none_installed")),
            );
        }

        let mut settings_rows = div().flex().flex_col().gap_2();
        for (plugin_id, schema) in schemas {
            if schema.is_empty() {
                continue;
            }
            let current = self.plugin_host.settings_for(&plugin_id);
            settings_rows = settings_rows.child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(plugin_id.clone()),
            );
            for setting in schema {
                let value = current
                    .get(&setting.key)
                    .cloned()
                    .unwrap_or_else(|| luban_plugin::manifest::default_setting_value(&setting));
                let row_id = format!("plugin-setting-{plugin_id}-{}", setting.key);
                let plugin_id2 = plugin_id.clone();
                let key = setting.key.clone();
                let field = match setting.setting_type {
                    luban_plugin::manifest::SettingType::Boolean => {
                        let checked = value.as_bool().unwrap_or(false);
                        let plugin_id3 = plugin_id2.clone();
                        let key3 = key.clone();
                        Checkbox::new(row_id)
                            .label(setting.label.clone())
                            .checked(checked)
                            .on_click(cx.listener(move |this, checked, _, cx| {
                                this.apply_plugin_setting(
                                    &plugin_id3,
                                    &key3,
                                    serde_json::Value::Bool(*checked),
                                    cx,
                                );
                            }))
                            .into_any_element()
                    }
                    luban_plugin::manifest::SettingType::Number => {
                        let current_value = value.as_f64().unwrap_or(0.0);
                        let step = setting.step.unwrap_or(1.0);
                        let min = setting.min.unwrap_or(f64::NEG_INFINITY);
                        let max = setting.max.unwrap_or(f64::INFINITY);
                        let down = (current_value - step).max(min);
                        let up = (current_value + step).min(max);
                        let plugin_down = plugin_id2.clone();
                        let plugin_up = plugin_id2.clone();
                        let key_down = key.clone();
                        let key_up = key.clone();
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().text_sm().child(setting.label.clone()))
                            .child(
                                Button::new(format!("{row_id}-down"))
                                    .label("−")
                                    .compact()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.apply_plugin_setting(
                                            &plugin_down,
                                            &key_down,
                                            serde_json::json!(down),
                                            cx,
                                        );
                                    })),
                            )
                            .child(div().text_sm().child(format!("{current_value}")))
                            .child(
                                Button::new(format!("{row_id}-up"))
                                    .label("+")
                                    .compact()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.apply_plugin_setting(
                                            &plugin_up,
                                            &key_up,
                                            serde_json::json!(up),
                                            cx,
                                        );
                                    })),
                            )
                            .into_any_element()
                    }
                    luban_plugin::manifest::SettingType::Enum => {
                        let current_value = value.as_str().unwrap_or_default().to_string();
                        let values = setting.values.clone();
                        let weak_enum = cx.weak_entity();
                        let plugin_enum = plugin_id2.clone();
                        let key_enum = key.clone();
                        Button::new(row_id)
                            .label(format!("{}: {current_value}", setting.label))
                            .compact()
                            .dropdown_menu(move |menu, _window, _cx| {
                                let mut menu = menu;
                                for candidate in values.clone() {
                                    let weak = weak_enum.clone();
                                    let plugin = plugin_enum.clone();
                                    let key = key_enum.clone();
                                    let value = candidate.clone();
                                    menu = menu.item(PopupMenuItem::new(candidate).on_click(
                                        move |_, _, cx| {
                                            let _ = weak.update(cx, |this, cx| {
                                                this.apply_plugin_setting(
                                                    &plugin,
                                                    &key,
                                                    serde_json::Value::String(value.clone()),
                                                    cx,
                                                );
                                            });
                                        },
                                    ));
                                }
                                menu
                            })
                            .into_any_element()
                    }
                    luban_plugin::manifest::SettingType::String => {
                        let current_value = value.as_str().unwrap_or_default().to_string();
                        let key_string = format!("{plugin_id2}:{key}");
                        let input =
                            self.plugin_manager
                                .text_input(&key_string, &current_value, window, cx);
                        let weak_apply = cx.weak_entity();
                        let plugin_string = plugin_id2.clone();
                        let key_apply = key.clone();
                        let input_apply = input.clone();
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().text_sm().child(setting.label.clone()))
                            .child(Input::new(&input).h(px(28.)).flex_1())
                            .child(
                                Button::new(format!("{row_id}-apply"))
                                    .label(tr("common.apply"))
                                    .compact()
                                    .on_click(move |_, _, cx| {
                                        let value = input_apply.read(cx).value().to_string();
                                        let _ = weak_apply.update(cx, |this, cx| {
                                            this.apply_plugin_setting(
                                                &plugin_string,
                                                &key_apply,
                                                serde_json::Value::String(value),
                                                cx,
                                            );
                                        });
                                    }),
                            )
                            .into_any_element()
                    }
                };
                settings_rows = settings_rows.child(field);
            }
        }

        let content = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .gap_3()
            .p_5()
            .child(self.plugin_manager_header(cx))
            .child(self.plugin_manager_tabs(cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .child(
                        Checkbox::new("plugins-enabled")
                            .label(tr("plugins.enabled"))
                            .checked(plugins_enabled)
                            .on_click(move |this_checked, window, cx| {
                                let _ = weak_toggle.update(cx, |this, cx| {
                                    this.settings.plugins_enabled = *this_checked;
                                    this.global_settings.plugins_enabled = *this_checked;
                                    this.commit_settings();
                                    this.plugin_action_reload(window, cx);
                                });
                            }),
                    )
                    .child(
                        Checkbox::new("plugins-remove-data")
                            .label(tr("plugins.remove_data"))
                            .checked(remove_data)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.plugin_manager.remove_data = *checked;
                                cx.notify();
                            })),
                    )
                    .when(safe_mode, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().colors.muted_foreground)
                                .child(tr("plugins.safe_mode")),
                        )
                    }),
            )
            .when_some(message, |this, message| {
                this.child(div().text_xs().child(message))
            })
            .when_some(error, |this, error| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().colors.danger)
                        .child(error),
                )
            })
            .when(!host_errors.is_empty(), |this| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .text_xs()
                        .text_color(cx.theme().colors.muted_foreground)
                        .children(host_errors.iter().cloned().map(|error| div().child(error))),
                )
            })
            .when_some(pending_uninstall, |this, id| {
                let detail_key = if remove_data {
                    "plugins.uninstall_confirm_data"
                } else {
                    "plugins.uninstall_confirm"
                };
                let id_confirm = id.clone();
                let mut banner = div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors.danger)
                    .child(
                        div()
                            .text_sm()
                            .child(tr_args(detail_key, &[("plugin", id.clone())])),
                    );
                if !uninstall_dependents.is_empty() {
                    banner =
                        banner.child(div().text_xs().text_color(cx.theme().colors.danger).child(
                            tr_args(
                                "plugins.uninstall_dependents",
                                &[("plugins", uninstall_dependents.join(", "))],
                            ),
                        ));
                }
                this.child(
                    banner
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    Button::new("plugin-uninstall-cancel")
                                        .label(tr("common.cancel"))
                                        .compact()
                                        .on_click(move |_, _, cx| {
                                            let _ = weak_uninstall_cancel.update(cx, |this, cx| {
                                                this.plugin_manager.pending_uninstall = None;
                                                cx.notify();
                                            });
                                        }),
                                )
                                .child(
                                    Button::new("plugin-uninstall-confirm")
                                        .label(tr("plugins.uninstall_confirm_button"))
                                        .compact()
                                        .danger()
                                        .on_click(move |_, window, cx| {
                                            let _ =
                                                weak_uninstall_confirm.update(cx, |this, cx| {
                                                    this.uninstall_plugin(&id_confirm, window, cx);
                                                });
                                        }),
                                ),
                        )
                        .into_any_element(),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Input::new(&install_input).h(px(30.)).flex_1())
                    .child(
                        Button::new("plugin-install")
                            .label(tr("plugins.install"))
                            .primary()
                            .compact()
                            .on_click(move |_, window, cx| {
                                let _ = weak_install.update(cx, |this, cx| {
                                    let source = this
                                        .plugin_manager
                                        .install_source
                                        .read(cx)
                                        .value()
                                        .to_string();
                                    let source = source.trim().to_string();
                                    if source.is_empty() {
                                        this.plugin_manager.error =
                                            Some(tr("plugins.install_source_required"));
                                        cx.notify();
                                        return;
                                    }
                                    let replace = this.plugin_manager.pending_replace.as_deref()
                                        == Some(source.as_str());
                                    let result = if source.ends_with(".zip") {
                                        luban_plugin::install::install_zip_with(
                                            &this.plugin_host.root,
                                            std::path::Path::new(&source),
                                            replace,
                                        )
                                    } else if source.starts_with("http://")
                                        || source.starts_with("https://")
                                        || source.starts_with("git@")
                                        || source.ends_with(".git")
                                    {
                                        luban_plugin::install::install_git_with(
                                            &this.plugin_host.root,
                                            &source,
                                            None,
                                            replace,
                                        )
                                    } else {
                                        luban_plugin::install::install_local_with(
                                            &this.plugin_host.root,
                                            std::path::Path::new(&source),
                                            replace,
                                        )
                                    };
                                    match result {
                                        Ok(record) => {
                                            this.plugin_manager.pending_replace = None;
                                            this.plugin_manager.error = None;
                                            this.plugin_manager.message = Some(format!(
                                                "{} {}",
                                                tr("plugins.installed"),
                                                record.id
                                            ));
                                            this.plugin_action_reload(window, cx);
                                        }
                                        Err(error) => {
                                            if let Some(id) = error.strip_prefix(
                                                luban_plugin::install::REPLACE_CONFIRMATION,
                                            ) {
                                                this.plugin_manager.pending_replace = Some(source);
                                                this.plugin_manager.message = Some(tr_args(
                                                    "plugins.replace_confirm",
                                                    &[("plugin", id.to_string())],
                                                ));
                                                this.plugin_manager.error = None;
                                                cx.notify();
                                            } else {
                                                this.plugin_manager.pending_replace = None;
                                                this.plugin_manager.error = Some(error);
                                                cx.notify();
                                            }
                                        }
                                    }
                                });
                            }),
                    )
                    .child(
                        Button::new("plugin-reload")
                            .label(tr("plugins.reload"))
                            .compact()
                            .on_click(move |_, window, cx| {
                                let _ = weak_reload.update(cx, |this, cx| {
                                    this.plugin_action_reload(window, cx);
                                });
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .flex_1()
                    .min_h(px(0.))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child(tr("plugins.installed_title")),
                    )
                    .child(list),
            )
            .child(
                div()
                    .id("plugin-settings-scroll")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .max_h(px(180.))
                    .overflow_y_scroll()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child(tr("plugins.settings_title")),
                    )
                    .child(settings_rows),
            );
        self.overlay_card(760.0, Some(620.0), cx, content.into_any_element())
    }
}

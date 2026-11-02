use super::*;

impl LubanApp {
    // ------------------------------------------------------------------
    // Preferences and font picker
    // ------------------------------------------------------------------

    pub(super) fn close_preferences(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preferences.open = false;
        self.preferences.font_picker = None;
        let search = self.preferences.search.clone();
        search.update(cx, |input, cx| input.set_value(String::new(), window, cx));
        self.focus_editor(window, cx);
        cx.notify();
    }

    pub(super) fn open_preferences(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preferences.open = true;
        self.preferences
            .search
            .update(cx, |input, cx| input.focus(window, cx));
        let value = self
            .settings
            .tab_size_for(&self.preferences.language)
            .to_string();
        self.preferences
            .language_tab_size
            .update(cx, |input, cx| input.set_value(value, window, cx));
        cx.notify();
    }

    fn open_font_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(picker) = &self.preferences.font_picker {
            picker.filter.update(cx, |filter, cx| {
                filter.set_value(String::new(), window, cx);
                filter.focus(window, cx);
            });
            return;
        }
        if self.preferences.fonts.is_none() {
            self.preferences.fonts = Some(cx.text_system().all_font_names());
        }
        let fonts = self.preferences.fonts.clone().unwrap_or_default();
        let picker = FontPickerState::new(window, cx, fonts);
        picker
            .filter
            .update(cx, |filter, cx| filter.focus(window, cx));
        self.preferences.font_picker = Some(picker);
        cx.notify();
    }

    fn select_font_family(&mut self, family: String, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.font_family = family;
        self.preferences.font_picker = None;
        // The picker owned focus through its filter input; hand it back so
        // Escape and the global shortcuts keep reaching the app.
        self.focus_editor(window, cx);
        self.apply_settings(window, cx);
    }

    pub(super) fn render_preferences(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let weak = cx.weak_entity();
        let settings = self.settings.clone();
        let search_input = self.preferences.search.clone();
        let search_query = search_input.read(cx).value().to_string();
        let visible = settings_search::visible_group_keys(&search_query);
        let no_matches = !settings_search::has_matches(&search_query);
        let weak_theme_light = cx.weak_entity();
        let weak_theme_dark = cx.weak_entity();
        let weak_close = cx.weak_entity();
        let weak_font_picker = cx.weak_entity();
        let weak_zoom_in = cx.weak_entity();
        let weak_zoom_out = cx.weak_entity();
        let weak_tab_in = cx.weak_entity();
        let weak_tab_out = cx.weak_entity();
        let weak_lang_system = cx.weak_entity();
        let weak_lang_en = cx.weak_entity();
        let weak_lang_zh = cx.weak_entity();
        let weak_theme_system = cx.weak_entity();
        let weak_days_down = cx.weak_entity();
        let weak_days_up = cx.weak_entity();
        let weak_mb_down = cx.weak_entity();
        let weak_mb_up = cx.weak_entity();
        let weak_shortcuts = cx.weak_entity();

        let mut language_system = Button::new("prefs-lang-system")
            .label(tr("prefs.language_system"))
            .compact();
        if settings.ui_language == "system" {
            language_system = language_system.primary();
        }
        let language_system = language_system.on_click(move |_, window, cx| {
            let _ = weak_lang_system.update(cx, |this, cx| {
                this.set_ui_language("system", window, cx);
            });
        });

        let mut language_en = Button::new("prefs-lang-en")
            .label(tr("prefs.language_english"))
            .compact();
        if settings.ui_language == "en" {
            language_en = language_en.primary();
        }
        let language_en = language_en.on_click(move |_, window, cx| {
            let _ = weak_lang_en.update(cx, |this, cx| {
                this.set_ui_language("en", window, cx);
            });
        });

        let mut language_zh = Button::new("prefs-lang-zh")
            .label(tr("prefs.language_chinese"))
            .compact();
        if settings.ui_language == "zh-CN" {
            language_zh = language_zh.primary();
        }
        let language_zh = language_zh.on_click(move |_, window, cx| {
            let _ = weak_lang_zh.update(cx, |this, cx| {
                this.set_ui_language("zh-CN", window, cx);
            });
        });

        let toggle_row =
            |label: String,
             value: bool,
             id: &'static str,
             weak: WeakEntity<Self>,
             set: fn(&mut Self, bool, &mut Window, &mut Context<Self>)| {
                Checkbox::new(id).label(label).checked(value).on_click(
                    move |checked, window, cx| {
                        let checked = *checked;
                        let _ = weak.update(cx, |this, cx| set(this, checked, window, cx));
                    },
                )
            };

        let content = div()
            .flex()
            .flex_col()
            .h_full()
            .min_h(px(0.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .p_5()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tr("prefs.title")),
                    )
                    .child(
                        Button::new("prefs-close")
                            .label(tr("common.close"))
                            .compact()
                            .on_click(move |_, window, cx| {
                                let _ = weak_close.update(cx, |this, cx| {
                                    this.close_preferences(window, cx);
                                });
                            }),
                    ),
            )
            .child(
                div().px_5().pb_2().child(
                    Input::new(&search_input)
                        .h(px(30.))
                        .aria_label(tr("prefs.search_placeholder")),
                ),
            )
            .child(
                div()
                    .id("prefs-scroll")
                    .flex()
                    .flex_col()
                    .gap_3()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scrollbar()
                    .px_5()
                    .pb_5()
                    .when(no_matches, |this| {
                        this.child(
                            div()
                                .py_4()
                                .text_sm()
                                .text_color(cx.theme().colors.muted_foreground)
                                .child(tr("prefs.no_matches")),
                        )
                    })
                    .when(visible.contains("language"), |this| {
                        this.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_3()
                                        .child(
                                            div().w(px(140.)).text_sm().child(tr("prefs.language")),
                                        )
                                        .child(language_system)
                                        .child(language_en)
                                        .child(language_zh),
                                )
                                .when(settings.ui_language == "system", |this| {
                                    this.child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().colors.muted_foreground)
                                            .child(tr_args(
                                                "prefs.language_current",
                                                &[("language", active_language_name().to_string())],
                                            )),
                                    )
                                }),
                        )
                    })
                    .when(visible.contains("appearance"), |this| {
                        this.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(div().w(px(140.)).text_sm().child(tr("prefs.theme")))
                                .child(
                                    Button::new("prefs-light")
                                        .label(tr("prefs.light"))
                                        .on_click(move |_, window, cx| {
                                            let _ = weak_theme_light.update(cx, |this, cx| {
                                                this.settings.theme = ThemeChoice::Light;
                                                this.apply_settings(window, cx);
                                            });
                                        }),
                                )
                                .child(Button::new("prefs-dark").label(tr("prefs.dark")).on_click(
                                    move |_, window, cx| {
                                        let _ = weak_theme_dark.update(cx, |this, cx| {
                                            this.settings.theme = ThemeChoice::Dark;
                                            this.apply_settings(window, cx);
                                        });
                                    },
                                ))
                                .child(
                                    Button::new("prefs-system")
                                        .label(tr("prefs.system_theme"))
                                        .on_click(move |_, window, cx| {
                                            let _ = weak_theme_system.update(cx, |this, cx| {
                                                this.settings.theme = ThemeChoice::System;
                                                this.apply_settings(window, cx);
                                            });
                                        }),
                                ),
                        )
                    })
                    .when(visible.contains("appearance"), |this| {
                        this.child({
                            let package_themes = self.packages.themes.clone();
                            let syntax_label = if let Some(package) = package_themes
                                .iter()
                                .find(|theme| theme.id == settings.syntax_theme)
                            {
                                package.name.clone()
                            } else if settings.syntax_theme == "custom" {
                                settings
                                    .syntax_theme_path
                                    .as_ref()
                                    .and_then(|path| path.file_stem())
                                    .map(|stem| stem.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| tr("prefs.syntax_custom"))
                            } else if settings.syntax_theme == "default"
                                || settings.syntax_theme.is_empty()
                            {
                                tr("prefs.syntax_default")
                            } else {
                                THEME_OPTIONS
                                    .iter()
                                    .find(|option| option.id == settings.syntax_theme)
                                    .map(|option| option.label.to_string())
                                    .unwrap_or_else(|| settings.syntax_theme.clone())
                            };
                            let weak_theme = weak.clone();
                            let weak_import = weak.clone();
                            let custom_theme = settings.syntax_theme_path.as_ref().map(|path| {
                                let name = path
                                    .file_stem()
                                    .map(|stem| stem.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| tr("prefs.syntax_custom"));
                                format!("{}: {name}", tr("prefs.syntax_custom"))
                            });
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(div().w(px(140.)).text_sm().child(tr("prefs.syntax_theme")))
                                .child(
                                    Button::new("prefs-syntax-theme")
                                        .label(syntax_label)
                                        .dropdown_menu(move |mut menu, window, _cx| {
                                            let weak_default = weak_theme.clone();
                                            menu = menu.item(
                                                PopupMenuItem::new(tr("prefs.syntax_default"))
                                                    .on_click(move |_, window, cx| {
                                                        let _ =
                                                            weak_default.update(cx, |this, cx| {
                                                                this.set_syntax_theme(
                                                                    "default", window, cx,
                                                                );
                                                            });
                                                    }),
                                            );
                                            for option in THEME_OPTIONS {
                                                let weak = weak_theme.clone();
                                                let id = option.id;
                                                menu = menu.item(
                                                    PopupMenuItem::new(option.label).on_click(
                                                        move |_, window, cx| {
                                                            let _ = weak.update(cx, |this, cx| {
                                                                this.set_syntax_theme(
                                                                    id, window, cx,
                                                                );
                                                            });
                                                        },
                                                    ),
                                                );
                                            }
                                            if let Some(label) = custom_theme.clone() {
                                                let weak = weak_theme.clone();
                                                menu =
                                                    menu.item(PopupMenuItem::new(label).on_click(
                                                        move |_, window, cx| {
                                                            let _ = weak.update(cx, |this, cx| {
                                                                this.set_syntax_theme(
                                                                    syntax_theme::CUSTOM_ID,
                                                                    window,
                                                                    cx,
                                                                );
                                                            });
                                                        },
                                                    ));
                                            }
                                            for theme in &package_themes {
                                                let weak = weak_theme.clone();
                                                let id = theme.id.clone();
                                                menu = menu.item(
                                                    PopupMenuItem::new(theme.name.clone())
                                                        .on_click(move |_, window, cx| {
                                                            let _ = weak.update(cx, |this, cx| {
                                                                this.set_syntax_theme(
                                                                    &id, window, cx,
                                                                );
                                                            });
                                                        }),
                                                );
                                            }
                                            Self::fit_menu(menu, window)
                                        }),
                                )
                                .child(
                                    Button::new("prefs-import-theme")
                                        .label(tr("prefs.import_theme"))
                                        .compact()
                                        .on_click(move |_, window, cx| {
                                            let _ = weak_import.update(cx, |this, cx| {
                                                this.open_syntax_theme_dialog(window, cx);
                                            });
                                        }),
                                )
                        })
                    })
                    .when(visible.contains("font"), |this| {
                        this.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(div().w(px(140.)).text_sm().child(tr("prefs.font_size")))
                                .child(
                                    Button::new("prefs-font-minus")
                                        .label("−")
                                        .compact()
                                        .on_click(move |_, _, cx| {
                                            let _ = weak_zoom_out
                                                .update(cx, |this, cx| this.zoom(-1.0, cx));
                                        }),
                                )
                                .child(div().text_sm().child(tr_args(
                                    "prefs.pixels",
                                    &[("size", format!("{:.0}", settings.font_size))],
                                )))
                                .child(
                                    Button::new("prefs-font-plus")
                                        .label("+")
                                        .compact()
                                        .on_click(move |_, _, cx| {
                                            let _ = weak_zoom_in
                                                .update(cx, |this, cx| this.zoom(1.0, cx));
                                        }),
                                ),
                        )
                    })
                    .when(visible.contains("font"), |this| {
                        this.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(div().w(px(140.)).text_sm().child(tr("prefs.font_family")))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .truncate()
                                        .text_sm()
                                        .text_color(cx.theme().colors.muted_foreground)
                                        .child(if settings.font_family.is_empty() {
                                            tr("prefs.font_default")
                                        } else {
                                            settings.font_family.clone()
                                        }),
                                )
                                .child(
                                    Button::new("prefs-choose-font")
                                        .label(tr("prefs.choose_font"))
                                        .compact()
                                        .on_click(move |_, window, cx| {
                                            let _ = weak_font_picker.update(cx, |this, cx| {
                                                this.open_font_picker(window, cx);
                                            });
                                        }),
                                ),
                        )
                    })
                    .when(visible.contains("editor"), |this| {
                        this.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(div().w(px(140.)).text_sm().child(tr("prefs.tab_size")))
                                .child(
                                    Button::new("prefs-tab-minus")
                                        .label("−")
                                        .compact()
                                        .on_click(move |_, window, cx| {
                                            let _ = weak_tab_out.update(cx, |this, cx| {
                                                this.settings.tab_size =
                                                    this.settings.tab_size.saturating_sub(1).max(1);
                                                this.apply_settings(window, cx);
                                            });
                                        }),
                                )
                                .child(div().text_sm().child(format!("{}", settings.tab_size)))
                                .child(Button::new("prefs-tab-plus").label("+").compact().on_click(
                                    move |_, window, cx| {
                                        let _ = weak_tab_in.update(cx, |this, cx| {
                                            this.settings.tab_size =
                                                (this.settings.tab_size + 1).min(16);
                                            this.apply_settings(window, cx);
                                        });
                                    },
                                ))
                                .child(toggle_row(
                                    tr("prefs.insert_spaces"),
                                    settings.use_spaces,
                                    "prefs-spaces",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.use_spaces = checked;
                                        this.apply_settings(window, cx);
                                    },
                                )),
                        )
                    })
                    .when(visible.contains("editor"), |this| {
                        this.child({
                            let language = self.preferences.language.clone();
                            let overrides = self.language_override().cloned().unwrap_or_default();
                            let weak_language = weak.clone();
                            let weak_tab_apply = weak.clone();
                            let weak_spaces_inherit = weak.clone();
                            let weak_spaces_on = weak.clone();
                            let weak_spaces_off = weak.clone();
                            let weak_eol_inherit = weak.clone();
                            let weak_eol_lf = weak.clone();
                            let weak_eol_crlf = weak.clone();
                            let weak_eol_cr = weak.clone();
                            let tab_size_input = self.preferences.language_tab_size.clone();
                            let selected_language = language.clone();
                            let weak_language_menu = weak_language.clone();
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(tr("prefs.per_language")),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_3()
                                        .child(
                                            div()
                                                .w(px(140.))
                                                .text_sm()
                                                .child(tr("prefs.language_select")),
                                        )
                                        .child(
                                            Button::new("prefs-language-select")
                                                .label(language_display_name(&language))
                                                .dropdown_menu(move |mut menu, window, _cx| {
                                                    for name in language::names() {
                                                        let selected =
                                                            selected_language.as_str() == name;
                                                        let name = name.to_string();
                                                        let weak = weak_language_menu.clone();
                                                        menu = menu.item(
                                                            PopupMenuItem::new(
                                                                language_display_name(&name),
                                                            )
                                                            .checked(selected)
                                                            .on_click(move |_, window, cx| {
                                                                let name = name.clone();
                                                                let _ =
                                                                    weak.update(cx, |this, cx| {
                                                                        this.preferences.language =
                                                                            name.clone();
                                                                        let value = this
                                                                            .settings
                                                                            .tab_size_for(&name)
                                                                            .to_string();
                                                                        this.preferences
                                                                            .language_tab_size
                                                                            .update(
                                                                                cx,
                                                                                |input, cx| {
                                                                                    input.set_value(
                                                                                        value,
                                                                                        window, cx,
                                                                                    )
                                                                                },
                                                                            );
                                                                        cx.notify();
                                                                    });
                                                            }),
                                                        );
                                                    }
                                                    Self::fit_menu(menu, window)
                                                }),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .w(px(140.))
                                                .text_sm()
                                                .child(tr("prefs.language_tab_size")),
                                        )
                                        .child(Input::new(&tab_size_input).w(px(60.)).h(px(28.)))
                                        .child(
                                            Button::new("prefs-language-tab-apply")
                                                .label(tr("prefs.apply"))
                                                .compact()
                                                .on_click(move |_, _, cx| {
                                                    let _ =
                                                        weak_tab_apply.update(cx, |this, cx| {
                                                            this.apply_language_tab_size(cx);
                                                        });
                                                }),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .w(px(140.))
                                                .text_sm()
                                                .child(tr("prefs.language_indent")),
                                        )
                                        .child(
                                            Button::new("prefs-language-spaces-inherit")
                                                .label(tr("prefs.inherit"))
                                                .compact()
                                                .when(overrides.use_spaces.is_none(), |button| {
                                                    button.primary()
                                                })
                                                .on_click(move |_, _, cx| {
                                                    let _ = weak_spaces_inherit.update(
                                                        cx,
                                                        |this, cx| {
                                                            this.set_language_use_spaces(None, cx);
                                                        },
                                                    );
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-language-spaces-on")
                                                .label(tr("prefs.spaces"))
                                                .compact()
                                                .when(
                                                    overrides.use_spaces == Some(true),
                                                    |button| button.primary(),
                                                )
                                                .on_click(move |_, _, cx| {
                                                    let _ =
                                                        weak_spaces_on.update(cx, |this, cx| {
                                                            this.set_language_use_spaces(
                                                                Some(true),
                                                                cx,
                                                            );
                                                        });
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-language-spaces-off")
                                                .label(tr("prefs.tabs"))
                                                .compact()
                                                .when(
                                                    overrides.use_spaces == Some(false),
                                                    |button| button.primary(),
                                                )
                                                .on_click(move |_, _, cx| {
                                                    let _ =
                                                        weak_spaces_off.update(cx, |this, cx| {
                                                            this.set_language_use_spaces(
                                                                Some(false),
                                                                cx,
                                                            );
                                                        });
                                                }),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .w(px(140.))
                                                .text_sm()
                                                .child(tr("prefs.language_eol")),
                                        )
                                        .child(
                                            Button::new("prefs-language-eol-inherit")
                                                .label(tr("prefs.inherit"))
                                                .compact()
                                                .when(overrides.eol.is_none(), |button| {
                                                    button.primary()
                                                })
                                                .on_click(move |_, _, cx| {
                                                    let _ =
                                                        weak_eol_inherit.update(cx, |this, cx| {
                                                            this.set_language_eol(None, cx);
                                                        });
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-language-eol-lf")
                                                .label("LF")
                                                .compact()
                                                .when(overrides.eol == Some(Eol::Lf), |button| {
                                                    button.primary()
                                                })
                                                .on_click(move |_, _, cx| {
                                                    let _ = weak_eol_lf.update(cx, |this, cx| {
                                                        this.set_language_eol(Some(Eol::Lf), cx);
                                                    });
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-language-eol-crlf")
                                                .label("CRLF")
                                                .compact()
                                                .when(overrides.eol == Some(Eol::CrLf), |button| {
                                                    button.primary()
                                                })
                                                .on_click(move |_, _, cx| {
                                                    let _ = weak_eol_crlf.update(cx, |this, cx| {
                                                        this.set_language_eol(Some(Eol::CrLf), cx);
                                                    });
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-language-eol-cr")
                                                .label("CR")
                                                .compact()
                                                .when(overrides.eol == Some(Eol::Cr), |button| {
                                                    button.primary()
                                                })
                                                .on_click(move |_, _, cx| {
                                                    let _ = weak_eol_cr.update(cx, |this, cx| {
                                                        this.set_language_eol(Some(Eol::Cr), cx);
                                                    });
                                                }),
                                        ),
                                )
                        })
                    })
                    .when(visible.contains("editor"), |this| {
                        this.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(tr("prefs.editor")),
                                )
                                .child(toggle_row(
                                    tr("prefs.line_numbers"),
                                    settings.show_line_numbers,
                                    "prefs-linenumbers",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.show_line_numbers = checked;
                                        this.apply_settings(window, cx);
                                    },
                                ))
                                .child(toggle_row(
                                    tr("prefs.word_wrap"),
                                    settings.word_wrap,
                                    "prefs-wrap",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.word_wrap = checked;
                                        this.apply_settings(window, cx);
                                    },
                                ))
                                .child(toggle_row(
                                    tr("prefs.match_brackets"),
                                    settings.highlight_matching_brackets,
                                    "prefs-brackets",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.highlight_matching_brackets = checked;
                                        this.apply_settings(window, cx);
                                    },
                                ))
                                .child(toggle_row(
                                    tr("prefs.trim_on_save"),
                                    settings.trim_trailing_whitespace_on_save,
                                    "prefs-trim",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.trim_trailing_whitespace_on_save = checked;
                                        this.apply_settings(window, cx);
                                    },
                                )),
                        )
                    })
                    .when(visible.contains("session"), |this| {
                        this.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(tr("prefs.session")),
                                )
                                .child(toggle_row(
                                    tr("prefs.restore_session"),
                                    settings.restore_session,
                                    "prefs-restore",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.restore_session = checked;
                                        this.apply_settings(window, cx);
                                    },
                                ))
                                .child(toggle_row(
                                    tr("prefs.autosave"),
                                    settings.autosave_session,
                                    "prefs-autosave",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.autosave_session = checked;
                                        this.apply_settings(window, cx);
                                    },
                                ))
                                .child(toggle_row(
                                    tr("prefs.hidden_files"),
                                    settings.show_hidden_files,
                                    "prefs-hidden",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.show_hidden_files = checked;
                                        this.explorer.refresh();
                                        this.apply_settings(window, cx);
                                    },
                                )),
                        )
                    })
                    .when(
                        visible.contains("history") || visible.contains("keybindings"),
                        |this| {
                            this.child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child(tr("prefs.local_history")),
                                    )
                                    .child(toggle_row(
                                        tr("prefs.local_history_enabled"),
                                        settings.local_history_enabled,
                                        "prefs-history-enabled",
                                        weak.clone(),
                                        |this, checked, window, cx| {
                                            this.settings.local_history_enabled = checked;
                                            this.apply_settings(window, cx);
                                        },
                                    ))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_3()
                                            .child(
                                                div()
                                                    .w(px(140.))
                                                    .text_sm()
                                                    .child(tr("prefs.local_history_days")),
                                            )
                                            .child(
                                                Button::new("prefs-history-days-down")
                                                    .label("−")
                                                    .compact()
                                                    .on_click(move |_, window, cx| {
                                                        let _ = weak_days_down.update(
                                                            cx,
                                                            |this, cx| {
                                                                this.settings.local_history_days =
                                                                    this.settings
                                                                        .local_history_days
                                                                        .saturating_sub(1)
                                                                        .max(1);
                                                                this.apply_settings(window, cx);
                                                            },
                                                        );
                                                    }),
                                            )
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .child(settings.local_history_days.to_string()),
                                            )
                                            .child(
                                                Button::new("prefs-history-days-up")
                                                    .label("+")
                                                    .compact()
                                                    .on_click(move |_, window, cx| {
                                                        let _ =
                                                            weak_days_up.update(cx, |this, cx| {
                                                                this.settings.local_history_days =
                                                                    (this
                                                                        .settings
                                                                        .local_history_days
                                                                        + 1)
                                                                    .min(3650);
                                                                this.apply_settings(window, cx);
                                                            });
                                                    }),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_3()
                                            .child(
                                                div()
                                                    .w(px(140.))
                                                    .text_sm()
                                                    .child(tr("prefs.local_history_size")),
                                            )
                                            .child(
                                                Button::new("prefs-history-mb-down")
                                                    .label("−")
                                                    .compact()
                                                    .on_click(move |_, window, cx| {
                                                        let _ =
                                                            weak_mb_down.update(cx, |this, cx| {
                                                                this.settings
                                                                    .local_history_max_mb = this
                                                                    .settings
                                                                    .local_history_max_mb
                                                                    .saturating_sub(1)
                                                                    .max(1);
                                                                this.apply_settings(window, cx);
                                                            });
                                                    }),
                                            )
                                            .child(
                                                div().text_sm().child(
                                                    settings.local_history_max_mb.to_string(),
                                                ),
                                            )
                                            .child(
                                                Button::new("prefs-history-mb-up")
                                                    .label("+")
                                                    .compact()
                                                    .on_click(move |_, window, cx| {
                                                        let _ =
                                                            weak_mb_up.update(cx, |this, cx| {
                                                                this.settings
                                                                    .local_history_max_mb = (this
                                                                    .settings
                                                                    .local_history_max_mb
                                                                    + 1)
                                                                .min(4096);
                                                                this.apply_settings(window, cx);
                                                            });
                                                    }),
                                            ),
                                    )
                                    .child(
                                        Button::new("prefs-shortcuts")
                                            .label(tr("shortcuts.customize"))
                                            .compact()
                                            .on_click(move |_, _, cx| {
                                                let _ = weak_shortcuts.update(cx, |this, cx| {
                                                    this.open_shortcut_editor(cx);
                                                });
                                            }),
                                    ),
                            )
                        },
                    )
                    .when(visible.contains("view"), |this| {
                        this.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(tr("prefs.view")),
                                )
                                .child(toggle_row(
                                    tr("view.whitespace"),
                                    settings.show_whitespace,
                                    "prefs-whitespace",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.show_whitespace = checked;
                                        this.apply_settings(window, cx);
                                    },
                                ))
                                .child(toggle_row(
                                    tr("view.indent_guides"),
                                    settings.show_indent_guides,
                                    "prefs-indent-guides",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.show_indent_guides = checked;
                                        this.apply_settings(window, cx);
                                    },
                                ))
                                .child(toggle_row(
                                    tr("view.ruler"),
                                    settings.show_ruler,
                                    "prefs-ruler",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.show_ruler = checked;
                                        this.apply_settings(window, cx);
                                    },
                                ))
                                .child(toggle_row(
                                    tr("view.document_map"),
                                    settings.show_document_map,
                                    "prefs-document-map",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.show_document_map = checked;
                                        this.apply_settings(window, cx);
                                    },
                                ))
                                .child(toggle_row(
                                    tr("view.bracket_colors"),
                                    settings.bracket_colors,
                                    "prefs-bracket-colors",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.bracket_colors = checked;
                                        this.apply_settings(window, cx);
                                    },
                                ))
                                .child(toggle_row(
                                    tr("view.outline"),
                                    settings.show_outline,
                                    "prefs-outline",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.show_outline = checked;
                                        this.apply_settings(window, cx);
                                    },
                                ))
                                .child(toggle_row(
                                    tr("view.breadcrumbs"),
                                    settings.show_breadcrumbs,
                                    "prefs-breadcrumbs",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.show_breadcrumbs = checked;
                                        this.apply_settings(window, cx);
                                    },
                                )),
                        )
                    })
                    .when(visible.contains("workspace"), |this| {
                        let weak_open = weak.clone();
                        let weak_save = weak.clone();
                        let weak_save_as = weak.clone();
                        let weak_add = weak.clone();
                        let weak_close_workspace = weak.clone();
                        let roots = if self.workspace_roots.is_empty() {
                            tr("workspace.none")
                        } else {
                            self.workspace_roots
                                .iter()
                                .map(|root| root.display().to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        };
                        let name = if self.workspace_name.is_empty() {
                            tr("workspace.none")
                        } else {
                            self.workspace_name.clone()
                        };
                        let file = self
                            .workspace_file
                            .as_ref()
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| tr("workspace.memory_only"));
                        this.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(tr("prefs.workspace")),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .gap_2()
                                        .text_xs()
                                        .text_color(cx.theme().colors.muted_foreground)
                                        .child(tr_args("prefs.workspace_name", &[("name", name)]))
                                        .child(tr_args("prefs.workspace_file", &[("path", file)])),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().colors.muted_foreground)
                                        .child(roots),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .gap_2()
                                        .flex_wrap()
                                        .child(
                                            Button::new("prefs-workspace-open")
                                                .label(tr("workspace.open"))
                                                .compact()
                                                .on_click(move |_, window, cx| {
                                                    let _ = weak_open.update(cx, |this, cx| {
                                                        this.open_workspace_dialog(window, cx);
                                                    });
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-workspace-save")
                                                .label(tr("workspace.save"))
                                                .compact()
                                                .on_click(move |_, window, cx| {
                                                    let _ = weak_save.update(cx, |this, cx| {
                                                        this.save_workspace(window, cx);
                                                    });
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-workspace-save-as")
                                                .label(tr("workspace.save_as"))
                                                .compact()
                                                .on_click(move |_, window, cx| {
                                                    let _ = weak_save_as.update(cx, |this, cx| {
                                                        this.save_workspace_as(window, cx);
                                                    });
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-workspace-add")
                                                .label(tr("workspace.add_folder"))
                                                .compact()
                                                .on_click(move |_, window, cx| {
                                                    let _ = weak_add.update(cx, |this, cx| {
                                                        this.open_add_folder_dialog(window, cx);
                                                    });
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-workspace-close")
                                                .label(tr("workspace.close"))
                                                .compact()
                                                .on_click(move |_, window, cx| {
                                                    let _ = weak_close_workspace.update(
                                                        cx,
                                                        |this, cx| {
                                                            this.close_workspace(window, cx);
                                                        },
                                                    );
                                                }),
                                        ),
                                )
                                .child(toggle_row(
                                    tr("ff.respect_gitignore"),
                                    settings.respect_gitignore,
                                    "prefs-respect-gitignore",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.respect_gitignore = checked;
                                        this.find_in_files.respect_gitignore = checked;
                                        this.apply_settings(window, cx);
                                        this.rebuild_index(cx);
                                        if this.find_in_files.open && this.find_in_files.workspace {
                                            this.start_find_in_files(cx);
                                        }
                                    },
                                ))
                                .child(toggle_row(
                                    tr("prefs.hidden_files"),
                                    settings.show_hidden_files,
                                    "prefs-workspace-hidden",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.show_hidden_files = checked;
                                        this.apply_settings(window, cx);
                                        this.rebuild_index(cx);
                                    },
                                )),
                        )
                    })
                    .when(visible.contains("files"), |this| {
                        let weak_limit_down = weak.clone();
                        let weak_limit_up = weak.clone();
                        this.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(tr("prefs.files")),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_3()
                                        .child(
                                            div()
                                                .w(px(200.))
                                                .text_sm()
                                                .child(tr("prefs.max_file_size")),
                                        )
                                        .child(
                                            Button::new("prefs-max-size-down")
                                                .label("−")
                                                .compact()
                                                .on_click(move |_, window, cx| {
                                                    let _ =
                                                        weak_limit_down.update(cx, |this, cx| {
                                                            this.settings.max_file_size_mb = this
                                                                .settings
                                                                .max_file_size_mb
                                                                .saturating_sub(64)
                                                                .max(1);
                                                            this.apply_settings(window, cx);
                                                        });
                                                }),
                                        )
                                        .child(
                                            div()
                                                .text_sm()
                                                .child(format!("{} MB", settings.max_file_size_mb)),
                                        )
                                        .child(
                                            Button::new("prefs-max-size-up")
                                                .label("+")
                                                .compact()
                                                .on_click(move |_, window, cx| {
                                                    let _ = weak_limit_up.update(cx, |this, cx| {
                                                        this.settings.max_file_size_mb =
                                                            (this.settings.max_file_size_mb + 64)
                                                                .min(4096);
                                                        this.apply_settings(window, cx);
                                                    });
                                                }),
                                        ),
                                ),
                        )
                    })
                    .when(visible.contains("backup"), |this| {
                        let weak_export = weak.clone();
                        let weak_import = weak.clone();
                        let weak_reset = weak.clone();
                        this.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(tr("prefs.import_export")),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .gap_2()
                                        .child(
                                            Button::new("prefs-export-settings")
                                                .label(tr("settings.export"))
                                                .compact()
                                                .on_click(move |_, window, cx| {
                                                    let _ = weak_export.update(cx, |this, cx| {
                                                        this.open_export_settings_dialog(
                                                            window, cx,
                                                        );
                                                    });
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-import-settings")
                                                .label(tr("settings.import"))
                                                .compact()
                                                .on_click(move |_, window, cx| {
                                                    let _ = weak_import.update(cx, |this, cx| {
                                                        this.open_import_settings_dialog(
                                                            window, cx,
                                                        );
                                                    });
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-reset-settings")
                                                .label(tr("settings.reset"))
                                                .compact()
                                                .on_click(move |_, _, cx| {
                                                    let _ = weak_reset.update(cx, |this, cx| {
                                                        this.confirm = Some(ConfirmState::new(
                                                            CloseIntent::ResetSettings,
                                                            tr("settings.reset_title"),
                                                            tr("settings.reset_detail"),
                                                        ));
                                                        cx.notify();
                                                    });
                                                }),
                                        ),
                                ),
                        )
                    })
                    .when(visible.contains("plugins"), |this| {
                        let weak_manage = weak.clone();
                        let weak_store = weak.clone();
                        let weak_plugins = weak.clone();
                        let weak_packages = weak.clone();
                        let plugins_enabled = self.settings.plugins_enabled;
                        let safe_mode = self.safe_mode;
                        this.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(tr("prefs.plugins")),
                                )
                                .child(
                                    Checkbox::new("prefs-plugins-enabled")
                                        .label(tr("plugins.enabled"))
                                        .checked(plugins_enabled)
                                        .on_click(move |checked, window, cx| {
                                            let _ = weak_plugins.update(cx, |this, cx| {
                                                this.settings.plugins_enabled = *checked;
                                                this.global_settings.plugins_enabled = *checked;
                                                this.commit_settings();
                                                this.plugin_action_reload(window, cx);
                                            });
                                        }),
                                )
                                .when(safe_mode, |this| {
                                    this.child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().colors.muted_foreground)
                                            .child(tr("plugins.safe_mode")),
                                    )
                                })
                                .child(
                                    div()
                                        .flex()
                                        .gap_2()
                                        .child(
                                            Button::new("prefs-plugins-manage")
                                                .label(tr("plugins.manage"))
                                                .compact()
                                                .on_click(move |_, window, cx| {
                                                    let _ = weak_manage.update(cx, |this, cx| {
                                                        this.plugin_manager.open(window, cx);
                                                        cx.notify();
                                                    });
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-plugins-store")
                                                .label(tr("menu.plugins.store"))
                                                .compact()
                                                .on_click(move |_, window, cx| {
                                                    let _ = weak_store.update(cx, |this, cx| {
                                                        this.open_plugin_store(window, cx);
                                                    });
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-packages-reload")
                                                .label(tr("packages.reload"))
                                                .compact()
                                                .on_click(move |_, window, cx| {
                                                    let _ = weak_packages.update(cx, |this, cx| {
                                                        this.reload_packages(window, cx);
                                                    });
                                                }),
                                        ),
                                ),
                        )
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(tr("prefs.footer")),
                    ),
            )
            .into_any_element();
        let _ = window;
        self.overlay_card(680.0, Some(640.0), cx, content)
    }

    pub(super) fn render_font_picker(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(picker) = &self.preferences.font_picker else {
            return div().into_any_element();
        };
        let filter = picker.filter.clone();
        let filter_text = filter.read(cx).value().to_string();
        let current = self.settings.font_family.clone();
        let weak = cx.weak_entity();
        let default_selected = current.is_empty();
        let weak_default = weak.clone();
        let mut list = vec![div()
            .id("font-default")
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .text_sm()
            .cursor_pointer()
            .when(default_selected, |this| {
                this.bg(cx.theme().colors.secondary)
            })
            .hover(|this| this.bg(cx.theme().colors.secondary_hover))
            .child(if default_selected { "•" } else { " " })
            .child(tr("prefs.font_default"))
            .on_click(move |_, window, cx| {
                let _ = weak_default.update(cx, |this, cx| {
                    this.select_font_family(String::new(), window, cx);
                });
            })
            .into_any_element()];
        for name in matching_fonts(&picker.fonts, &filter_text) {
            let selected = name == current;
            let name_owned = name.to_string();
            let weak_item = weak.clone();
            list.push(
                div()
                    .id(format!("font:{name_owned}"))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |this| this.bg(cx.theme().colors.secondary))
                    .hover(|this| this.bg(cx.theme().colors.secondary_hover))
                    .child(if selected { "•" } else { " " })
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .child(name_owned.clone()),
                    )
                    .on_click(move |_, window, cx| {
                        let _ = weak_item.update(cx, |this, cx| {
                            this.select_font_family(name_owned.clone(), window, cx);
                        });
                    })
                    .into_any_element(),
            );
        }
        let weak_close = cx.weak_entity();
        let content = div()
            .flex()
            .flex_col()
            .h_full()
            .min_h(px(0.))
            .gap_2()
            .p_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tr("prefs.font_picker_title")),
                    )
                    .child(
                        Button::new("font-picker-close")
                            .label(tr("common.close"))
                            .compact()
                            .on_click(move |_, window, cx| {
                                let _ = weak_close.update(cx, |this, cx| {
                                    this.preferences.font_picker = None;
                                    this.focus_editor(window, cx);
                                    cx.notify();
                                });
                            }),
                    ),
            )
            .child(Input::new(&filter).h(px(30.)))
            .child(
                div()
                    .id("font-list")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scrollbar()
                    .children(list),
            )
            .into_any_element();
        self.overlay_card(480.0, Some(560.0), cx, content)
    }
}

//! Application-side plugin event pump.
//!
//! Collects app events into a queue and dispatches them to plugin runtimes on
//! a 50 ms tick: at most [`MAX_DISPATCHES_PER_TICK`] dispatches per tick,
//! `on_edit` after [`EDIT_STABLE_AFTER`] of edit stability, due timers fire on
//! the same tick, and a plugin that failed too often is quarantined (auto-
//! disabled and persisted) after the dispatches that pushed it over the limit.
//!
//! # Emit-site coverage
//!
//! User-visible document events are emitted at these sites (each guarded by
//! [`LubanApp::plugin_emit`]'s event-context suppression, so a plugin's own
//! write-backs never cascade back into plugins):
//!
//! - `DocumentOpened`: the synchronous fresh-open success path of
//!   `open_path` (which `open_path_at`, the file picker and drag-and-drop all
//!   funnel through for new tabs).
//! - `DocumentSaved`: the two synchronous save completions —
//!   `save_tab_at_with` (Ctrl+S and every batch save) and `save_as_request`
//!   (first save of an untitled document and every Save As).
//! - `ActiveDocumentChanged`: `activate_tab_index`, `next_tab`,
//!   `close_tab_at_inner` (including the last-tab-close branch that opens a
//!   fresh untitled document) and `open_path`'s already-open branch.
//!
//! Deliberately not covered: huge-file and background/controller opens plus
//! the huge-file async save completion (`finish_huge_save`) — the snapshot
//! contract (`plugin_document_snapshot`) never exposes huge, partial-range,
//! large-file, non-text or read-only documents, so those events would carry no
//! payload; `open_path_at`'s already-open goto jump; split-group focus
//! switches; diff-completion activation; session/tab restore; new untitled
//! documents. Plugins that need those transitions can watch `on_edit`.
//!
//! # Pump start
//!
//! The pump starts from `LubanApp::new` (the earliest reliable site — a
//! window handle is available there) and lazily from any windowed
//! `reload_plugins`. It only spawns once at least one plugin runtime is
//! active, and `plugin_pump_started` is only set when the spawn actually
//! happens, so a reload that installs the first runtime (re)tries. The
//! windowless `reload_plugins_now` (background store completions) cannot
//! spawn the pump; the next windowed reload or restart does.

use std::path::Path;
use std::time::{Duration, Instant};

use super::*;

/// Composite generation of the active tab's edit state, mirroring
/// `plugin_panel_generation`'s tuple: the tab identity plus its edit
/// generation and document revision. Any field changing means "the document
/// the user is working on changed".
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(super) struct EditGeneration {
    pub(super) tab_id: u64,
    pub(super) edit_generation: u64,
    pub(super) revision: u64,
}

/// Debounces edit generations: emits once after [`EDIT_STABLE_AFTER`] of
/// stability. The armed stability clock only clears when a generation changes
/// or the debounce fires, so a poll that finds the change not yet stable does
/// not consume it.
pub(super) struct EditDebouncer {
    last: Option<EditGeneration>,
    changed_at: Option<Instant>,
}

impl EditDebouncer {
    pub(super) fn new() -> Self {
        Self {
            last: None,
            changed_at: None,
        }
    }

    /// Returns true exactly once after a generation change has been stable for
    /// [`EDIT_STABLE_AFTER`]. The first sighting of a generation only seeds
    /// the debouncer — only *changes* emit. `None` (no active tab) clears all
    /// state and never emits.
    pub(super) fn poll(&mut self, generation: Option<EditGeneration>, now: Instant) -> bool {
        let Some(generation) = generation else {
            self.last = None;
            self.changed_at = None;
            return false;
        };
        match self.last {
            None => {
                self.last = Some(generation);
                false
            }
            Some(previous) if previous != generation => {
                self.last = Some(generation);
                self.changed_at = Some(now);
                false
            }
            Some(_) => {
                let due = self
                    .changed_at
                    .is_some_and(|since| now.duration_since(since) >= EDIT_STABLE_AFTER);
                if due {
                    self.changed_at = None;
                }
                due
            }
        }
    }
}

/// One queued app event plus the index of the first listener still owed a
/// dispatch. A tick whose budget ran out mid-batch re-queues the event with
/// its cursor, so the remainder is delivered exactly once on a later tick
/// instead of re-delivering to listeners already served.
pub(super) struct QueuedPluginEvent {
    event: PluginEvent,
    listener_cursor: usize,
}

/// A document event queued by the app for the plugin pump.
#[derive(Clone, Debug)]
pub(super) enum PluginEvent {
    DocumentOpened(luban_plugin::runtime::DocumentSnapshot),
    DocumentSaved(luban_plugin::runtime::DocumentSnapshot),
    ActiveDocumentChanged(Option<luban_plugin::runtime::DocumentSnapshot>),
}

pub(super) const PUMP_TICK: Duration = Duration::from_millis(50);
pub(super) const MAX_DISPATCHES_PER_TICK: usize = 8;
pub(super) const EDIT_STABLE_AFTER: Duration = Duration::from_millis(150);
/// Upper bound on queued events. Every queued `DocumentOpened`/`DocumentSaved`
/// carries a full document snapshot, so an unbounded queue with no pump
/// running (no plugin loaded yet) would leak memory; dropping the oldest
/// event keeps the latest document state, which is what handlers care about.
const MAX_QUEUED_PLUGIN_EVENTS: usize = 64;

impl LubanApp {
    // ------------------------------------------------------------------
    // Event queue
    // ------------------------------------------------------------------

    /// Queue one app event for the pump. Suppressed while a plugin-originated
    /// call's actions are being applied (spec §6): a plugin's write-backs must
    /// not cascade back into the plugin that caused them.
    pub(super) fn plugin_emit(&mut self, event: PluginEvent) {
        if self.plugin_in_event_context() {
            return;
        }
        if self.plugin_event_queue.len() >= MAX_QUEUED_PLUGIN_EVENTS {
            self.plugin_event_queue.pop_front();
            log::debug!("plugin event queue full; dropped the oldest event");
        }
        self.plugin_event_queue.push_back(QueuedPluginEvent {
            event,
            listener_cursor: 0,
        });
    }

    /// Queue `ActiveDocumentChanged` for the now-active tab (or `None` when
    /// the active tab cannot be exposed to plugins). Call sites are
    /// user-driven activations; the event-context suppression inside
    /// [`Self::plugin_emit`] covers plugin-caused ones.
    pub(super) fn emit_active_document_changed(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.plugin_document_snapshot(cx);
        self.plugin_emit(PluginEvent::ActiveDocumentChanged(snapshot));
    }

    /// Composite edit generation of the active tab (mirrors
    /// `plugin_panel_generation`), or `None` without an active tab.
    fn plugin_edit_generation(&self) -> Option<EditGeneration> {
        let tab = self.active_tab()?;
        Some(EditGeneration {
            tab_id: tab.id,
            edit_generation: tab.edit_generation,
            revision: tab.doc.revision(),
        })
    }

    // ------------------------------------------------------------------
    // Pump
    // ------------------------------------------------------------------

    /// Spawn the 50 ms pump task unless it already ran or there is no active
    /// runtime to dispatch to (lazy start; see the module docs).
    pub(super) fn start_plugin_pump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.plugin_pump_started {
            return;
        }
        if self.plugin_host.active_plugin_ids().is_empty() {
            return;
        }
        self.plugin_pump_started = true;
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(PUMP_TICK).await;
                if this
                    .update_in(cx, |app, window, cx| app.plugin_pump_tick(window, cx))
                    .is_err()
                {
                    // The app is gone; stop the pump.
                    break;
                }
            }
        })
        .detach();
    }

    /// One pump tick: drain queued events within the dispatch budget, then the
    /// (coalesced) `on_edit` debounce, then due timers, then quarantine
    /// enforcement — the failure counters were recorded inside the dispatches
    /// above, so a failure during this tick counts.
    fn plugin_pump_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = Instant::now();
        let mut budget = MAX_DISPATCHES_PER_TICK;
        self.dispatch_queued_plugin_events(&mut budget, window, cx);
        let generation = self.plugin_edit_generation();
        if self.plugin_edit_debouncer.poll(generation, now) {
            self.dispatch_plugin_edit(&mut budget, window, cx);
        }
        self.fire_due_plugin_timers(&mut budget, now, window, cx);
        self.enforce_plugin_quarantine(window, cx);
    }

    /// Dispatch queued events until the budget runs out. An event whose
    /// listener batch does not fit stays at the front of the queue with a
    /// cursor so the remainder is delivered on a later tick without
    /// re-delivering to listeners already served this tick.
    fn dispatch_queued_plugin_events(
        &mut self,
        budget: &mut usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        while *budget > 0 {
            let Some(mut queued) = self.plugin_event_queue.pop_front() else {
                break;
            };
            let (name, snapshot): (
                &'static str,
                Option<luban_plugin::runtime::DocumentSnapshot>,
            ) = match &queued.event {
                PluginEvent::DocumentOpened(snapshot) => {
                    ("on_document_open", Some(snapshot.clone()))
                }
                PluginEvent::DocumentSaved(snapshot) => {
                    ("on_document_save", Some(snapshot.clone()))
                }
                PluginEvent::ActiveDocumentChanged(snapshot) => {
                    ("on_active_document_changed", snapshot.clone())
                }
            };
            let listeners = self.plugin_host.listeners_of(name);
            let total = listeners.len();
            if queued.listener_cursor >= total {
                // No listener (or none left): drop without spending budget.
                continue;
            }
            let mut delivered = 0usize;
            for plugin_id in &listeners[queued.listener_cursor.min(total)..] {
                if *budget == 0 {
                    break;
                }
                let input = self.plugin_event_input(plugin_id, snapshot.as_ref());
                let Some(call) = self.plugin_host.dispatch_event(plugin_id, name, input) else {
                    continue;
                };
                self.raise_event_depth_during(|app| {
                    app.apply_plugin_call(plugin_id, call, window, cx)
                });
                *budget -= 1;
                delivered += 1;
            }
            queued.listener_cursor += delivered;
            if queued.listener_cursor < total {
                self.plugin_event_queue.push_front(queued);
            }
        }
    }

    /// Dispatch the coalesced `on_edit` call to every listener. This is one
    /// call per stability window, not per edit, so it dispatches even when the
    /// event budget is spent — a busy event queue must not starve it — while
    /// still counting against the budget (saturating) so later stages yield.
    fn dispatch_plugin_edit(
        &mut self,
        budget: &mut usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        const NAME: &str = "on_edit";
        for plugin_id in self.plugin_host.listeners_of(NAME) {
            // on_edit is about the document being edited right now: build the
            // snapshot fresh at dispatch time. The Edit context kind makes the
            // runtime enforce document read-only (spec §6).
            let input = luban_plugin::runtime::CallInput {
                plugin: luban_plugin::runtime::PluginIdentity {
                    id: plugin_id.clone(),
                    version: self
                        .plugin_host
                        .manifest(&plugin_id)
                        .map(|manifest| manifest.version.clone())
                        .unwrap_or_default(),
                    data_dir: self.plugin_host.root.join(&plugin_id).join("data"),
                },
                document: self.plugin_document_snapshot(cx),
                settings: self.plugin_host.settings_for(&plugin_id),
                allowed_roots: self.plugin_allowed_roots(),
                command_depth: 0,
                context_kind: luban_plugin::runtime::CallContextKind::Edit,
            };
            let Some(call) = self.plugin_host.dispatch_event(&plugin_id, NAME, input) else {
                continue;
            };
            self.raise_event_depth_during(|app| {
                app.apply_plugin_call(&plugin_id, call, window, cx)
            });
            *budget = budget.saturating_sub(1);
        }
    }

    /// Fire every timer that is due, within the remaining budget. Timers left
    /// over wait for the next tick — `due_timer_ids` recomputes, so nothing is
    /// lost.
    fn fire_due_plugin_timers(
        &mut self,
        budget: &mut usize,
        now: Instant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for plugin_id in self.plugin_host.active_plugin_ids() {
            let ids = self.plugin_host.due_timer_ids(&plugin_id, now);
            if ids.is_empty() {
                continue;
            }
            for id in ids {
                if *budget == 0 {
                    return;
                }
                let input = self.plugin_timer_input(&plugin_id, cx);
                match self.plugin_host.fire_timer(&plugin_id, &id, input) {
                    // None: no runtime (vanished mid-batch). Some(Err):
                    // unknown timer id, vanished mid-batch — a benign skip,
                    // no failure counted.
                    Some(Err(error)) => {
                        log::debug!("plugin '{plugin_id}' timer '{id}' vanished: {error}")
                    }
                    Some(Ok(call)) => {
                        self.raise_event_depth_during(|app| {
                            app.apply_plugin_call(&plugin_id, call, window, cx)
                        });
                        *budget -= 1;
                    }
                    None => {}
                }
            }
        }
    }

    /// Auto-disable every active plugin whose consecutive-failure count
    /// reached the quarantine threshold. Mirrors the Plugins-manager disable
    /// flow: update `disabled_plugins`, keep the global snapshot in sync,
    /// persist and reload. Runs after this tick's dispatches so a failure that
    /// just happened counts.
    fn enforce_plugin_quarantine(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let due: Vec<String> = self
            .plugin_host
            .active_plugin_ids()
            .into_iter()
            .filter(|id| self.plugin_host.quarantine_due(id))
            .collect();
        if due.is_empty() {
            return;
        }
        for id in &due {
            // Re-enabling later starts from a clean failure count.
            self.plugin_host.reset_failures(id);
            if !self.settings.disabled_plugins.contains(id) {
                self.settings.disabled_plugins.push(id.clone());
            }
            log::error!("plugin '{id}' disabled after repeated failures");
            self.set_status(
                format!("plugin '{id}' disabled after repeated failures"),
                cx,
            );
        }
        self.global_settings.disabled_plugins = self.settings.disabled_plugins.clone();
        self.commit_settings();
        self.reload_plugins(window, cx);
    }

    // ------------------------------------------------------------------
    // Dispatch inputs
    // ------------------------------------------------------------------

    /// Build the dispatch input for one plugin for a queued event. The payload
    /// snapshot travels with the event (state at emit time — no re-read
    /// mid-tick); the document directory for allowed roots comes from the
    /// event's own path when it has one, falling back to the active document
    /// like a user command would.
    fn plugin_event_input(
        &self,
        plugin_id: &str,
        snapshot: Option<&luban_plugin::runtime::DocumentSnapshot>,
    ) -> luban_plugin::runtime::CallInput {
        let document_dir = match snapshot.and_then(|snapshot| snapshot.path.as_deref()) {
            Some(path) => path.parent(),
            None => self
                .active_doc()
                .and_then(|doc| doc.path.as_deref())
                .and_then(Path::parent),
        };
        luban_plugin::runtime::CallInput {
            plugin: luban_plugin::runtime::PluginIdentity {
                id: plugin_id.to_string(),
                version: self
                    .plugin_host
                    .manifest(plugin_id)
                    .map(|manifest| manifest.version.clone())
                    .unwrap_or_default(),
                data_dir: self.plugin_host.root.join(plugin_id).join("data"),
            },
            document: snapshot.cloned(),
            settings: self.plugin_host.settings_for(plugin_id),
            allowed_roots: Self::plugin_allowed_roots_for(
                self.workspace_root.as_ref(),
                &self.workspace_roots,
                document_dir,
            ),
            command_depth: 0,
            context_kind: luban_plugin::runtime::CallContextKind::Event,
        }
    }

    /// Build the dispatch input for one plugin's timer: fresh active-document
    /// snapshot, Event context (timers are not read-only, unlike `on_edit`).
    fn plugin_timer_input(
        &self,
        plugin_id: &str,
        cx: &mut Context<Self>,
    ) -> luban_plugin::runtime::CallInput {
        luban_plugin::runtime::CallInput {
            plugin: luban_plugin::runtime::PluginIdentity {
                id: plugin_id.to_string(),
                version: self
                    .plugin_host
                    .manifest(plugin_id)
                    .map(|manifest| manifest.version.clone())
                    .unwrap_or_default(),
                data_dir: self.plugin_host.root.join(plugin_id).join("data"),
            },
            document: self.plugin_document_snapshot(cx),
            settings: self.plugin_host.settings_for(plugin_id),
            allowed_roots: self.plugin_allowed_roots(),
            command_depth: 0,
            context_kind: luban_plugin::runtime::CallContextKind::Event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EditDebouncer, EditGeneration, EDIT_STABLE_AFTER};
    use std::time::{Duration, Instant};

    fn generation(tab_id: u64, edit_generation: u64, revision: u64) -> EditGeneration {
        EditGeneration {
            tab_id,
            edit_generation,
            revision,
        }
    }

    /// The plan's scenario: gen1 seed, gen2 change, +100 ms no, +160 ms yes
    /// (measured from the change), +200 ms no.
    #[test]
    fn edit_debouncer_emits_once_after_stability() {
        let t0 = Instant::now();
        let mut debouncer = EditDebouncer::new();
        // The first sighting of a generation only seeds the debouncer.
        assert!(!debouncer.poll(Some(generation(1, 1, 1)), t0));
        // A changed generation arms the stability clock without emitting.
        assert!(!debouncer.poll(Some(generation(1, 2, 2)), t0));
        // Not stable yet.
        assert!(!debouncer.poll(
            Some(generation(1, 2, 2)),
            t0 + EDIT_STABLE_AFTER - Duration::from_millis(50)
        ));
        // Stable after EDIT_STABLE_AFTER → emit exactly once.
        assert!(debouncer.poll(
            Some(generation(1, 2, 2)),
            t0 + EDIT_STABLE_AFTER + Duration::from_millis(10)
        ));
        // Still stable → no further emits.
        assert!(!debouncer.poll(
            Some(generation(1, 2, 2)),
            t0 + EDIT_STABLE_AFTER + Duration::from_millis(60)
        ));
    }

    #[test]
    fn edit_debouncer_resets_without_a_tab() {
        let t0 = Instant::now();
        let mut debouncer = EditDebouncer::new();
        assert!(!debouncer.poll(Some(generation(7, 3, 9)), t0));
        assert!(!debouncer.poll(Some(generation(7, 4, 10)), t0));
        // Losing the active tab clears the debounce state.
        assert!(!debouncer.poll(None, t0));
        // The same generation re-seeds after the reset and never emits on its
        // own — only observed changes emit.
        assert!(!debouncer.poll(Some(generation(7, 3, 9)), t0));
        assert!(!debouncer.poll(Some(generation(7, 3, 9)), t0 + Duration::from_millis(500)));
    }

    /// A poll that finds the change not yet stable must not consume the armed
    /// clock — a later poll inside the same stability window still fires.
    #[test]
    fn edit_debouncer_keeps_the_clock_through_early_polls() {
        let t0 = Instant::now();
        let mut debouncer = EditDebouncer::new();
        assert!(!debouncer.poll(Some(generation(1, 1, 1)), t0));
        assert!(!debouncer.poll(Some(generation(1, 2, 2)), t0));
        // Every poll strictly before stability keeps the clock armed.
        for step_ms in (10..150).step_by(10) {
            let step = Duration::from_millis(step_ms);
            assert!(
                !debouncer.poll(Some(generation(1, 2, 2)), t0 + step),
                "emitted early at {step:?}"
            );
        }
        assert!(
            debouncer.poll(Some(generation(1, 2, 2)), t0 + EDIT_STABLE_AFTER),
            "did not emit after stability"
        );
    }

    /// A tab switch is a generation change (the tab id is part of the
    /// composite), so the debounce re-arms for the newly active tab.
    #[test]
    fn edit_debouncer_treats_a_tab_switch_as_a_change() {
        let t0 = Instant::now();
        let mut debouncer = EditDebouncer::new();
        assert!(!debouncer.poll(Some(generation(1, 5, 5)), t0));
        assert!(!debouncer.poll(Some(generation(2, 0, 0)), t0));
        assert!(debouncer.poll(
            Some(generation(2, 0, 0)),
            t0 + EDIT_STABLE_AFTER + Duration::from_millis(1)
        ));
    }
}

/// The tick loop, dispatch budget splitting, timers and quarantine enforcement
/// are UI-bound (they run inside `plugin_pump_tick` on a window) and are
/// exercised by the Task 16 smoke tests and review; the tests here cover the
/// queue mechanics and suppression that are reachable without dispatching.
#[cfg(test)]
mod pump_tests {
    use super::super::{test_config_dir, test_lock, LaunchOptions, LubanApp, WindowInit};
    use super::{PluginEvent, MAX_QUEUED_PLUGIN_EVENTS};
    use luban_plugin::runtime::DocumentSnapshot;
    use std::path::{Path, PathBuf};

    fn options() -> LaunchOptions {
        LaunchOptions {
            files: Vec::new(),
            smoke_test: false,
            safe_mode: true,
            session_writer: false,
            wait: false,
            goto: None,
            diff: None,
            stdin: None,
            command: None,
            exit_on_command_failure: false,
            command_result: None,
            ipc_queue: None,
            manual_window_chrome: false,
            recoveries: Vec::new(),
            init: WindowInit::Blank,
            auto_recovery: false,
            window_id: 1,
        }
    }

    fn snapshot(name: &str) -> DocumentSnapshot {
        DocumentSnapshot {
            path: Some(PathBuf::from(name)),
            text: String::new(),
            language: String::new(),
            line: 1,
            column: 1,
            selection_start: 0,
            selection_end: 0,
            modified: false,
        }
    }

    /// `plugin_emit` queues outside a plugin event context and is suppressed
    /// while one is active (spec §6 cascade suppression).
    #[gpui::test]
    fn plugin_emit_suppressed_in_event_context(cx: &mut gpui::TestAppContext) {
        let _guard = test_lock();
        test_config_dir();
        cx.update(gpui_kit::init);
        let (app, cx) = cx.add_window_view(|window, cx| LubanApp::new(window, cx, options()));
        cx.update(|_window, cx| {
            app.update(cx, |app, _cx| {
                // safe_mode loaded no runtimes, so the pump has not started;
                // it starts lazily on the first windowed reload with one.
                assert!(!app.plugin_pump_started);
                // Outside an event context events queue up.
                app.plugin_emit(PluginEvent::DocumentOpened(snapshot("a")));
                assert_eq!(app.plugin_event_queue.len(), 1);
                // Inside one, emission is suppressed entirely.
                app.plugin_event_depth = 1;
                app.plugin_emit(PluginEvent::DocumentSaved(snapshot("a")));
                assert_eq!(app.plugin_event_queue.len(), 1);
                app.plugin_event_depth = 0;
                app.plugin_emit(PluginEvent::ActiveDocumentChanged(None));
                assert_eq!(app.plugin_event_queue.len(), 2);
            });
        });
    }

    /// The queue caps itself by dropping the oldest event, so a session
    /// without a running pump cannot grow the queue without bound (every
    /// queued open/save carries a full document snapshot).
    #[gpui::test]
    fn plugin_event_queue_caps_and_drops_the_oldest(cx: &mut gpui::TestAppContext) {
        let _guard = test_lock();
        test_config_dir();
        cx.update(gpui_kit::init);
        let (app, cx) = cx.add_window_view(|window, cx| LubanApp::new(window, cx, options()));
        cx.update(|_window, cx| {
            app.update(cx, |app, _cx| {
                for index in 0..MAX_QUEUED_PLUGIN_EVENTS {
                    app.plugin_emit(PluginEvent::DocumentOpened(snapshot(&format!("f{index}"))));
                }
                assert_eq!(app.plugin_event_queue.len(), MAX_QUEUED_PLUGIN_EVENTS);
                app.plugin_emit(PluginEvent::DocumentSaved(snapshot("new")));
                assert_eq!(app.plugin_event_queue.len(), MAX_QUEUED_PLUGIN_EVENTS);
                // The oldest event was dropped; the newest is at the back.
                let back = app.plugin_event_queue.back().unwrap();
                assert!(matches!(back.event, PluginEvent::DocumentSaved(_)));
                let front = app.plugin_event_queue.front().unwrap();
                match &front.event {
                    PluginEvent::DocumentOpened(snapshot) => {
                        assert_eq!(snapshot.path.as_deref(), Some(Path::new("f1")))
                    }
                    other => panic!("unexpected front event: {other:?}"),
                }
            });
        });
    }
}

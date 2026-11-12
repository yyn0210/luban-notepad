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
//! switches; drag-tab-into-group activation (`move_tab_by_id`); navigation
//! history activation (`apply_navigation_entry`); diff-completion activation;
//! session/tab restore; new untitled documents (except via last-tab close and
//! Close All). Plugins that need those transitions can watch `on_edit`.
//!
//! Events are only queued while at least one active plugin listens for the
//! event's name, so the common no-plugins session pays nothing for emissions.
//!
//! # Pump start
//!
//! The pump starts from `LubanApp::new` (the earliest reliable site — a
//! window handle is available there) and lazily from any windowed
//! `reload_plugins`. It only spawns once at least one plugin runtime is
//! active, and `plugin_pump_started` is only set when the spawn actually
//! happens, so a reload that installs the first runtime (re)tries. The
//! windowless `reload_plugins_now` (background store completions) cannot
//! spawn the pump; the next windowed reload or restart does. A pump whose
//! last runtime disappears parks itself: the tick task breaks and resets the
//! flag, so an app without plugins costs zero ticks and the next windowed
//! reload restarts the pump.

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

impl PluginEvent {
    /// Manifest event name this payload dispatches as.
    fn event_name(&self) -> &'static str {
        match self {
            PluginEvent::DocumentOpened(_) => "on_document_open",
            PluginEvent::DocumentSaved(_) => "on_document_save",
            PluginEvent::ActiveDocumentChanged(_) => "on_active_document_changed",
        }
    }
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
    /// not cascade back into the plugin that caused them. The suppression
    /// check runs first; second, the event is dropped when no active plugin
    /// listens for its name — emission would otherwise clone a full document
    /// snapshot per event for nobody.
    pub(super) fn plugin_emit(&mut self, event: PluginEvent) {
        if self.plugin_in_event_context() {
            return;
        }
        if self.plugin_host.listeners_of(event.event_name()).is_empty() {
            return;
        }
        self.plugin_enqueue(event);
    }

    /// Cap-and-push one event into the queue without any gating (the
    /// gate-free mechanics behind [`Self::plugin_emit`]).
    fn plugin_enqueue(&mut self, event: PluginEvent) {
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
                // Err: the app is gone. Ok(false): the pump parked itself (no
                // runtimes and nothing queued) — the next windowed reload with
                // a runtime restarts it. Either way the task is done.
                if !this
                    .update_in(cx, |app, window, cx| app.plugin_pump_tick(window, cx))
                    .is_ok_and(|keep| keep)
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// One pump tick: drain queued events within the dispatch budget, then the
    /// (coalesced) `on_edit` debounce, then due timers, then quarantine
    /// enforcement — the failure counters were recorded inside the dispatches
    /// above, so a failure during this tick counts. Returns false to make the
    /// pump task park (no active runtime and nothing queued), true to keep
    /// ticking.
    fn plugin_pump_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let active_ids = self.plugin_host.active_plugin_ids();
        if active_ids.is_empty() && self.plugin_event_queue.is_empty() {
            // Park: without a runtime nothing can be dispatched, queued or
            // timed (timers live in runtimes). Reset the flag so the next
            // windowed reload with a runtime spawns the pump again.
            self.plugin_pump_started = false;
            return false;
        }
        let mut budget = MAX_DISPATCHES_PER_TICK;
        self.dispatch_queued_plugin_events(&mut budget, window, cx);
        // Re-capture the clock: a heavy dispatch stage must not shrink the
        // effective stability window of this tick's debounce decision.
        let now = Instant::now();
        let generation = self.plugin_edit_generation();
        if self.plugin_edit_debouncer.poll(generation, now) {
            self.dispatch_plugin_edit(&mut budget, window, cx);
        }
        self.fire_due_plugin_timers(&active_ids, &mut budget, now, window, cx);
        self.enforce_plugin_quarantine(&active_ids, window, cx);
        true
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
            let name = queued.event.event_name();
            let snapshot: Option<luban_plugin::runtime::DocumentSnapshot> = match &queued.event {
                PluginEvent::DocumentOpened(snapshot) => Some(snapshot.clone()),
                PluginEvent::DocumentSaved(snapshot) => Some(snapshot.clone()),
                PluginEvent::ActiveDocumentChanged(snapshot) => snapshot.clone(),
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
                    app.apply_plugin_call(
                        plugin_id,
                        call,
                        luban_plugin::runtime::CallContextKind::Event,
                        window,
                        cx,
                    )
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
            // runtime enforce document read-only (spec §6), and nested
            // `command.run` actions inherit it so a handler cannot mutate the
            // document through a chained command either.
            let input = self.plugin_fresh_input(
                &plugin_id,
                cx,
                luban_plugin::runtime::CallContextKind::Edit,
            );
            let Some(call) = self.plugin_host.dispatch_event(&plugin_id, NAME, input) else {
                continue;
            };
            self.raise_event_depth_during(|app| {
                app.apply_plugin_call(
                    &plugin_id,
                    call,
                    luban_plugin::runtime::CallContextKind::Edit,
                    window,
                    cx,
                )
            });
            *budget = budget.saturating_sub(1);
        }
    }

    /// Fire every timer that is due, within the remaining budget. Timers left
    /// over wait for the next tick — `due_timer_ids` recomputes, so nothing is
    /// lost. `active_ids` is the tick's snapshot; a stale id whose runtime
    /// vanished mid-tick degrades to an empty timer list.
    fn fire_due_plugin_timers(
        &mut self,
        active_ids: &[String],
        budget: &mut usize,
        now: Instant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for plugin_id in active_ids {
            let ids = self.plugin_host.due_timer_ids(plugin_id, now);
            if ids.is_empty() {
                continue;
            }
            for id in ids {
                if *budget == 0 {
                    return;
                }
                let input = self.plugin_fresh_input(
                    plugin_id,
                    cx,
                    luban_plugin::runtime::CallContextKind::Event,
                );
                match self.plugin_host.fire_timer(plugin_id, &id, input) {
                    // None: no runtime (vanished mid-batch). Some(Err):
                    // unknown timer id, vanished mid-batch — a benign skip,
                    // no failure counted.
                    Some(Err(error)) => {
                        log::debug!("plugin '{plugin_id}' timer '{id}' vanished: {error}")
                    }
                    Some(Ok(call)) => {
                        self.raise_event_depth_during(|app| {
                            app.apply_plugin_call(
                                plugin_id,
                                call,
                                luban_plugin::runtime::CallContextKind::Event,
                                window,
                                cx,
                            )
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
    /// just happened counts. `active_ids` is the tick's snapshot (captured
    /// before the dispatch stages).
    fn enforce_plugin_quarantine(
        &mut self,
        active_ids: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let due: Vec<String> = active_ids
            .iter()
            .filter(|id| self.plugin_host.quarantine_due(id))
            .cloned()
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
        self.commit_settings(cx);
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

    /// Build the dispatch input for one plugin from the document state right
    /// now (fresh active-document snapshot, active-document allowed roots).
    /// Used by the `on_edit` stage (`context_kind` Edit — read-only) and the
    /// timer stage (`context_kind` Event).
    fn plugin_fresh_input(
        &self,
        plugin_id: &str,
        cx: &Context<Self>,
        context_kind: luban_plugin::runtime::CallContextKind,
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
            context_kind,
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

    /// `plugin_emit` drops events when no active plugin listens for the
    /// event's name (the gate keeps full-text snapshot clones off the queue
    /// when nobody is listening), and the suppression check runs before the
    /// gate. With `safe_mode` there are no runtimes, so no listener can be
    /// injected headlessly — queue-side behavior under real listeners is
    /// exercised by the Task 16 smoke tests.
    #[gpui::test]
    fn plugin_emit_gates_on_listeners(cx: &mut gpui::TestAppContext) {
        let _guard = test_lock();
        test_config_dir();
        cx.update(gpui_kit::init);
        let (app, cx) = cx.add_window_view(|window, cx| LubanApp::new(window, cx, options()));
        cx.update(|_window, cx| {
            app.update(cx, |app, _cx| {
                // safe_mode loaded no runtimes, so the pump has not started;
                // it starts lazily on the first windowed reload with one.
                assert!(!app.plugin_pump_started);
                // No listener for any of the three event names: dropped.
                app.plugin_emit(PluginEvent::DocumentOpened(snapshot("a")));
                app.plugin_emit(PluginEvent::DocumentSaved(snapshot("a")));
                app.plugin_emit(PluginEvent::ActiveDocumentChanged(None));
                assert!(app.plugin_event_queue.is_empty());
                // Inside an event context emission is suppressed outright
                // (checked before the listeners gate — same outcome here).
                app.plugin_event_depth = 1;
                app.plugin_emit(PluginEvent::DocumentOpened(snapshot("a")));
                assert!(app.plugin_event_queue.is_empty());
                app.plugin_event_depth = 0;
                assert!(!app.plugin_in_event_context());
            });
        });
    }

    /// The queue caps itself by dropping the oldest event, so a session
    /// without a running pump cannot grow the queue without bound (every
    /// queued open/save carries a full document snapshot). Driven through the
    /// gate-free enqueue to observe the mechanics directly.
    #[gpui::test]
    fn plugin_event_queue_caps_and_drops_the_oldest(cx: &mut gpui::TestAppContext) {
        let _guard = test_lock();
        test_config_dir();
        cx.update(gpui_kit::init);
        let (app, cx) = cx.add_window_view(|window, cx| LubanApp::new(window, cx, options()));
        cx.update(|_window, cx| {
            app.update(cx, |app, _cx| {
                for index in 0..MAX_QUEUED_PLUGIN_EVENTS {
                    app.plugin_enqueue(PluginEvent::DocumentOpened(snapshot(&format!("f{index}"))));
                }
                assert_eq!(app.plugin_event_queue.len(), MAX_QUEUED_PLUGIN_EVENTS);
                app.plugin_enqueue(PluginEvent::DocumentSaved(snapshot("new")));
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

    /// A plugin-requested `command.run` targeting a built-in still runs from an
    /// Edit ambient: the inherited kind constrains plugin document APIs (the
    /// runtime rejects them — see luban-plugin's
    /// `document_mutations_are_read_only_in_edit_context`), not the built-in
    /// executor. The built-in/macro residual is documented on
    /// `apply_run_command_action`. Exercises the refactored
    /// `apply_plugin_call` signature end-to-end within headless reach.
    #[gpui::test]
    fn nested_run_command_from_edit_context_still_runs_builtins(cx: &mut gpui::TestAppContext) {
        let _guard = test_lock();
        test_config_dir();
        cx.update(gpui_kit::init);
        let (app, cx) = cx.add_window_view(|window, cx| LubanApp::new(window, cx, options()));
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let before = app.tabs.len();
                let call = luban_plugin::runtime::RuntimeCall {
                    output: luban_plugin::runtime::RuntimeOutput {
                        actions: vec![luban_plugin::runtime::HostAction::RunCommand {
                            id: "file.new".to_string(),
                            args: None,
                            depth: 0,
                        }],
                    },
                    value: Some(()),
                    error: None,
                };
                assert!(app.apply_plugin_call(
                    "test-plugin",
                    call,
                    luban_plugin::runtime::CallContextKind::Edit,
                    window,
                    cx,
                ));
                assert_eq!(app.tabs.len(), before + 1);
            });
        });
    }
}

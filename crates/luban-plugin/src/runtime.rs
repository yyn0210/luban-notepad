//! Sandboxed Lua plugin runtime.
//!
//! Each plugin owns one Lua state. The runtime exposes a small `luban` table and
//! never gives plugins access to `io`, `os`, `package`, `debug` or dynamic code
//! loading; `require` is replaced by a sandboxed in-plugin loader that only
//! reads modules from the plugin directory (spec §8). Document and status
//! changes are collected as actions and applied by the application after the
//! callback returns, so a plugin error cannot leave a half-applied host action.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use mlua::{HookTriggers, IntoLuaMulti, Lua, LuaOptions, StdLib, Table, Value, VmState};
use serde_json::{Map, Value as JsonValue};

use crate::manifest::{PluginManifest, SettingContribution};
use crate::permissions::PermissionSet;

pub const DEFAULT_INSTRUCTION_LIMIT: u64 = 50_000_000;
const HOOK_STEP: u64 = 10_000;
const MEMORY_LIMIT: usize = 64 * 1024 * 1024;
const MAX_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PANEL_LINES: usize = 5_000;
const MAX_PANEL_LINE_BYTES: usize = 4 * 1024;
const MAX_PANEL_SPANS: usize = 512;
const MAX_PANEL_ACTIONS: usize = 8;
const MAX_PANEL_ACTION_LABEL_BYTES: usize = 64;
const MAX_PANEL_ACTION_COMMAND_BYTES: usize = 128;
const MAX_HIGHLIGHT_BYTES: usize = 64 * 1024;
const MAX_ROLE_BYTES: usize = 32;
/// Maximum length for plugin-supplied resource ids (panels, commands).
const MAX_ID_BYTES: usize = 128;
// Bounds for the v2 call context and host action surface; the still-unused
// ones are consumed by upcoming v2 API tasks (panel command args).
const MAX_EVENT_HANDLERS: usize = 32;
/// Maximum number of simultaneously active timers per plugin (spec §9).
const MAX_TIMERS: usize = 16;
/// Smallest timer interval; shorter requests are clamped up (spec §9).
const MIN_TIMER_MS: u64 = 50;
/// Consecutive failed dispatches (commands, panels, events, timers) before
/// [`PluginRuntime::should_quarantine`] trips (spec §6).
const QUARANTINE_THRESHOLD: u32 = 10;
/// Maximum `command.run` cascade depth (pub: the app executor in Task 11
/// rejects `HostAction::RunCommand` chains deeper than this).
pub const MAX_COMMAND_DEPTH: u8 = 4;
const MAX_NOTIFY_PER_CALL: usize = 8;
const MAX_NOTIFY_BYTES: usize = 1024;
#[expect(dead_code)]
const MAX_PANEL_ARGS_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug)]
pub struct PluginIdentity {
    pub id: String,
    pub version: String,
    pub data_dir: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentSnapshot {
    pub path: Option<PathBuf>,
    pub text: String,
    pub language: String,
    pub line: usize,
    pub column: usize,
    pub selection_start: usize,
    pub selection_end: usize,
    pub modified: bool,
}

/// Dispatch origin. Event/Edit contexts are cascade-suppressed; Edit is
/// additionally read-only. Timer dispatch shares `Event`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CallContextKind {
    #[default]
    User,
    Event,
    Edit,
}

#[derive(Clone, Debug)]
pub struct CallInput {
    pub plugin: PluginIdentity,
    pub document: Option<DocumentSnapshot>,
    pub settings: Map<String, JsonValue>,
    pub allowed_roots: Vec<PathBuf>,
    /// Depth of the command.run chain that initiated this call (user/CLI/panel
    /// click = 0).
    pub command_depth: u8,
    pub context_kind: CallContextKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HostAction {
    SetDocumentText {
        text: String,
        selection: Option<(usize, usize)>,
    },
    Notify(String),
    SetStatus {
        id: String,
        text: String,
    },
    OpenPanel {
        id: String,
    },
    /// Re-render the plugin's already-open panel (ignored when closed).
    RefreshPanel {
        id: String,
    },
    /// Set the active document cursor (1-based, host clamps).
    SetCursor {
        line: usize,
        col: usize,
    },
    /// Write back a plugin setting (host validates against the declared schema
    /// and persists).
    SetSetting {
        key: String,
        value: JsonValue,
    },
    /// Run a command through the host command system. `depth` is the depth of
    /// the command being launched (caller's `command_depth` + 1); the executor
    /// rejects `depth > MAX_COMMAND_DEPTH`.
    RunCommand {
        id: String,
        args: Option<JsonValue>,
        depth: u8,
    },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RuntimeOutput {
    pub actions: Vec<HostAction>,
}

#[derive(Clone, Debug)]
pub struct RuntimeCall<T> {
    pub output: RuntimeOutput,
    pub value: Option<T>,
    pub error: Option<String>,
}

impl<T> RuntimeCall<T> {
    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelContent {
    pub title: Option<String>,
    pub lines: Vec<PanelLine>,
    pub truncated: bool,
    /// Optional flat action bar rendered in the panel header.
    pub actions: Vec<PanelAction>,
}

/// Command reference carried by panel lines / header actions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelCommand {
    pub id: String,
    /// JSON-compatible args, passed verbatim as the command entry's second
    /// parameter.
    pub args: Option<JsonValue>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelAction {
    pub label: String,
    pub command: PanelCommand,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelLine {
    pub text: String,
    pub command: Option<PanelCommand>,
    pub style: PanelLineStyle,
    /// Nesting level for list/quote lines, clamped to `0..=8`.
    pub indent: u8,
    /// Inline runs. Empty means the line is rendered as plain `text`.
    pub spans: Vec<PanelSpan>,
}

impl PanelLine {
    fn plain(text: String) -> Self {
        Self {
            text,
            command: None,
            style: PanelLineStyle::Text,
            indent: 0,
            spans: Vec::new(),
        }
    }
}

/// Block-level presentation for one panel line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PanelLineStyle {
    #[default]
    Text,
    Muted,
    H1,
    H2,
    H3,
    H4,
    H5,
    H6,
    Code,
    Quote,
    List,
    Table,
    Rule,
}

impl PanelLineStyle {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "text" => Some(Self::Text),
            "muted" => Some(Self::Muted),
            "h1" => Some(Self::H1),
            "h2" => Some(Self::H2),
            "h3" => Some(Self::H3),
            "h4" => Some(Self::H4),
            "h5" => Some(Self::H5),
            "h6" => Some(Self::H6),
            "code" => Some(Self::Code),
            "quote" => Some(Self::Quote),
            "list" => Some(Self::List),
            "table" => Some(Self::Table),
            "rule" => Some(Self::Rule),
            _ => None,
        }
    }
}

/// Inline flags for a panel span.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PanelSpanStyle(u8);

impl PanelSpanStyle {
    pub const BOLD: u8 = 1 << 0;
    pub const ITALIC: u8 = 1 << 1;
    pub const UNDERLINE: u8 = 1 << 2;
    pub const STRIKE: u8 = 1 << 3;
    pub const CODE: u8 = 1 << 4;
    pub const LINK: u8 = 1 << 5;
    pub const MUTED: u8 = 1 << 6;

    pub fn parse(value: &str) -> Self {
        let mut flags = 0;
        for part in value.split_whitespace() {
            flags |= match part {
                "bold" => Self::BOLD,
                "italic" => Self::ITALIC,
                "underline" => Self::UNDERLINE,
                "strike" => Self::STRIKE,
                "code" => Self::CODE,
                "link" => Self::LINK,
                "muted" => Self::MUTED,
                _ => 0,
            };
        }
        Self(flags)
    }

    pub fn contains(self, flag: u8) -> bool {
        self.0 & flag != 0
    }

    pub fn bold(self) -> bool {
        self.contains(Self::BOLD)
    }

    pub fn italic(self) -> bool {
        self.contains(Self::ITALIC)
    }

    pub fn underline(self) -> bool {
        self.contains(Self::UNDERLINE)
    }

    pub fn strike(self) -> bool {
        self.contains(Self::STRIKE)
    }

    pub fn code(self) -> bool {
        self.contains(Self::CODE)
    }

    pub fn link(self) -> bool {
        self.contains(Self::LINK)
    }

    pub fn muted(self) -> bool {
        self.contains(Self::MUTED)
    }
}

/// One inline run inside a panel line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelSpan {
    pub text: String,
    pub style: PanelSpanStyle,
    /// Syntax role resolved against the active syntax theme.
    pub role: Option<String>,
}

struct CallState {
    plugin: PluginIdentity,
    permissions: PermissionSet,
    document: Option<DocumentSnapshot>,
    settings: Map<String, JsonValue>,
    allowed_roots: Vec<PathBuf>,
    status_ids: BTreeSet<String>,
    /// Declared setting schemas, for `luban.settings.set` validation.
    schemas: Vec<SettingContribution>,
    /// Events declared in the manifest, for `luban.events.on` validation.
    allowed_events: BTreeSet<String>,
    command_depth: u8,
    context_kind: CallContextKind,
    actions: Vec<HostAction>,
}

/// One registered timer. `interval` is `None` for one-shot timers
/// (`luban.timer.after`) and `Some(period)` for repeating ones
/// (`luban.timer.every`). Entries live in the runtime's timer map, so
/// rebuilding the VM (a fresh [`PluginRuntime`]) clears them automatically.
#[derive(Clone)]
struct TimerEntry {
    callback: mlua::Function,
    next_due: std::time::Instant,
    interval: Option<std::time::Duration>,
}

pub struct PluginRuntime {
    manifest: PluginManifest,
    dir: PathBuf,
    permissions: PermissionSet,
    lua: Lua,
    plugin: Table,
    error_handler: mlua::Function,
    remaining: Rc<Cell<u64>>,
    limit_hit: Rc<Cell<bool>>,
    instruction_limit: u64,
    /// Registered event handlers keyed by event name; shared with the
    /// `luban.events` closures installed on every call so registrations made
    /// during one dispatch survive into later ones.
    event_handlers: Rc<RefCell<BTreeMap<String, Vec<mlua::Function>>>>,
    /// Registered timers keyed by id; shared with the `luban.timer` closures
    /// installed on every call (so registrations made during one dispatch
    /// survive into later ones) and drained by the host pump via
    /// [`Self::fire_timer`]. Entries live here, not in the Lua VM, so
    /// rebuilding the VM clears them.
    timers: Rc<RefCell<BTreeMap<String, TimerEntry>>>,
    /// Consecutive failed dispatches; any successful dispatch resets it.
    /// The host quarantines the plugin when [`Self::should_quarantine`].
    consecutive_failures: Cell<u32>,
}

impl PluginRuntime {
    pub fn new(
        manifest: &PluginManifest,
        dir: &Path,
        permissions: PermissionSet,
    ) -> Result<Self, String> {
        let lua = Lua::new_with(
            StdLib::TABLE | StdLib::STRING | StdLib::UTF8 | StdLib::MATH,
            LuaOptions::default(),
        )
        .map_err(|error| format!("cannot create Lua state: {error}"))?;
        if let Err(error) = lua.set_memory_limit(MEMORY_LIMIT) {
            log::warn!("cannot set plugin memory limit: {error}");
        }
        let globals = lua.globals();
        for name in [
            "load",
            "loadfile",
            "dofile",
            "collectgarbage",
            "package",
            "io",
            "os",
            "debug",
            "coroutine",
            "print",
            "warn",
        ] {
            globals
                .set(name, Value::Nil)
                .map_err(|error| format!("cannot sandbox Lua state: {error}"))?;
        }

        // Sandboxed in-plugin require (spec §8): `require("lib.parser")` loads
        // and evaluates `<plugin-dir>/lib/parser.lua` inside this same Lua
        // state, so a module sees the main script's globals and the same
        // sandbox. The `package` library stays invisible; loaded modules are
        // cached for the VM's lifetime, and module code runs inside the
        // dispatching call, so it consumes that dispatch's instruction budget
        // under the same debug hook. `loading` marks modules whose chunk is
        // currently being evaluated — the only point where a cycle can
        // re-enter `require` — so a circular dependency raises instead of
        // recursing to a Rust stack overflow.
        //
        // Notes for plugin authors: `require('main')` re-evaluates main.lua as
        // a module, so its top-level side effects run a second time; and a
        // module whose body returns nil caches and returns nil (stock Lua's
        // require would return true instead), so side-effect modules should
        // return an explicit table or `true`.
        let module_cache: Rc<RefCell<HashMap<String, Value>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let loading: Rc<RefCell<HashSet<String>>> = Rc::new(RefCell::new(HashSet::new()));
        let plugin_dir = dir.to_path_buf();
        let require = lua
            .create_function(move |lua, name: String| {
                let cached = module_cache.borrow().get(&name).cloned();
                if let Some(value) = cached {
                    return Ok(value);
                }
                if loading.borrow().contains(&name) {
                    return Err(mlua::Error::RuntimeError(format!(
                        "module '{name}': circular dependency"
                    )));
                }
                let path =
                    resolve_module_path(&plugin_dir, &name).map_err(mlua::Error::RuntimeError)?;
                // Same resource bound as `luban.fs.read`: an over-large module
                // file fails the load instead of being slurped whole.
                let metadata = fs::metadata(&path).map_err(|error| {
                    mlua::Error::RuntimeError(format!("module '{name}': {error}"))
                })?;
                if metadata.len() > MAX_FILE_BYTES {
                    return Err(mlua::Error::RuntimeError(format!(
                        "module '{name}': file is larger than {MAX_FILE_BYTES} bytes"
                    )));
                }
                let source = fs::read_to_string(&path).map_err(|error| {
                    mlua::Error::RuntimeError(format!("module '{name}': {error}"))
                })?;
                // Past all fallible I/O: mark the module as loading only while its
                // chunk runs, and always drop the marker afterwards so a failed
                // load (e.g. pcall-caught by the plugin) cannot poison later
                // requires of the same name. No RefCell borrow is held across
                // `eval`, so nested requires can re-enter this closure safely.
                loading.borrow_mut().insert(name.clone());
                let evaluated: mlua::Result<Value> =
                    lua.load(&source).set_name(format!("@{name}.lua")).eval();
                loading.borrow_mut().remove(&name);
                // Propagate the original eval error unwrapped: re-wrapping it
                // as a string would bake one mlua Display prefix plus one
                // traceback blob into the message per require nesting level
                // (a 600-deep chain produced a 122 KB error). mlua's
                // CallbackError Display already traces errors down to their
                // root and deduplicates the nested tracebacks, so the final
                // message stays bounded. Plugins see the same WrappedFailure
                // error object through pcall as for any other host API error.
                let value = evaluated?;
                module_cache.borrow_mut().insert(name, value.clone());
                Ok(value)
            })
            .map_err(|error| format!("cannot install plugin require: {error}"))?;
        globals
            .set("require", require)
            .map_err(|error| format!("cannot install plugin require: {error}"))?;
        let remaining = Rc::new(Cell::new(DEFAULT_INSTRUCTION_LIMIT));
        let limit_hit = Rc::new(Cell::new(false));
        let hook_remaining = remaining.clone();
        let hook_limit_hit = limit_hit.clone();
        lua.set_hook(
            HookTriggers::default().every_nth_instruction(HOOK_STEP as u32),
            move |_, _| {
                let left = hook_remaining.get();
                if left <= HOOK_STEP {
                    hook_remaining.set(0);
                    hook_limit_hit.set(true);
                    return Err(mlua::Error::RuntimeError(
                        "plugin instruction limit exceeded".to_string(),
                    ));
                }
                hook_remaining.set(left - HOOK_STEP);
                Ok(VmState::Continue)
            },
        )
        .map_err(|error| format!("cannot install plugin instruction limit: {error}"))?;

        let _ = fs::create_dir_all(data_dir(dir));
        let main = manifest.main_path(dir)?;
        let source = fs::read_to_string(&main)
            .map_err(|error| format!("cannot read {}: {error}", main.display()))?;
        let initial = CallInput {
            plugin: PluginIdentity {
                id: manifest.id.clone(),
                version: manifest.version.clone(),
                data_dir: data_dir(dir),
            },
            document: None,
            settings: Map::new(),
            allowed_roots: vec![dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf())],
            command_depth: 0,
            context_kind: CallContextKind::default(),
        };
        let status_ids: BTreeSet<String> = manifest
            .contributes
            .status_bar
            .iter()
            .map(|item| item.id.clone())
            .collect();
        let allowed_events: BTreeSet<String> = manifest.events.iter().cloned().collect();
        let state = Rc::new(RefCell::new(CallState::from_input(
            initial,
            &status_ids,
            &manifest.contributes.settings,
            &allowed_events,
        )));
        state.borrow_mut().permissions = permissions.clone();
        let event_handlers = Rc::new(RefCell::new(BTreeMap::new()));
        let timers = Rc::new(RefCell::new(BTreeMap::new()));
        install_luban_table(&lua, state.clone(), event_handlers.clone(), timers.clone())
            .map_err(|error| format!("cannot initialize plugin API: {error}"))?;
        let handler_limit_hit = limit_hit.clone();
        // Rust-raised v2 API errors (`Err(mlua::Error::RuntimeError(..))` from
        // the closures below) reach this handler as mlua WrappedFailure
        // userdata whose message lives behind `__tostring`. Stringify through
        // Lua's `tostring` (captured before any plugin code can override it);
        // `coerce_string` cannot see that metamethod and would lose the
        // message.
        let handler_tostring: mlua::Function = lua
            .globals()
            .get("tostring")
            .map_err(|error| format!("cannot install plugin error handler: {error}"))?;
        let error_handler = lua
            .create_function(move |lua, message: Value| {
                let message = if handler_limit_hit.get() {
                    "plugin instruction limit exceeded".to_string()
                } else {
                    handler_tostring
                        .call::<String>(message)
                        .unwrap_or_else(|_| "plugin error".to_string())
                };
                match lua.traceback(Some(&message), 1) {
                    Ok(traceback) => Ok(Value::String(traceback)),
                    Err(_) => Ok(Value::String(lua.create_string(&message)?)),
                }
            })
            .map_err(|error| format!("cannot install plugin error handler: {error}"))?;
        let value: Value = lua
            .load(&source)
            .set_name(format!("@{}", manifest.main))
            .eval()
            .map_err(|error| format!("cannot load plugin '{}': {error}", manifest.id))?;
        let plugin = match value {
            Value::Table(table) => table,
            _ => {
                return Err(format!(
                    "plugin '{}' must return a table from {}",
                    manifest.id, manifest.main
                ))
            }
        };
        Ok(Self {
            manifest: manifest.clone(),
            dir: dir.to_path_buf(),
            permissions,
            lua,
            plugin,
            error_handler,
            remaining,
            limit_hit,
            instruction_limit: DEFAULT_INSTRUCTION_LIMIT,
            event_handlers,
            timers,
            consecutive_failures: Cell::new(0),
        })
    }

    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    pub fn permissions(&self) -> &PermissionSet {
        &self.permissions
    }

    pub fn data_dir(&self) -> PathBuf {
        data_dir(&self.dir)
    }

    pub fn activate(&mut self, input: CallInput) -> RuntimeCall<()> {
        let Some(name) = self.manifest.activate.clone() else {
            return RuntimeCall {
                output: RuntimeOutput::default(),
                value: Some(()),
                error: None,
            };
        };
        self.invoke(&name, None, input).map_value(|_| ())
    }

    pub fn deactivate(&mut self, input: CallInput) -> RuntimeCall<()> {
        let Some(name) = self.manifest.deactivate.clone() else {
            return RuntimeCall {
                output: RuntimeOutput::default(),
                value: Some(()),
                error: None,
            };
        };
        self.invoke(&name, None, input).map_value(|_| ())
    }

    pub fn call_command(
        &mut self,
        entry: &str,
        command_id: &str,
        input: CallInput,
    ) -> RuntimeCall<()> {
        let call = self
            .invoke(entry, Some(command_id), input)
            .map_value(|_| ());
        self.record_outcome(call.error.is_none());
        call
    }

    pub fn call_panel(&mut self, entry: &str, input: CallInput) -> RuntimeCall<PanelContent> {
        let call = self.invoke(entry, None, input);
        let call = match call.value {
            Some(value) if call.error.is_none() => match parse_panel(value) {
                Ok(panel) => RuntimeCall {
                    output: call.output,
                    value: Some(panel),
                    error: None,
                },
                Err(error) => RuntimeCall {
                    output: call.output,
                    value: None,
                    error: Some(error),
                },
            },
            _ => RuntimeCall {
                output: call.output,
                value: None,
                error: call.error,
            },
        };
        self.record_outcome(call.error.is_none());
        call
    }

    /// Dispatch an event: run every registered handler in registration order
    /// with `(context, nil)`. Handlers share one call state, so host actions
    /// accumulate across the whole dispatch; the context table is shared too,
    /// so later handlers observe mutations made by earlier ones. A failing
    /// handler does not stop the remaining ones — the first failure is
    /// surfaced and the whole dispatch counts as one quarantine step.
    ///
    /// The handler list is a snapshot taken before the dispatch starts:
    /// handlers registered during the dispatch — including by the handlers
    /// themselves — do not fire in it, and handlers removed during it still
    /// fire. The instruction budget is shared across all handlers of the
    /// dispatch, so a runaway handler can starve later ones into limit
    /// failures; the whole dispatch still counts as one failure step.
    pub fn dispatch_event(&mut self, event: &str, input: CallInput) -> RuntimeCall<()> {
        let handlers = self
            .event_handlers
            .borrow()
            .get(event)
            .cloned()
            .unwrap_or_default();
        if handlers.is_empty() {
            // A no-op dispatch is still a successful dispatch: it resets the
            // consecutive-failure counter like any other success.
            self.record_outcome(true);
            return RuntimeCall {
                output: RuntimeOutput::default(),
                value: Some(()),
                error: None,
            };
        }
        let state = self.prepare_call(input);
        let context = match install_luban_table(
            &self.lua,
            state.clone(),
            self.event_handlers.clone(),
            self.timers.clone(),
        )
        .and_then(|()| context_table(&self.lua, &state, None))
        {
            Ok(context) => context,
            Err(error) => {
                self.record_outcome(false);
                return RuntimeCall {
                    output: RuntimeOutput::default(),
                    value: None,
                    error: Some(format!("cannot prepare plugin call: {error}")),
                };
            }
        };
        self.remaining.set(self.instruction_limit);
        self.limit_hit.set(false);
        let mut first_error: Option<String> = None;
        for handler in &handlers {
            if let Err(error) = call_with_traceback(
                &self.lua,
                handler,
                &self.error_handler,
                (context.clone(), Value::Nil),
            ) {
                let message = format_plugin_error(&self.manifest.id, event, &error);
                if first_error.is_none() {
                    first_error = Some(message);
                }
            }
        }
        let output = RuntimeOutput {
            actions: state.borrow().actions.clone(),
        };
        match first_error {
            None => {
                self.record_outcome(true);
                RuntimeCall {
                    output,
                    value: Some(()),
                    error: None,
                }
            }
            Some(error) => {
                self.record_outcome(false);
                RuntimeCall {
                    output,
                    value: None,
                    error: Some(error),
                }
            }
        }
    }

    /// Explicit registration helper: resolve `handler_name` in the plugin
    /// table and register it for a manifest-declared event, with the same
    /// validation as `luban.events.on` (declared event, handler limit).
    pub fn register_event(&mut self, event: &str, handler_name: &str) -> Result<(), String> {
        let handler: mlua::Function = self.plugin.get(handler_name).map_err(|_| {
            format!(
                "plugin '{}' has no function '{handler_name}'",
                self.manifest.id
            )
        })?;
        let allowed: BTreeSet<String> = self.manifest.events.iter().cloned().collect();
        let mut registry = self.event_handlers.borrow_mut();
        register_handler(&mut registry, &allowed, event.to_string(), handler)
    }

    /// Explicit registration helper / future host use: resolve `handler_name`
    /// in the plugin table and register it as a timer, with the same rules as
    /// `luban.timer.after/every` (id validation, interval clamp, active-timer
    /// cap, overwrite of an existing id). Timers are not declared in the
    /// manifest, so no event-style allow-list applies.
    pub fn register_timer(
        &mut self,
        id: &str,
        ms: u64,
        handler_name: &str,
        one_shot: bool,
    ) -> Result<(), String> {
        let callback: mlua::Function = self.plugin.get(handler_name).map_err(|_| {
            format!(
                "plugin '{}' has no function '{handler_name}'",
                self.manifest.id
            )
        })?;
        register_timer_entry(
            &mut self.timers.borrow_mut(),
            id.to_string(),
            callback,
            ms,
            one_shot,
        )
    }

    /// Ids of the timers due at `now`, in `BTreeMap` key (alphabetical) order.
    /// The host pump calls this every tick and feeds each id to
    /// [`Self::fire_timer`].
    pub fn due_timer_ids(&self, now: std::time::Instant) -> Vec<String> {
        self.timers
            .borrow()
            .iter()
            .filter(|(_, entry)| entry.next_due <= now)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Fire one timer, dispatched like an event handler: same call
    /// preparation, instruction budget, `(context, nil)` argument shape and
    /// quarantine counting. The `input`'s `context_kind` is forced to
    /// [`CallContextKind::Event`] before dispatch (spec §6 and the §10 flood
    /// matrix): timer dispatch always shares the Event context, so a host
    /// call site cannot accidentally enable document/command actions from a
    /// timer.
    ///
    /// The entry is taken out of the map before any plugin code runs (no
    /// borrow guard is live across Lua, mirroring `dispatch_event`'s handler
    /// snapshot). A repeating timer is reinserted *before* dispatch with
    /// `next_due = now + interval`, so a handler that registers or cancels
    /// timers cannot corrupt the iteration and an overrun still schedules the
    /// next fire relative to now — no catch-up stacking. A one-shot timer
    /// stays removed, so firing an expired id is an error.
    ///
    /// The `Err` variant is reserved for unknown ids; a failing callback is
    /// contained in `RuntimeCall::error` and counted via `record_outcome`.
    ///
    /// Pump contract (Task 10): `due_timer_ids` can return up to `MAX_TIMERS`
    /// ids per tick, but the pump caps timer dispatches per tick (8) —
    /// leftover due timers wait for the next tick. An `Err` means the id
    /// vanished mid-batch (e.g. an earlier timer's callback cancelled it
    /// during the same tick): the pump must treat that as a benign skip —
    /// not a failure, no `record_failure`.
    pub fn fire_timer(&mut self, id: &str, input: CallInput) -> Result<RuntimeCall<()>, String> {
        let entry = self.timers.borrow_mut().remove(id);
        let Some(entry) = entry else {
            return Err(format!("timer '{id}' is not active"));
        };
        if let Some(interval) = entry.interval {
            self.timers.borrow_mut().insert(
                id.to_string(),
                TimerEntry {
                    callback: entry.callback.clone(),
                    next_due: std::time::Instant::now() + interval,
                    interval: Some(interval),
                },
            );
        }
        // Timer dispatch always shares the Event context (spec §6/§10); the
        // override happens here so host call sites cannot enable user-only
        // privileges by passing a different context_kind.
        let mut input = input;
        input.context_kind = CallContextKind::Event;
        let state = self.prepare_call(input);
        let context = match install_luban_table(
            &self.lua,
            state.clone(),
            self.event_handlers.clone(),
            self.timers.clone(),
        )
        .and_then(|()| context_table(&self.lua, &state, None))
        {
            Ok(context) => context,
            Err(error) => {
                self.record_outcome(false);
                return Ok(RuntimeCall {
                    output: RuntimeOutput::default(),
                    value: None,
                    error: Some(format!("cannot prepare plugin call: {error}")),
                });
            }
        };
        self.remaining.set(self.instruction_limit);
        self.limit_hit.set(false);
        let error = call_with_traceback(
            &self.lua,
            &entry.callback,
            &self.error_handler,
            (context, Value::Nil),
        )
        .err()
        .map(|error| format_plugin_error(&self.manifest.id, id, &error));
        let output = RuntimeOutput {
            actions: state.borrow().actions.clone(),
        };
        self.record_outcome(error.is_none());
        Ok(RuntimeCall {
            output,
            value: Some(()),
            error,
        })
    }

    /// Record a dispatch outcome for the consecutive-failure quarantine
    /// counter: success resets it, failure bumps it.
    fn record_outcome(&self, ok: bool) {
        if ok {
            self.consecutive_failures.set(0);
        } else {
            self.consecutive_failures
                .set(self.consecutive_failures.get().saturating_add(1));
        }
    }

    /// True once the plugin failed `QUARANTINE_THRESHOLD` dispatches in a row.
    /// The app side auto-disables the plugin (Task 10).
    pub fn should_quarantine(&self) -> bool {
        self.consecutive_failures.get() >= QUARANTINE_THRESHOLD
    }

    /// Current consecutive-failure count (test/introspection helper).
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures.get()
    }

    /// Host-side quarantine bookkeeping (Task 9): count a failed dispatch on
    /// the same counter the runtime uses, so host-path failures (e.g. panics
    /// while applying plugin actions) contribute to auto-disable too.
    pub fn record_failure(&self) {
        self.record_outcome(false);
    }

    /// Host-side quarantine bookkeeping (Task 9): count a successful dispatch.
    pub fn record_success(&self) {
        self.record_outcome(true);
    }

    /// Host-side quarantine bookkeeping (Task 9): clear the counter (e.g.
    /// after the user manually re-enables a quarantined plugin).
    pub fn reset_failures(&self) {
        self.consecutive_failures.set(0);
    }

    /// Shared per-dispatch preparation for the command, panel, event and
    /// timer paths: build the call state from the input. The instruction
    /// budget reset happens at dispatch time, so every dispatch invocation
    /// starts fresh.
    fn prepare_call(&self, input: CallInput) -> Rc<RefCell<CallState>> {
        let status_ids: BTreeSet<String> = self
            .manifest
            .contributes
            .status_bar
            .iter()
            .map(|item| item.id.clone())
            .collect();
        let allowed_events: BTreeSet<String> = self.manifest.events.iter().cloned().collect();
        let mut call_state = CallState::from_input(
            input,
            &status_ids,
            &self.manifest.contributes.settings,
            &allowed_events,
        );
        call_state.permissions = self.permissions.clone();
        Rc::new(RefCell::new(call_state))
    }

    fn invoke(
        &mut self,
        entry: &str,
        command_id: Option<&str>,
        input: CallInput,
    ) -> RuntimeCall<Value> {
        let state = self.prepare_call(input);
        let output = match install_luban_table(
            &self.lua,
            state.clone(),
            self.event_handlers.clone(),
            self.timers.clone(),
        )
        .and_then(|()| context_table(&self.lua, &state, command_id))
        {
            Ok(context) => {
                self.remaining.set(self.instruction_limit);
                self.limit_hit.set(false);
                let function = match self.plugin.get::<mlua::Function>(entry) {
                    Ok(function) => function,
                    Err(error) => {
                        return RuntimeCall {
                            output: RuntimeOutput {
                                actions: state.borrow().actions.clone(),
                            },
                            value: None,
                            error: Some(format!(
                                "plugin '{}' has no function '{}': {error}",
                                self.manifest.id, entry
                            )),
                        }
                    }
                };
                match call_with_traceback(&self.lua, &function, &self.error_handler, context) {
                    Ok(value) => RuntimeCall {
                        output: RuntimeOutput {
                            actions: state.borrow().actions.clone(),
                        },
                        value: Some(value),
                        error: None,
                    },
                    Err(error) => RuntimeCall {
                        output: RuntimeOutput {
                            actions: state.borrow().actions.clone(),
                        },
                        value: None,
                        error: Some(format_plugin_error(&self.manifest.id, entry, &error)),
                    },
                }
            }
            Err(error) => RuntimeCall {
                output: RuntimeOutput::default(),
                value: None,
                error: Some(format!("cannot prepare plugin call: {error}")),
            },
        };
        output
    }
}

impl<T> RuntimeCall<T> {
    fn map_value<U>(self, map: impl FnOnce(T) -> U) -> RuntimeCall<U> {
        RuntimeCall {
            output: self.output,
            value: self.value.map(map),
            error: self.error,
        }
    }
}

impl CallState {
    fn from_input(
        input: CallInput,
        status_ids: &BTreeSet<String>,
        schemas: &[SettingContribution],
        allowed_events: &BTreeSet<String>,
    ) -> Self {
        Self {
            plugin: input.plugin,
            permissions: PermissionSet::empty(),
            document: input.document,
            settings: input.settings,
            allowed_roots: input.allowed_roots,
            status_ids: status_ids.clone(),
            schemas: schemas.to_vec(),
            allowed_events: allowed_events.clone(),
            command_depth: input.command_depth,
            context_kind: input.context_kind,
            actions: Vec::new(),
        }
    }
}

fn data_dir(plugin_dir: &Path) -> PathBuf {
    plugin_dir.join("data")
}

/// Errors stay readable in the status bar but keep the full Lua traceback in
/// the log. The spec caps the visible summary at 2 KiB.
const MAX_PLUGIN_ERROR_CHARS: usize = 2048;

fn truncate_plugin_error(text: &str) -> String {
    let count = text.chars().count();
    if count <= MAX_PLUGIN_ERROR_CHARS {
        return text.to_string();
    }
    let kept: String = text.chars().take(MAX_PLUGIN_ERROR_CHARS).collect();
    format!("{kept}… [{count} characters total]")
}

fn format_plugin_error(id: &str, entry: &str, error: &mlua::Error) -> String {
    let full = error.to_string();
    log::error!("plugin '{id}' failed in '{entry}': {full}");
    format!(
        "plugin '{id}' failed in '{entry}': {}",
        truncate_plugin_error(&full)
    )
}

/// Call a plugin entry through `xpcall` so Lua errors carry a traceback from
/// the point where they were raised. The sandbox has no `debug` library, so
/// the message handler is a Rust function backed by `Lua::traceback`. `args`
/// becomes the trailing arguments of the call (a single table for
/// command/panel entries, `(context, nil)` for event handlers).
fn call_with_traceback(
    lua: &Lua,
    function: &mlua::Function,
    handler: &mlua::Function,
    args: impl IntoLuaMulti,
) -> mlua::Result<Value> {
    let xpcall: mlua::Function = lua.globals().get("xpcall")?;
    let mut argv: mlua::MultiValue = args.into_lua_multi(lua)?;
    argv.push_front(Value::Function(handler.clone()));
    argv.push_front(Value::Function(function.clone()));
    let result: mlua::MultiValue = xpcall.call(argv)?;
    let mut values = result.into_iter();
    let succeeded = values
        .next()
        .and_then(|value| value.as_boolean())
        .unwrap_or(false);
    if succeeded {
        return Ok(values.next().unwrap_or(Value::Nil));
    }
    let message = values
        .next()
        .map(|value| match value {
            Value::String(text) => text.to_string_lossy(),
            other => format!("{other:?}"),
        })
        .unwrap_or_else(|| "plugin error".to_string());
    Err(mlua::Error::RuntimeError(message))
}

fn install_luban_table(
    lua: &Lua,
    state: Rc<RefCell<CallState>>,
    events_registry: Rc<RefCell<BTreeMap<String, Vec<mlua::Function>>>>,
    timers: Rc<RefCell<BTreeMap<String, TimerEntry>>>,
) -> mlua::Result<()> {
    let luban = lua.create_table()?;
    luban.set("version", 2)?;

    let log_plugin_id = state.borrow().plugin.id.clone();
    luban.set(
        "log",
        lua.create_function(move |_, (level, message): (String, String)| {
            let level = match level.to_ascii_lowercase().as_str() {
                "debug" => log::Level::Debug,
                "warn" | "warning" => log::Level::Warn,
                "error" => log::Level::Error,
                _ => log::Level::Info,
            };
            log::log!(target: "luban_plugin", level, "[{log_plugin_id}] {message}");
            Ok(())
        })?,
    )?;

    // Resource over-limits drop silently instead of failing the call.
    let notify_state = state.clone();
    luban.set(
        "notify",
        lua.create_function(move |_, message: String| {
            let mut state = notify_state.borrow_mut();
            let message = if message.len() > MAX_NOTIFY_BYTES {
                truncate_utf8(&message, MAX_NOTIFY_BYTES).to_string()
            } else {
                message
            };
            let count = state
                .actions
                .iter()
                .filter(|action| matches!(action, HostAction::Notify(_)))
                .count();
            if count >= MAX_NOTIFY_PER_CALL {
                log::warn!(
                    "plugin '{}' notify dropped (limit {MAX_NOTIFY_PER_CALL})",
                    state.plugin.id
                );
                return Ok(());
            }
            state.actions.push(HostAction::Notify(message));
            Ok(())
        })?,
    )?;

    let json = lua.create_table()?;
    json.set(
        "encode",
        lua.create_function(move |lua, value: Value| {
            let value = lua_to_json(&value, 0).map_err(mlua::Error::RuntimeError)?;
            let text = serde_json::to_string(&value)
                .map_err(|error| mlua::Error::RuntimeError(error.to_string()))?;
            Ok(Value::String(lua.create_string(&text)?))
        })?,
    )?;
    json.set(
        "decode",
        lua.create_function(move |lua, text: String| {
            let value: JsonValue = serde_json::from_str(&text)
                .map_err(|error| mlua::Error::RuntimeError(error.to_string()))?;
            json_to_lua(lua, &value)
        })?,
    )?;
    luban.set("json", json)?;

    let document = lua.create_table()?;
    let get_document_state = state.clone();
    document.set(
        "get",
        lua.create_function(move |lua, ()| {
            let state = get_document_state.borrow();
            match &state.document {
                Some(document) => Ok(Value::Table(document_to_lua(lua, document)?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;
    let set_text_state = state.clone();
    document.set(
        "set_text",
        lua.create_function(
            move |_, (text, start, end): (String, Option<usize>, Option<usize>)| {
                let mut state = set_text_state.borrow_mut();
                ensure_document_writable(&state)?;
                let selection = match (start, end) {
                    (None, None) => None,
                    (Some(start), Some(end)) => Some((start, end)),
                    _ => {
                        return Err(mlua::Error::RuntimeError(
                            "set_text expects (text, sel_start, sel_end) with both offsets".into(),
                        ))
                    }
                };
                replace_document(&mut state, text, selection).map_err(mlua::Error::RuntimeError)?;
                Ok(())
            },
        )?,
    )?;
    let insert_state = state.clone();
    document.set(
        "insert",
        lua.create_function(move |_, text: String| {
            let mut state = insert_state.borrow_mut();
            ensure_document_writable(&state)?;
            insert_document(&mut state, text).map_err(mlua::Error::RuntimeError)?;
            Ok(())
        })?,
    )?;
    let cursor_state = state.clone();
    document.set(
        "set_cursor",
        lua.create_function(move |_, (line, col): (usize, usize)| {
            if line == 0 || col == 0 {
                return Err(mlua::Error::RuntimeError("line and col are 1-based".into()));
            }
            cursor_state
                .borrow_mut()
                .actions
                .push(HostAction::SetCursor { line, col });
            Ok(())
        })?,
    )?;
    luban.set("document", document)?;

    let settings = lua.create_table()?;
    let settings_state = state.clone();
    settings.set(
        "get",
        lua.create_function(move |lua, key: String| {
            let state = settings_state.borrow();
            match state.settings.get(&key) {
                Some(value) => json_to_lua(lua, value),
                None => Ok(Value::Nil),
            }
        })?,
    )?;
    let set_setting_state = state.clone();
    settings.set(
        "set",
        lua.create_function(move |_, (key, value): (String, Value)| {
            let mut state = set_setting_state.borrow_mut();
            let json = lua_to_json(&value, 0).map_err(mlua::Error::RuntimeError)?;
            validate_setting(&state, &key, json.clone()).map_err(mlua::Error::RuntimeError)?;
            // Optimistic same-dispatch readback: the host applies SetSetting
            // only after the call returns, but spec §5 promises that a
            // read-after-write sees the new value immediately. The host apply
            // self-corrects any divergence on the next dispatch.
            state.settings.insert(key.clone(), json.clone());
            state
                .actions
                .push(HostAction::SetSetting { key, value: json });
            Ok(())
        })?,
    )?;
    luban.set("settings", settings)?;

    let status = lua.create_table()?;
    let status_state = state.clone();
    status.set(
        "set",
        lua.create_function(move |_, (id, text): (String, String)| {
            let mut state = status_state.borrow_mut();
            if !state.status_ids.contains(&id) {
                return Err(mlua::Error::RuntimeError(format!(
                    "status item '{id}' is not declared"
                )));
            }
            state.actions.push(HostAction::SetStatus { id, text });
            Ok(())
        })?,
    )?;
    luban.set("status", status)?;

    let syntax = lua.create_table()?;
    syntax.set(
        "highlight",
        lua.create_function(move |lua, (text, language): (String, String)| {
            if text.len() > MAX_HIGHLIGHT_BYTES {
                return Err(mlua::Error::RuntimeError(format!(
                    "text is larger than {MAX_HIGHLIGHT_BYTES} bytes"
                )));
            }
            let spans = luban_core::syntax::highlight_spans(&text, &language);
            let table = lua.create_table()?;
            for (index, (part, role)) in spans.into_iter().enumerate() {
                let item = lua.create_table()?;
                item.set("text", part)?;
                item.set("role", role)?;
                table.set(index + 1, item)?;
            }
            Ok(Value::Table(table))
        })?,
    )?;
    luban.set("syntax", syntax)?;

    let panel = lua.create_table()?;
    let panel_state = state.clone();
    panel.set(
        "open",
        lua.create_function(move |_, id: String| {
            validate_resource_id(&id, "panel").map_err(mlua::Error::RuntimeError)?;
            panel_state
                .borrow_mut()
                .actions
                .push(HostAction::OpenPanel { id });
            Ok(())
        })?,
    )?;
    let refresh_state = state.clone();
    panel.set(
        "refresh",
        lua.create_function(move |_, id: String| {
            validate_resource_id(&id, "panel").map_err(mlua::Error::RuntimeError)?;
            refresh_state
                .borrow_mut()
                .actions
                .push(HostAction::RefreshPanel { id });
            Ok(())
        })?,
    )?;
    luban.set("panel", panel)?;

    let fs = lua.create_table()?;
    let read_state = state.clone();
    fs.set(
        "read",
        lua.create_function(move |lua, path: String| {
            let state = read_state.borrow();
            if !state.permissions.filesystem() {
                return Err(mlua::Error::RuntimeError(
                    "plugin does not have the filesystem permission".into(),
                ));
            }
            let path =
                resolve_allowed_path(&state, &path, false).map_err(mlua::Error::RuntimeError)?;
            let metadata = fs::metadata(&path)
                .map_err(|error| mlua::Error::RuntimeError(error.to_string()))?;
            if metadata.len() > MAX_FILE_BYTES {
                return Err(mlua::Error::RuntimeError(format!(
                    "file is larger than {MAX_FILE_BYTES} bytes"
                )));
            }
            let text = fs::read_to_string(&path)
                .map_err(|error| mlua::Error::RuntimeError(error.to_string()))?;
            Ok(Value::String(lua.create_string(&text)?))
        })?,
    )?;
    let write_state = state.clone();
    fs.set(
        "write",
        lua.create_function(move |_, (path, text): (String, String)| {
            let state = write_state.borrow();
            if !state.permissions.filesystem() {
                return Err(mlua::Error::RuntimeError(
                    "plugin does not have the filesystem permission".into(),
                ));
            }
            if text.len() > MAX_DOCUMENT_BYTES {
                return Err(mlua::Error::RuntimeError(format!(
                    "text is larger than {MAX_DOCUMENT_BYTES} bytes"
                )));
            }
            let path =
                resolve_allowed_path(&state, &path, true).map_err(mlua::Error::RuntimeError)?;
            luban_core::atomic_write::write_atomic(&path, text.as_bytes())
                .map_err(|error| mlua::Error::RuntimeError(error.to_string()))?;
            Ok(())
        })?,
    )?;
    luban.set("fs", fs)?;

    let data_dir_state = state.clone();
    luban.set(
        "data_dir",
        lua.create_function(move |lua, ()| {
            let state = data_dir_state.borrow();
            fs::create_dir_all(&state.plugin.data_dir).map_err(|error| {
                mlua::Error::RuntimeError(format!("cannot create plugin data directory: {error}"))
            })?;
            Ok(Value::String(lua.create_string(
                state.plugin.data_dir.to_string_lossy().as_bytes(),
            )?))
        })?,
    )?;

    let command = lua.create_table()?;
    let run_state = state.clone();
    command.set(
        "run",
        lua.create_function(move |_, (id, args): (String, Option<Value>)| {
            validate_resource_id(&id, "command").map_err(mlua::Error::RuntimeError)?;
            let mut state = run_state.borrow_mut();
            let args = match args {
                Some(value) => Some(lua_to_json(&value, 0).map_err(mlua::Error::RuntimeError)?),
                None => None,
            };
            let depth = state.command_depth.saturating_add(1);
            if depth > MAX_COMMAND_DEPTH {
                return Err(mlua::Error::RuntimeError(format!(
                    "command.run exceeds the maximum depth of {MAX_COMMAND_DEPTH}"
                )));
            }
            state
                .actions
                .push(HostAction::RunCommand { id, args, depth });
            Ok(())
        })?,
    )?;
    luban.set("command", command)?;

    let events_api = lua.create_table()?;
    let on_events = events_registry.clone();
    let on_allowed: BTreeSet<String> = state.borrow().allowed_events.clone();
    events_api.set(
        "on",
        lua.create_function(move |_, (name, handler): (String, mlua::Function)| {
            let mut registry = on_events.borrow_mut();
            register_handler(&mut registry, &on_allowed, name, handler)
                .map_err(mlua::Error::RuntimeError)
        })?,
    )?;
    let off_events = events_registry.clone();
    events_api.set(
        "off",
        lua.create_function(move |_, name: String| {
            off_events.borrow_mut().remove(&name);
            Ok(())
        })?,
    )?;
    luban.set("events", events_api)?;

    // Timers: `after` (one-shot) and `every` (repeating) register callbacks in
    // the runtime's timer map; the host pump dispatches due timers through
    // `PluginRuntime::fire_timer`, which shares the event-dispatch path
    // (instruction budget, error containment, quarantine counting). Entries
    // live in the runtime struct, so rebuilding the VM clears them. Very
    // large intervals register fine and simply never fire (the clock is a
    // monotonic `Instant` per boot) while occupying one of the 16 slots until
    // the timer is cancelled or the VM is rebuilt.
    let timer = lua.create_table()?;
    timer.set("after", install_timer_fn(lua, timers.clone(), true)?)?;
    timer.set("every", install_timer_fn(lua, timers.clone(), false)?)?;
    timer.set(
        "cancel",
        lua.create_function(move |_, id: String| {
            timers.borrow_mut().remove(&id);
            Ok(())
        })?,
    )?;
    luban.set("timer", timer)?;

    lua.globals().set("luban", luban)?;
    Ok(())
}

/// Truncate to `limit` bytes, backing off to the last UTF-8 character boundary
/// so multi-byte text is never split mid-character.
fn truncate_utf8(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// v2 API: document mutations are user-command only. The `on_edit` context is
/// read-only and calls without a document raise fail-fast errors.
fn ensure_document_writable(state: &CallState) -> Result<(), mlua::Error> {
    if state.context_kind == CallContextKind::Edit {
        return Err(mlua::Error::RuntimeError(
            "document is read-only in the on_edit context".into(),
        ));
    }
    if state.document.is_none() {
        return Err(mlua::Error::RuntimeError("unsupported document".into()));
    }
    Ok(())
}

/// Check `key` against the plugin's declared setting schema.
fn validate_setting(state: &CallState, key: &str, value: JsonValue) -> Result<(), String> {
    let schema = state
        .schemas
        .iter()
        .find(|setting| setting.key == key)
        .ok_or_else(|| format!("setting '{key}' is not declared"))?;
    if !crate::manifest::setting_value_is_valid(schema, &value) {
        return Err(format!("invalid value for setting '{key}'"));
    }
    Ok(())
}

/// Validate a plugin-supplied resource id (panel, command or timer).
fn validate_resource_id(id: &str, what: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > MAX_ID_BYTES {
        return Err(format!("invalid {what} id"));
    }
    Ok(())
}

/// Shared event-handler registration rule for `luban.events.on` and the
/// Rust-side `PluginRuntime::register_event` helper: the event must be
/// declared in the manifest and the total handler count stays under the cap.
fn register_handler(
    registry: &mut BTreeMap<String, Vec<mlua::Function>>,
    allowed: &BTreeSet<String>,
    name: String,
    handler: mlua::Function,
) -> Result<(), String> {
    if !allowed.contains(&name) {
        return Err(format!("event '{name}' is not declared in the manifest"));
    }
    let total: usize = registry.values().map(Vec::len).sum();
    if total >= MAX_EVENT_HANDLERS {
        return Err(format!(
            "too many event handlers (limit {MAX_EVENT_HANDLERS})"
        ));
    }
    registry.entry(name).or_default().push(handler);
    Ok(())
}

/// Shared timer registration rule for `luban.timer.after/every` and the
/// Rust-side `PluginRuntime::register_timer` helper: the id passes
/// `validate_resource_id`, intervals below `MIN_TIMER_MS` are clamped up, the
/// active-timer count stays under the cap (overwriting an existing id is
/// always allowed) and a duplicate id replaces the previous entry outright.
fn register_timer_entry(
    timers: &mut BTreeMap<String, TimerEntry>,
    id: String,
    callback: mlua::Function,
    ms: u64,
    one_shot: bool,
) -> Result<(), String> {
    validate_resource_id(&id, "timer")?;
    let is_new = !timers.contains_key(&id);
    if is_new && timers.len() >= MAX_TIMERS {
        return Err(format!("too many active timers (limit {MAX_TIMERS})"));
    }
    let clamped = ms.max(MIN_TIMER_MS);
    let interval = std::time::Duration::from_millis(clamped);
    timers.insert(
        id,
        TimerEntry {
            callback,
            next_due: std::time::Instant::now() + interval,
            interval: if one_shot { None } else { Some(interval) },
        },
    );
    Ok(())
}

/// Build `luban.timer.after` / `luban.timer.every`: reject a negative
/// interval, then insert a `TimerEntry` via `register_timer_entry` (id
/// validation, interval clamp and the active-timer cap live there). Timers are
/// not declared in the manifest, so there is no event-style allow-list.
fn install_timer_fn(
    lua: &Lua,
    timers: Rc<RefCell<BTreeMap<String, TimerEntry>>>,
    one_shot: bool,
) -> mlua::Result<mlua::Function> {
    lua.create_function(
        move |_, (id, ms, callback): (String, i64, mlua::Function)| {
            if ms < 0 {
                return Err(mlua::Error::RuntimeError(
                    "timer interval must be non-negative".into(),
                ));
            }
            register_timer_entry(&mut timers.borrow_mut(), id, callback, ms as u64, one_shot)
                .map_err(mlua::Error::RuntimeError)
        },
    )
}

fn context_table(
    lua: &Lua,
    state: &Rc<RefCell<CallState>>,
    command_id: Option<&str>,
) -> mlua::Result<Table> {
    let state = state.borrow();
    let context = lua.create_table()?;
    let plugin = lua.create_table()?;
    plugin.set("id", state.plugin.id.as_str())?;
    plugin.set("version", state.plugin.version.as_str())?;
    plugin.set(
        "data_dir",
        state.plugin.data_dir.to_string_lossy().to_string(),
    )?;
    context.set("plugin", plugin)?;
    if let Some(command_id) = command_id {
        context.set("command", command_id)?;
    }
    if let Some(document) = &state.document {
        context.set("document", document_to_lua(lua, document)?)?;
    }
    context.set(
        "settings",
        json_to_lua(lua, &JsonValue::Object(state.settings.clone()))?,
    )?;
    Ok(context)
}

fn document_to_lua(lua: &Lua, document: &DocumentSnapshot) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set(
        "path",
        match &document.path {
            Some(path) => Value::String(lua.create_string(path.to_string_lossy().as_bytes())?),
            None => Value::Nil,
        },
    )?;
    table.set("text", document.text.as_str())?;
    table.set("language", document.language.as_str())?;
    table.set("line", document.line)?;
    table.set("column", document.column)?;
    table.set("selection_start", document.selection_start)?;
    table.set("selection_end", document.selection_end)?;
    table.set("modified", document.modified)?;
    Ok(table)
}

fn replace_document(
    state: &mut CallState,
    text: String,
    selection: Option<(usize, usize)>,
) -> Result<(), String> {
    if text.len() > MAX_DOCUMENT_BYTES {
        return Err(format!("text is larger than {MAX_DOCUMENT_BYTES} bytes"));
    }
    let Some(document) = state.document.as_mut() else {
        return Err("unsupported document".to_string());
    };
    let chars = text.chars().count();
    let (start, end) = selection.unwrap_or((document.selection_start, document.selection_end));
    if start > end || end > chars {
        return Err("invalid selection".to_string());
    }
    document.text = text.clone();
    document.selection_start = start;
    document.selection_end = end;
    document.modified = true;
    state.actions.push(HostAction::SetDocumentText {
        text,
        selection: Some((start, end)),
    });
    Ok(())
}

fn insert_document(state: &mut CallState, text: String) -> Result<(), String> {
    if text.len() > MAX_DOCUMENT_BYTES {
        return Err(format!("text is larger than {MAX_DOCUMENT_BYTES} bytes"));
    }
    let Some(document) = state.document.as_mut() else {
        return Err("unsupported document".to_string());
    };
    let chars = document.text.chars().count();
    let start = document.selection_start.min(chars);
    let end = document.selection_end.min(chars);
    if start > end {
        return Err("invalid selection".to_string());
    }
    let mut updated = String::with_capacity(document.text.len() + text.len());
    updated.extend(document.text.chars().take(start));
    updated.push_str(&text);
    updated.extend(document.text.chars().skip(end));
    let cursor = start + text.chars().count();
    document.text = updated.clone();
    document.selection_start = cursor;
    document.selection_end = cursor;
    document.modified = true;
    state.actions.push(HostAction::SetDocumentText {
        text: updated,
        selection: Some((cursor, cursor)),
    });
    Ok(())
}

fn resolve_allowed_path(state: &CallState, path: &str, write: bool) -> Result<PathBuf, String> {
    if path.trim().is_empty() {
        return Err("path is empty".to_string());
    }
    let raw = PathBuf::from(path);
    let candidate = if raw.is_absolute() {
        raw
    } else {
        state
            .allowed_roots
            .first()
            .cloned()
            .unwrap_or_else(|| state.plugin.data_dir.clone())
            .join(raw)
    };
    let resolved = if candidate.exists() {
        candidate
            .canonicalize()
            .map_err(|error| format!("cannot resolve {}: {error}", candidate.display()))?
    } else {
        let parent = candidate
            .parent()
            .ok_or_else(|| format!("invalid path {}", candidate.display()))?;
        let parent = parent
            .canonicalize()
            .map_err(|error| format!("cannot resolve {}: {error}", parent.display()))?;
        let name = candidate
            .file_name()
            .ok_or_else(|| format!("invalid path {}", candidate.display()))?;
        parent.join(name)
    };
    let mut roots = state.allowed_roots.clone();
    roots.push(state.plugin.data_dir.clone());
    let allowed = roots.iter().any(|root| {
        let root = root.canonicalize().unwrap_or_else(|_| root.clone());
        resolved.starts_with(root)
    });
    if !allowed {
        return Err(format!(
            "path {} is outside the allowed roots",
            resolved.display()
        ));
    }
    if !write && !resolved.is_file() {
        return Err(format!("{} is not a file", resolved.display()));
    }
    Ok(resolved)
}

/// Resolve a dot-separated module name to a Lua file inside the plugin
/// directory (spec §8): `lib.parser` maps to `<plugin-dir>/lib/parser.lua`.
/// Names may only contain `[a-z0-9._]`, every dot-separated segment must be
/// non-empty (so `..` and absolute paths are impossible by construction) and
/// the name itself must be 1..=256 bytes. As defense in depth against
/// symlinks, both the resolved file's parent and the file itself (when it
/// exists) must canonicalize inside the canonical plugin directory. A missing
/// module file keeps its resolved path so the caller's read surfaces
/// `module '<name>': ...`.
fn resolve_module_path(plugin_dir: &Path, name: &str) -> Result<PathBuf, String> {
    if name.is_empty() || name.len() > 256 {
        return Err("invalid module name".into());
    }
    let valid = name.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'.' || byte == b'_'
    });
    if !valid {
        return Err(format!("module name '{name}' may only contain [a-z0-9._]"));
    }
    if name.split('.').any(|part| part.is_empty() || part == "..") {
        return Err(format!(
            "module name '{name}' must not contain empty or '..' segments"
        ));
    }
    let mut path = plugin_dir.to_path_buf();
    for part in name.split('.') {
        path.push(part);
    }
    path.set_extension("lua");
    let parent = path.parent().expect("module path always has a parent");
    let resolved_parent = parent
        .canonicalize()
        .map_err(|error| format!("module '{name}': {error}"))?;
    let root = plugin_dir
        .canonicalize()
        .unwrap_or_else(|_| plugin_dir.to_path_buf());
    if !resolved_parent.starts_with(&root) {
        return Err(format!("module '{name}' escapes the plugin directory"));
    }
    let resolved = resolved_parent.join(
        path.file_name()
            .expect("module path always has a file name"),
    );
    // The file itself may be a symlink pointing outside the plugin
    // directory: canonicalizing an existing file resolves the link, and the
    // target must stay inside. A missing file keeps the resolved path —
    // the caller's read produces the `module '<name>': ...` error.
    if resolved.exists() {
        let resolved_file = resolved
            .canonicalize()
            .map_err(|error| format!("module '{name}': {error}"))?;
        if !resolved_file.starts_with(&root) {
            return Err(format!("module '{name}' escapes the plugin directory"));
        }
        return Ok(resolved_file);
    }
    Ok(resolved)
}

/// Recursion limit for Lua→JSON conversion; mirrors serde_json's own nesting
/// limit. `Table::pairs` executes no VM instructions, so the instruction hook
/// and the Lua memory limit cannot stop a cyclic table — only this guard can.
const MAX_JSON_DEPTH: usize = 128;

fn lua_to_json(value: &Value, depth: usize) -> Result<JsonValue, String> {
    if depth > MAX_JSON_DEPTH {
        return Err("value nesting is too deep".to_string());
    }
    match value {
        Value::Nil => Ok(JsonValue::Null),
        Value::Boolean(value) => Ok(JsonValue::Bool(*value)),
        Value::Integer(value) => Ok(JsonValue::Number((*value).into())),
        Value::Number(value) => serde_json::Number::from_f64(*value)
            .map(JsonValue::Number)
            .ok_or_else(|| "number is not finite".to_string()),
        Value::String(value) => value
            .to_str()
            .map(|text| JsonValue::String(text.to_string()))
            .map_err(|error| format!("string is not valid UTF-8: {error}")),
        Value::Table(table) => table_to_json(table, depth),
        _ => Err("value cannot be encoded as JSON".to_string()),
    }
}

fn table_to_json(table: &Table, depth: usize) -> Result<JsonValue, String> {
    let mut entries = Vec::new();
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, value) = pair.map_err(|error| format!("cannot read Lua table: {error}"))?;
        entries.push((key, lua_to_json(&value, depth + 1)?));
    }
    if entries.is_empty() {
        return Ok(JsonValue::Object(Map::new()));
    }
    let all_integers = entries.iter().all(|(key, _)| key.as_integer().is_some());
    if all_integers {
        let mut numbers: Vec<(i64, JsonValue)> = entries
            .into_iter()
            .map(|(key, value)| (key.as_integer().expect("checked"), value))
            .collect();
        numbers.sort_by_key(|(index, _)| *index);
        let mut array = Vec::with_capacity(numbers.len());
        for (expected, (index, value)) in (1i64..).zip(numbers) {
            if index != expected {
                return Err("Lua array keys must be consecutive integers starting at 1".to_string());
            }
            array.push(value);
        }
        return Ok(JsonValue::Array(array));
    }
    let all_strings = entries
        .iter()
        .all(|(key, _)| matches!(key, Value::String(_)));
    if !all_strings {
        return Err("Lua table mixes integer and string keys".to_string());
    }
    let mut object = Map::new();
    for (key, value) in entries {
        let Value::String(key) = key else {
            return Err("Lua table mixes integer and string keys".to_string());
        };
        let key = key.to_str().map_err(|error| error.to_string())?.to_string();
        object.insert(key, value);
    }
    Ok(JsonValue::Object(object))
}

fn json_to_lua(lua: &Lua, value: &JsonValue) -> mlua::Result<Value> {
    Ok(match value {
        JsonValue::Null => Value::Nil,
        JsonValue::Bool(value) => Value::Boolean(*value),
        JsonValue::Number(value) => {
            if let Some(integer) = value.as_i64() {
                Value::Integer(integer)
            } else {
                Value::Number(value.as_f64().unwrap_or(0.0))
            }
        }
        JsonValue::String(value) => Value::String(lua.create_string(value)?),
        JsonValue::Array(values) => {
            let table = lua.create_table()?;
            for (index, value) in values.iter().enumerate() {
                table.set((index + 1) as i64, json_to_lua(lua, value)?)?;
            }
            Value::Table(table)
        }
        JsonValue::Object(values) => {
            let table = lua.create_table()?;
            for (key, value) in values {
                table.set(key.as_str(), json_to_lua(lua, value)?)?;
            }
            Value::Table(table)
        }
    })
}

fn parse_panel(value: Value) -> Result<PanelContent, String> {
    match value {
        Value::String(text) => Ok(PanelContent {
            title: None,
            lines: vec![PanelLine::plain(
                text.to_str()
                    .map_err(|error| error.to_string())?
                    .to_string(),
            )],
            truncated: false,
            actions: Vec::new(),
        }),
        Value::Table(table) => {
            let title: Option<String> = table.get("title").ok().flatten();
            let actions = parse_panel_actions(table.get("actions").unwrap_or(Value::Nil))?;
            let lines_value: Value = table.get("lines").unwrap_or(Value::Nil);
            let lines = if matches!(lines_value, Value::Nil) {
                parse_panel_lines(Value::Table(table))?
            } else {
                parse_panel_lines(lines_value)?
            };
            Ok(PanelContent {
                title,
                lines: lines.0,
                truncated: lines.1,
                actions,
            })
        }
        _ => Err("panel function must return a string, an array, or { title, lines }".to_string()),
    }
}

fn parse_panel_actions(value: Value) -> Result<Vec<PanelAction>, String> {
    let Value::Table(table) = value else {
        return Ok(Vec::new());
    };
    let mut actions = Vec::new();
    for value in table.sequence_values::<Value>() {
        let value = value.map_err(|error| format!("cannot read panel action: {error}"))?;
        if actions.len() >= MAX_PANEL_ACTIONS {
            return Err(format!("panel has more than {MAX_PANEL_ACTIONS} actions"));
        }
        let Value::Table(action) = value else {
            return Err("panel actions must be tables".to_string());
        };
        let label: String = action
            .get("label")
            .map_err(|error| format!("panel action label: {error}"))?;
        let command: String = action
            .get("command")
            .map_err(|error| format!("panel action command: {error}"))?;
        if label.is_empty() || label.len() > MAX_PANEL_ACTION_LABEL_BYTES {
            return Err("panel action label is empty or too long".to_string());
        }
        if command.is_empty() || command.len() > MAX_PANEL_ACTION_COMMAND_BYTES {
            return Err("panel action command is empty or too long".to_string());
        }
        // The table command form (with args) is parsed by the v2 panel API.
        actions.push(PanelAction {
            label,
            command: PanelCommand {
                id: command,
                args: None,
            },
        });
    }
    Ok(actions)
}

fn parse_panel_lines(value: Value) -> Result<(Vec<PanelLine>, bool), String> {
    let mut lines = Vec::new();
    let mut truncated = false;
    match value {
        Value::String(text) => {
            lines.push(PanelLine::plain(
                text.to_str()
                    .map_err(|error| error.to_string())?
                    .to_string(),
            ));
        }
        Value::Table(table) => {
            for value in table.sequence_values::<Value>() {
                let value = value.map_err(|error| format!("cannot read panel line: {error}"))?;
                if lines.len() >= MAX_PANEL_LINES {
                    truncated = true;
                    break;
                }
                match value {
                    Value::String(text) => lines.push(PanelLine::plain(
                        text.to_str()
                            .map_err(|error| error.to_string())?
                            .to_string(),
                    )),
                    Value::Table(line) => lines.push(parse_panel_line(&line)?),
                    _ => return Err("panel lines must be strings or tables".to_string()),
                }
            }
        }
        _ => return Err("panel lines must be a string or an array".to_string()),
    }
    Ok((lines, truncated))
}

fn parse_panel_line(line: &Table) -> Result<PanelLine, String> {
    let text: String = line
        .get("text")
        .map_err(|error| format!("panel line text: {error}"))?;
    if text.len() > MAX_PANEL_LINE_BYTES {
        return Err("panel line is too long".to_string());
    }
    let command: Option<String> = line.get("command").ok().flatten();
    let style = line
        .get::<Option<String>>("style")
        .ok()
        .flatten()
        .and_then(|value| PanelLineStyle::parse(&value))
        .unwrap_or_default();
    let indent = line
        .get::<Option<i64>>("indent")
        .ok()
        .flatten()
        .unwrap_or(0)
        .clamp(0, 8) as u8;
    let spans = parse_panel_spans(&text, line.get::<Value>("spans").unwrap_or(Value::Nil))?;
    // The table command form (with args) is parsed by the v2 panel API.
    let command = command.map(|id| PanelCommand { id, args: None });
    Ok(PanelLine {
        text,
        command,
        style,
        indent,
        spans,
    })
}

fn parse_panel_spans(text: &str, value: Value) -> Result<Vec<PanelSpan>, String> {
    let Value::Table(table) = value else {
        return Ok(Vec::new());
    };
    let mut spans: Vec<PanelSpan> = Vec::new();
    let mut total = 0usize;
    for value in table.sequence_values::<Value>() {
        let value = value.map_err(|error| format!("cannot read panel span: {error}"))?;
        let Value::Table(span) = value else {
            return Err("panel spans must be tables".to_string());
        };
        let part: String = span
            .get("text")
            .map_err(|error| format!("panel span text: {error}"))?;
        if part.is_empty() {
            continue;
        }
        total = total.saturating_add(part.len());
        if total > MAX_PANEL_LINE_BYTES {
            return Err("panel line spans are too long".to_string());
        }
        if spans.len() >= MAX_PANEL_SPANS {
            return Err("panel line has too many spans".to_string());
        }
        let style = span
            .get::<Option<String>>("style")
            .ok()
            .flatten()
            .map(|value| PanelSpanStyle::parse(&value))
            .unwrap_or_default();
        let role = span
            .get::<Option<String>>("role")
            .ok()
            .flatten()
            .and_then(|role| sanitize_role(&role));
        spans.push(PanelSpan {
            text: part,
            style,
            role,
        });
    }
    if !spans.is_empty() {
        let joined: String = spans.iter().map(|span| span.text.as_str()).collect();
        if joined != text {
            log::warn!("plugin panel spans do not match the line text; rendering plain text");
            spans.clear();
        }
    }
    Ok(spans)
}

fn sanitize_role(role: &str) -> Option<String> {
    if role.is_empty() || role.len() > MAX_ROLE_BYTES {
        return None;
    }
    role.bytes()
        .all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || byte == b'.'
                || byte == b'_'
                || byte == b'-'
        })
        .then(|| role.to_string())
}

impl Drop for PluginRuntime {
    fn drop(&mut self) {
        let Some(name) = self.manifest.deactivate.clone() else {
            return;
        };
        let input = CallInput {
            plugin: PluginIdentity {
                id: self.manifest.id.clone(),
                version: self.manifest.version.clone(),
                data_dir: data_dir(&self.dir),
            },
            document: None,
            settings: crate::settings::defaults(&self.manifest.contributes.settings),
            allowed_roots: vec![self.dir.canonicalize().unwrap_or_else(|_| self.dir.clone())],
            command_depth: 0,
            context_kind: CallContextKind::default(),
        };
        let call = self.invoke(&name, None, input);
        if let Some(error) = call.error {
            log::warn!("plugin '{}' deactivate failed: {error}", self.manifest.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::PluginManifest;
    use crate::settings::defaults as settings_defaults;
    use std::time::{Duration, Instant};

    fn write_plugin(dir: &Path, main: &str) -> PluginManifest {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("plugin.json"),
            r#"{
                "id": "com.example.test",
                "name": "Test",
                "version": "1.0.0",
                "api_version": 2,
                "main": "main.lua",
                "permissions": ["filesystem"],
                "activate": "activate",
                "deactivate": "deactivate",
                "contributes": {
                    "commands": [{ "id": "test.run", "label": "Run", "entry": "run" }],
                    "status_bar": [{ "id": "test.status", "text": "ready" }],
                    "settings": [{ "key": "uppercase", "type": "boolean", "label": "Uppercase", "default": false }]
                }
            }"#,
        )
        .unwrap();
        fs::write(dir.join("main.lua"), main).unwrap();
        PluginManifest::load(dir).unwrap()
    }

    fn input(manifest: &PluginManifest, dir: &Path) -> CallInput {
        CallInput {
            plugin: PluginIdentity {
                id: manifest.id.clone(),
                version: manifest.version.clone(),
                data_dir: dir.join("data"),
            },
            document: Some(DocumentSnapshot {
                path: Some(dir.join("doc.txt")),
                text: "abc".to_string(),
                language: "Plain Text".to_string(),
                line: 1,
                column: 1,
                selection_start: 1,
                selection_end: 2,
                modified: false,
            }),
            settings: settings_defaults(&manifest.contributes.settings),
            allowed_roots: vec![dir.to_path_buf()],
            command_depth: 0,
            context_kind: CallContextKind::default(),
        }
    }

    /// Run a command entry and return the collected host actions, panicking on
    /// fail-fast errors.
    fn run_command(
        runtime: &mut PluginRuntime,
        entry: &str,
        command_id: &str,
        input: CallInput,
    ) -> RuntimeOutput {
        let call = runtime.call_command(entry, command_id, input);
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        call.output
    }

    /// Run a command entry and return the fail-fast error message.
    fn command_error(
        runtime: &mut PluginRuntime,
        entry: &str,
        command_id: &str,
        input: CallInput,
    ) -> String {
        let call = runtime.call_command(entry, command_id, input);
        match call.error {
            Some(error) => error,
            None => panic!("expected a fail-fast error, got {:?}", call.value),
        }
    }

    #[test]
    fn runs_a_command_and_collects_actions() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                activate = function(ctx) luban.log("info", "activated") end,
                deactivate = function(ctx) end,
                run = function(ctx)
                    luban.log("info", "running")
                    luban.notify("hello")
                    luban.document.insert("X")
                    luban.status.set("test.status", "done")
                end
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let call = runtime.call_command("run", "test.run", input(&manifest, dir.path()));
        assert!(call.is_ok(), "{:?}", call.error);
        assert_eq!(
            call.output.actions,
            vec![
                HostAction::Notify("hello".into()),
                HostAction::SetDocumentText {
                    text: "aXc".into(),
                    selection: Some((2, 2)),
                },
                HostAction::SetStatus {
                    id: "test.status".into(),
                    text: "done".into(),
                },
            ]
        );
    }

    #[test]
    fn sandbox_hides_dangerous_libraries() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                run = function(ctx)
                    assert(io == nil)
                    assert(os == nil)
                    assert(package == nil)
                    -- require stays available in its sandboxed form: it can
                    -- only load modules from the plugin directory (spec §8).
                    assert(type(require) == "function")
                    assert(load == nil)
                    assert(debug == nil)
                    luban.notify("safe")
                end
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let call = runtime.call_command("run", "test.run", input(&manifest, dir.path()));
        assert!(call.is_ok(), "{:?}", call.error);
        assert_eq!(call.output.actions, vec![HostAction::Notify("safe".into())]);
    }

    #[test]
    fn stops_infinite_loops() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return { run = function(ctx) while true do end end }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let call = runtime.call_command("run", "test.run", input(&manifest, dir.path()));
        assert!(!call.is_ok());
        let error = call.error.unwrap();
        assert!(error.contains("instruction limit"), "{error}");
    }

    #[test]
    fn reports_unsupported_documents_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"local function fail() error("boom") end
            return { run = function(ctx) fail() end }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let mut input = input(&manifest, dir.path());
        input.document = None;
        let call = runtime.call_command("run", "test.run", input);
        assert!(!call.is_ok());
        let error = call.error.unwrap();
        assert!(error.contains("boom"), "{error}");
        assert!(error.contains("stack traceback"), "{error}");
        assert!(error.contains("main.lua"), "{error}");
    }

    #[test]
    fn error_messages_are_capped_but_report_the_full_length() {
        assert_eq!(truncate_plugin_error("short"), "short");
        let long = "x".repeat(MAX_PLUGIN_ERROR_CHARS + 5);
        let capped = truncate_plugin_error(&long);
        assert!(capped.ends_with(&format!(
            "… [{} characters total]",
            MAX_PLUGIN_ERROR_CHARS + 5
        )));
    }

    #[test]
    fn filesystem_requires_permission_and_stays_in_roots() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                run = function(ctx)
                    luban.fs.write(ctx.plugin.data_dir .. "/state.txt", "hello")
                    assert(luban.fs.read(ctx.plugin.data_dir .. "/state.txt") == "hello")
                end,
                escape = function(ctx)
                    luban.fs.write("/etc/luban-plugin-escape-test.txt", "no")
                end,
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        run_command(
            &mut runtime,
            "run",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("data/state.txt")).unwrap(),
            "hello"
        );
        let error = command_error(
            &mut runtime,
            "escape",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert!(error.contains("outside the allowed roots"), "{error}");
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), PermissionSet::empty()).unwrap();
        let error = command_error(
            &mut runtime,
            "run",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert!(error.contains("filesystem permission"), "{error}");
    }

    #[test]
    fn json_round_trips_and_panels_parse() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                run = function(ctx)
                    assert(luban.version == 2, "API version")
                    local encoded = luban.json.encode({ a = 1, b = "x", c = { true } })
                    local decoded = luban.json.decode(encoded)
                    assert(decoded.a == 1 and decoded.b == "x" and decoded.c[1] == true)
                    luban.notify(encoded)
                end,
                list = function(ctx)
                    return {
                        title = "Demo",
                        lines = { "one", { text = "two", command = "test.run" } },
                        actions = {
                            { label = "Refresh", command = "test.run" },
                            { label = "Info", command = "test.info" },
                        },
                    }
                end
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let call = runtime.call_command("run", "test.run", input(&manifest, dir.path()));
        assert!(call.is_ok(), "{:?}", call.error);
        let panel = runtime.call_panel("list", input(&manifest, dir.path()));
        let panel = panel.value.unwrap();
        assert_eq!(panel.title.as_deref(), Some("Demo"));
        assert_eq!(panel.lines.len(), 2);
        assert_eq!(
            panel.lines[1].command,
            Some(PanelCommand {
                id: "test.run".to_string(),
                args: None,
            })
        );
        assert_eq!(panel.actions.len(), 2);
        assert_eq!(panel.actions[0].label, "Refresh");
        assert_eq!(
            panel.actions[1].command,
            PanelCommand {
                id: "test.info".to_string(),
                args: None,
            }
        );
    }

    #[test]
    fn activation_is_optional_and_errors_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return { activate = function(ctx) error("activate failed") end, run = function() end }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let call = runtime.activate(input(&manifest, dir.path()));
        assert!(!call.is_ok());
        assert!(call.error.unwrap().contains("activate failed"));
    }

    #[test]
    fn syntax_highlighting_returns_covering_spans() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                run = function(ctx)
                    local spans = assert(luban.syntax.highlight("fn main() {}\n", "rust"))
                    local joined, seen = "", {}
                    for _, span in ipairs(spans) do
                        assert(type(span.text) == "string")
                        assert(type(span.role) == "string")
                        joined = joined .. span.text
                        seen[span.role] = true
                    end
                    assert(joined == "fn main() {}\n", joined)
                    assert(seen.keyword, "keyword role missing")
                    assert(seen.primary, "primary role missing")
                    local plain = assert(luban.syntax.highlight("hello", "no-such-language"))
                    assert(#plain == 1 and plain[1].role == "primary")
                end,
                oversize = function(ctx) luban.syntax.highlight(string.rep("x", 70000), "rust") end,
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let call = runtime.call_command("run", "test.run", input(&manifest, dir.path()));
        assert!(call.is_ok(), "{:?}", call.error);
        let error = command_error(
            &mut runtime,
            "oversize",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert!(error.contains("larger than"), "{error}");
    }

    #[test]
    fn rich_panel_lines_parse_styles_spans_and_indent() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                list = function(ctx)
                    return { title = "Rich", lines = {
                        { text = "Title", style = "h1" },
                        { text = "code line", style = "code", spans = {
                            { text = "code", style = "bold code", role = "keyword" },
                            { text = " line", style = "italic" },
                        }},
                        { text = "", style = "rule" },
                        { text = "item", style = "list", indent = 2 },
                        { text = "mismatch", style = "muted", spans = {
                            { text = "other", style = "bold" },
                        }},
                    } }
                end
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let panel = runtime.call_panel("list", input(&manifest, dir.path()));
        let panel = panel.value.unwrap();
        assert_eq!(panel.lines[0].style, PanelLineStyle::H1);
        assert_eq!(panel.lines[1].style, PanelLineStyle::Code);
        assert_eq!(panel.lines[1].spans.len(), 2);
        assert!(panel.lines[1].spans[0].style.bold());
        assert!(panel.lines[1].spans[0].style.code());
        assert_eq!(panel.lines[1].spans[0].role.as_deref(), Some("keyword"));
        assert!(panel.lines[1].spans[1].style.italic());
        assert_eq!(panel.lines[2].style, PanelLineStyle::Rule);
        assert_eq!(panel.lines[3].indent, 2);
        // A span list that does not reconstruct the line falls back to text.
        assert_eq!(panel.lines[4].spans.len(), 0);
        assert_eq!(panel.lines[4].text, "mismatch");
    }

    #[test]
    fn panel_lines_reject_excessive_spans() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                list = function(ctx)
                    local spans = {}
                    for i = 1, 600 do spans[i] = { text = "x" } end
                    return { lines = { { text = string.rep("x", 600), spans = spans } } }
                end
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let panel = runtime.call_panel("list", input(&manifest, dir.path()));
        assert!(panel.error.is_some());
    }

    #[test]
    fn opening_a_panel_is_reported_as_a_host_action() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                run = function(ctx)
                    luban.panel.open("test.panel")
                end
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let call = runtime.call_command("run", "test.run", input(&manifest, dir.path()));
        assert!(call.is_ok(), "{:?}", call.error);
        assert_eq!(
            call.output.actions,
            vec![HostAction::OpenPanel {
                id: "test.panel".into()
            }]
        );
    }

    #[test]
    fn context_kind_defaults_to_user() {
        assert_eq!(CallContextKind::default(), CallContextKind::User);
    }

    #[test]
    fn notify_drops_messages_over_the_per_dispatch_limit() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                run = function(ctx)
                    for i = 1, 10 do luban.notify("n" .. i) end
                end
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let output = run_command(
            &mut runtime,
            "run",
            "test.run",
            input(&manifest, dir.path()),
        );
        let notifies: Vec<&str> = output
            .actions
            .iter()
            .map(|action| match action {
                HostAction::Notify(text) => text.as_str(),
                _ => panic!("unexpected action {action:?}"),
            })
            .collect();
        assert_eq!(notifies.len(), MAX_NOTIFY_PER_CALL);
        assert_eq!(notifies.first(), Some(&"n1"));
        assert_eq!(notifies.last(), Some(&"n8"));
    }

    #[test]
    fn notify_truncates_long_messages_at_a_char_boundary() {
        let dir = tempfile::tempdir().unwrap();
        // 400 × "中" = 1200 bytes; the 1024-byte limit falls mid-character, so
        // truncation must back off to the last boundary (1023 bytes, 341 chars).
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                run = function(ctx) luban.notify(string.rep("中", 400)) end
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let output = run_command(
            &mut runtime,
            "run",
            "test.run",
            input(&manifest, dir.path()),
        );
        let HostAction::Notify(text) = &output.actions[0] else {
            panic!("expected a notify action, got {:?}", output.actions);
        };
        assert_eq!(text.len(), 1023);
        assert_eq!(text.chars().count(), 341);
    }

    #[test]
    fn json_errors_are_fail_fast() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                encode_bad = function(ctx) luban.json.encode(function() end) end,
                decode_bad = function(ctx) luban.json.decode("oops") end,
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let error = command_error(
            &mut runtime,
            "encode_bad",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert!(error.contains("cannot be encoded"), "{error}");
        let error = command_error(
            &mut runtime,
            "decode_bad",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert!(error.contains("expected"), "{error}");
    }

    #[test]
    fn document_api_is_namespaced_and_fail_fast() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                probe = function(ctx) luban.notify(tostring(luban.document.get() ~= nil)) end,
                write = function(ctx) luban.document.set_text("x", 0, 0) end,
                append = function(ctx) luban.document.insert("x") end,
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();

        let output = run_command(
            &mut runtime,
            "probe",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert_eq!(output.actions, vec![HostAction::Notify("true".into())]);

        let mut without_document = input(&manifest, dir.path());
        without_document.document = None;
        let output = run_command(&mut runtime, "probe", "test.run", without_document);
        assert_eq!(output.actions, vec![HostAction::Notify("false".into())]);

        // set_text / insert raise instead of returning (nil, err).
        let mut without_document = input(&manifest, dir.path());
        without_document.document = None;
        let error = command_error(&mut runtime, "write", "test.run", without_document);
        assert!(error.contains("unsupported document"), "{error}");

        let mut without_document = input(&manifest, dir.path());
        without_document.document = None;
        let error = command_error(&mut runtime, "append", "test.run", without_document);
        assert!(error.contains("unsupported document"), "{error}");
    }

    #[test]
    fn document_set_text_requires_both_selection_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                run = function(ctx) luban.document.set_text("x", 1) end
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let error = command_error(
            &mut runtime,
            "run",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert!(error.contains("set_text expects"), "{error}");
    }

    #[test]
    fn document_mutations_are_read_only_in_edit_context() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                write = function(ctx) luban.document.set_text("x", 1, 1) end,
                append = function(ctx) luban.document.insert("x") end,
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let mut edit = input(&manifest, dir.path());
        edit.context_kind = CallContextKind::Edit;
        let error = command_error(&mut runtime, "write", "test.run", edit);
        assert!(error.contains("read-only"), "{error}");
        let mut edit = input(&manifest, dir.path());
        edit.context_kind = CallContextKind::Edit;
        let error = command_error(&mut runtime, "append", "test.run", edit);
        assert!(error.contains("read-only"), "{error}");
    }

    #[test]
    fn document_set_cursor_is_one_based() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                run = function(ctx) luban.document.set_cursor(3, 1) end,
                zero_line = function(ctx) luban.document.set_cursor(0, 1) end,
                zero_col = function(ctx) luban.document.set_cursor(1, 0) end,
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let output = run_command(
            &mut runtime,
            "run",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert_eq!(
            output.actions,
            vec![HostAction::SetCursor { line: 3, col: 1 }]
        );
        let error = command_error(
            &mut runtime,
            "zero_line",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert!(error.contains("1-based"), "{error}");
        let error = command_error(
            &mut runtime,
            "zero_col",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert!(error.contains("1-based"), "{error}");
    }

    #[test]
    fn settings_get_returns_nil_for_undeclared_keys() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                probe = function(ctx)
                    luban.notify(tostring(luban.settings.get("uppercase")))
                    luban.notify(tostring(luban.settings.get("missing")))
                end
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let output = run_command(
            &mut runtime,
            "probe",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert_eq!(
            output.actions,
            vec![
                HostAction::Notify("false".into()),
                HostAction::Notify("nil".into()),
            ]
        );
    }

    #[test]
    fn settings_set_validates_against_the_declared_schema() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                ok = function(ctx) luban.settings.set("uppercase", true) end,
                undeclared = function(ctx) luban.settings.set("missing", 1) end,
                invalid = function(ctx) luban.settings.set("uppercase", "yes") end,
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let output = run_command(&mut runtime, "ok", "test.run", input(&manifest, dir.path()));
        assert_eq!(
            output.actions,
            vec![HostAction::SetSetting {
                key: "uppercase".into(),
                value: serde_json::Value::Bool(true),
            }]
        );
        let error = command_error(
            &mut runtime,
            "undeclared",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert!(
            error.contains("setting 'missing' is not declared"),
            "{error}"
        );
        let error = command_error(
            &mut runtime,
            "invalid",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert!(
            error.contains("invalid value for setting 'uppercase'"),
            "{error}"
        );
    }

    #[test]
    fn status_set_rejects_undeclared_ids() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                run = function(ctx) luban.status.set("other.status", "x") end
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let error = command_error(
            &mut runtime,
            "run",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert!(
            error.contains("status item 'other.status' is not declared"),
            "{error}"
        );
    }

    #[test]
    fn panel_open_and_refresh_are_fail_fast() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                refresh = function(ctx) luban.panel.refresh("test.panel") end,
                open_empty = function(ctx) luban.panel.open("") end,
                open_long = function(ctx) luban.panel.open(string.rep("x", 129)) end,
                refresh_empty = function(ctx) luban.panel.refresh("") end,
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let output = run_command(
            &mut runtime,
            "refresh",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert_eq!(
            output.actions,
            vec![HostAction::RefreshPanel {
                id: "test.panel".into()
            }]
        );
        for entry in ["open_empty", "open_long", "refresh_empty"] {
            let error = command_error(
                &mut runtime,
                entry,
                "test.run",
                input(&manifest, dir.path()),
            );
            assert!(error.contains("invalid panel id"), "{entry}: {error}");
        }
    }

    #[test]
    fn command_run_validates_id_and_depth() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                run = function(ctx) luban.command.run("other.plugin.cmd", { n = 1 }) end,
                empty = function(ctx) luban.command.run("") end,
                long = function(ctx) luban.command.run(string.rep("x", 129)) end,
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let output = run_command(
            &mut runtime,
            "run",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert_eq!(
            output.actions,
            vec![HostAction::RunCommand {
                id: "other.plugin.cmd".into(),
                args: Some(serde_json::json!({ "n": 1 })),
                depth: 1,
            }]
        );
        for entry in ["empty", "long"] {
            let error = command_error(
                &mut runtime,
                entry,
                "test.run",
                input(&manifest, dir.path()),
            );
            assert!(error.contains("invalid command id"), "{entry}: {error}");
        }
        // `depth` is the caller's command_depth + 1; chains beyond
        // MAX_COMMAND_DEPTH raise.
        let mut deep = input(&manifest, dir.path());
        deep.command_depth = MAX_COMMAND_DEPTH;
        let error = command_error(&mut runtime, "run", "test.run", deep);
        assert!(
            error.contains(&format!("maximum depth of {MAX_COMMAND_DEPTH}")),
            "{error}"
        );
    }

    #[test]
    fn data_dir_returns_the_plugin_data_path() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                run = function(ctx) luban.notify(luban.data_dir()) end
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let output = run_command(
            &mut runtime,
            "run",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert_eq!(
            output.actions,
            vec![HostAction::Notify(
                dir.path().join("data").to_string_lossy().into()
            )]
        );
    }

    #[test]
    fn json_encode_rejects_cyclic_tables() {
        let dir = tempfile::tempdir().unwrap();
        // `Table::pairs` runs no VM instructions, so without a depth guard a
        // self-referential table would recurse on the Rust stack until the
        // host process aborts.
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                run = function(ctx)
                    local t = {}
                    t.self = t
                    luban.json.encode(t)
                end
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let error = command_error(
            &mut runtime,
            "run",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert!(error.contains("nesting is too deep"), "{error}");
    }

    #[test]
    fn settings_set_is_visible_to_get_in_the_same_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                probe = function(ctx)
                    luban.settings.set("uppercase", true)
                    luban.notify(tostring(luban.settings.get("uppercase")))
                end
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let output = run_command(
            &mut runtime,
            "probe",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert_eq!(output.actions.len(), 2);
        assert_eq!(
            output.actions[0],
            HostAction::SetSetting {
                key: "uppercase".into(),
                value: serde_json::Value::Bool(true),
            }
        );
        // Read-after-write sees the new value before the host applies the
        // action.
        assert_eq!(output.actions[1], HostAction::Notify("true".into()));
    }

    #[test]
    fn document_set_text_reports_selection_payload() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                keep = function(ctx) luban.document.set_text("hello") end,
                select = function(ctx) luban.document.set_text("hello", 2, 4) end,
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        // Without offsets the current selection (input: 1..2) is kept.
        let output = run_command(
            &mut runtime,
            "keep",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert_eq!(
            output.actions,
            vec![HostAction::SetDocumentText {
                text: "hello".into(),
                selection: Some((1, 2)),
            }]
        );
        let output = run_command(
            &mut runtime,
            "select",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert_eq!(
            output.actions,
            vec![HostAction::SetDocumentText {
                text: "hello".into(),
                selection: Some((2, 4)),
            }]
        );
    }

    /// Like `write_plugin` but declares `events` in the manifest.
    fn write_events_plugin(dir: &Path, events: &[&str], main: &str) -> PluginManifest {
        fs::create_dir_all(dir).unwrap();
        let events = events
            .iter()
            .map(|event| format!(r#""{event}""#))
            .collect::<Vec<_>>()
            .join(", ");
        fs::write(
            dir.join("plugin.json"),
            format!(
                r#"{{
                    "id": "com.example.test",
                    "name": "Test",
                    "version": "1.0.0",
                    "api_version": 2,
                    "main": "main.lua",
                    "permissions": ["filesystem"],
                    "events": [{events}],
                    "activate": "activate",
                    "deactivate": "deactivate",
                    "contributes": {{
                        "commands": [{{ "id": "test.run", "label": "Run", "entry": "run" }}],
                        "status_bar": [{{ "id": "test.status", "text": "ready" }}],
                        "settings": [{{ "key": "uppercase", "type": "boolean", "label": "Uppercase", "default": false }}]
                    }}
                }}"#
            ),
        )
        .unwrap();
        fs::write(dir.join("main.lua"), main).unwrap();
        PluginManifest::load(dir).unwrap()
    }

    #[test]
    fn events_register_requires_declared_event() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_events_plugin(
            dir.path(),
            &[],
            r#"return { activate = function(ctx) end }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let error = runtime.register_event("on_edit", "activate").unwrap_err();
        assert!(error.contains("not declared"), "{error}");

        // Declared event but no such function in the plugin table: the error
        // names the missing handler.
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_events_plugin(
            dir.path(),
            &["on_document_save"],
            r#"return { activate = function(ctx) end }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let error = runtime
            .register_event("on_document_save", "no_such_handler")
            .unwrap_err();
        assert!(error.contains("no_such_handler"), "{error}");
    }

    #[test]
    fn events_register_and_dispatch_order() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_events_plugin(
            dir.path(),
            &["on_document_save"],
            r#"local M = {}
            M.first = function(ctx) luban.notify("a") end
            M.second = function(ctx) luban.notify("b") end
            function M.activate(ctx)
                luban.events.on("on_document_save", M.first)
                luban.events.on("on_document_save", M.second)
            end
            return M"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let call = runtime.activate(input(&manifest, dir.path()));
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        let call = runtime.dispatch_event("on_document_save", input(&manifest, dir.path()));
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        // Handlers run in registration order and their actions accumulate.
        assert_eq!(
            call.output.actions,
            vec![
                HostAction::Notify("a".into()),
                HostAction::Notify("b".into()),
            ]
        );
    }

    #[test]
    fn events_dispatch_contains_handler_failure() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_events_plugin(
            dir.path(),
            &["on_document_save"],
            r#"local M = {}
            M.fail = function(ctx) error("boom") end
            M.good = function(ctx) luban.notify("survived") end
            function M.activate(ctx)
                luban.events.on("on_document_save", M.fail)
                luban.events.on("on_document_save", M.good)
            end
            return M"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let call = runtime.activate(input(&manifest, dir.path()));
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        let call = runtime.dispatch_event("on_document_save", input(&manifest, dir.path()));
        assert!(!call.is_ok());
        let error = call.error.unwrap();
        assert!(error.contains("boom"), "{error}");
        assert!(error.contains("on_document_save"), "{error}");
        // The handler after the failing one still ran.
        assert_eq!(
            call.output.actions,
            vec![HostAction::Notify("survived".into())]
        );
        assert_eq!(runtime.consecutive_failures(), 1);
    }

    #[test]
    fn events_success_clears_failure_count() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_events_plugin(
            dir.path(),
            &["on_document_save"],
            r#"local M = {}
            M.fail = function(ctx) error("boom") end
            function M.activate(ctx)
                luban.events.on("on_document_save", M.fail)
            end
            function M.run(ctx) luban.notify("ok") end
            return M"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let call = runtime.activate(input(&manifest, dir.path()));
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        let call = runtime.dispatch_event("on_document_save", input(&manifest, dir.path()));
        assert!(!call.is_ok());
        assert_eq!(runtime.consecutive_failures(), 1);
        run_command(
            &mut runtime,
            "run",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert_eq!(runtime.consecutive_failures(), 0);
    }

    #[test]
    fn events_on_undeclared_raises() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_events_plugin(
            dir.path(),
            &[],
            r#"return {
                run = function(ctx) luban.events.on("on_edit", function() end) end
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let error = command_error(
            &mut runtime,
            "run",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert!(error.contains("not declared in the manifest"), "{error}");
    }

    #[test]
    fn events_on_over_limit_raises() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_events_plugin(
            dir.path(),
            &["on_document_save"],
            r#"return {
                run = function(ctx)
                    for i = 1, 33 do
                        luban.events.on("on_document_save", function() end)
                    end
                end
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let error = command_error(
            &mut runtime,
            "run",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert!(
            error.contains(&format!(
                "too many event handlers (limit {MAX_EVENT_HANDLERS})"
            )),
            "{error}"
        );
    }

    #[test]
    fn should_quarantine_after_ten_failures() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_events_plugin(
            dir.path(),
            &["on_document_save"],
            r#"local M = {}
            M.fail = function(ctx) error("boom") end
            function M.activate(ctx)
                luban.events.on("on_document_save", M.fail)
            end
            return M"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let call = runtime.activate(input(&manifest, dir.path()));
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        for _ in 0..9 {
            let call = runtime.dispatch_event("on_document_save", input(&manifest, dir.path()));
            assert!(!call.is_ok());
        }
        assert_eq!(runtime.consecutive_failures(), 9);
        assert!(!runtime.should_quarantine());
        let call = runtime.dispatch_event("on_document_save", input(&manifest, dir.path()));
        assert!(!call.is_ok());
        assert_eq!(runtime.consecutive_failures(), 10);
        assert!(runtime.should_quarantine());
    }

    #[test]
    fn dispatch_event_with_no_document_nil_payload() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_events_plugin(
            dir.path(),
            &["on_active_document_changed"],
            r#"local M = {}
            M.changed = function(ctx, payload)
                if payload ~= nil then error("payload must be nil") end
                if ctx.document == nil then
                    luban.notify("nil")
                else
                    luban.notify("doc")
                end
            end
            function M.activate(ctx)
                luban.events.on("on_active_document_changed", M.changed)
            end
            return M"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let call = runtime.activate(input(&manifest, dir.path()));
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        let mut without_document = input(&manifest, dir.path());
        without_document.document = None;
        let call = runtime.dispatch_event("on_active_document_changed", without_document);
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        assert_eq!(call.output.actions, vec![HostAction::Notify("nil".into())]);
        assert_eq!(runtime.consecutive_failures(), 0);
    }

    #[test]
    fn events_dispatch_counts_one_failure_per_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_events_plugin(
            dir.path(),
            &["on_document_save"],
            r#"local M = {}
            M.fail_a = function(ctx) error("boom a") end
            M.fail_b = function(ctx) error("boom b") end
            function M.activate(ctx)
                luban.events.on("on_document_save", M.fail_a)
                luban.events.on("on_document_save", M.fail_b)
            end
            return M"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let call = runtime.activate(input(&manifest, dir.path()));
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        let call = runtime.dispatch_event("on_document_save", input(&manifest, dir.path()));
        assert!(!call.is_ok());
        // Only the first failure is reported.
        let error = call.error.unwrap();
        assert!(error.contains("boom a"), "{error}");
        assert!(!error.contains("boom b"), "{error}");
        // Two failed handlers inside one dispatch count as ONE failure.
        assert_eq!(runtime.consecutive_failures(), 1);
    }

    #[test]
    fn events_off_removes_handlers() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_events_plugin(
            dir.path(),
            &["on_document_save"],
            r#"local M = {}
            M.hit = function(ctx) luban.notify("hit") end
            function M.activate(ctx)
                luban.events.on("on_document_save", M.hit)
            end
            function M.off(ctx) luban.events.off("on_document_save") end
            return M"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let call = runtime.activate(input(&manifest, dir.path()));
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        let call = runtime.dispatch_event("on_document_save", input(&manifest, dir.path()));
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        assert_eq!(call.output.actions, vec![HostAction::Notify("hit".into())]);
        run_command(
            &mut runtime,
            "off",
            "test.run",
            input(&manifest, dir.path()),
        );
        let call = runtime.dispatch_event("on_document_save", input(&manifest, dir.path()));
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        assert!(call.output.actions.is_empty());
        assert_eq!(runtime.consecutive_failures(), 0);
    }

    #[test]
    fn events_registered_mid_dispatch_fire_on_the_next_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_events_plugin(
            dir.path(),
            &["on_document_save"],
            r#"local M = {}
            M.first = function(ctx)
                luban.events.on("on_document_save", M.late)
                luban.notify("first")
            end
            M.late = function(ctx) luban.notify("late") end
            function M.activate(ctx)
                luban.events.on("on_document_save", M.first)
            end
            return M"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let call = runtime.activate(input(&manifest, dir.path()));
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        // The handler registered during dispatch #1 does not fire in it: the
        // handler list is snapshotted before the dispatch starts.
        let call = runtime.dispatch_event("on_document_save", input(&manifest, dir.path()));
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        assert_eq!(
            call.output.actions,
            vec![HostAction::Notify("first".into())]
        );
        // The registration survives into the next dispatch: `late` now runs
        // after `first` (which fires again and re-registers it each time).
        let call = runtime.dispatch_event("on_document_save", input(&manifest, dir.path()));
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        assert_eq!(
            call.output.actions,
            vec![
                HostAction::Notify("first".into()),
                HostAction::Notify("late".into()),
            ]
        );
    }

    #[test]
    fn events_register_helper_dispatches() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_events_plugin(
            dir.path(),
            &["on_document_save"],
            r#"local M = {}
            function M.activate(ctx) end
            function M.bump(ctx) luban.notify("helper") end
            return M"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        runtime.register_event("on_document_save", "bump").unwrap();
        let call = runtime.dispatch_event("on_document_save", input(&manifest, dir.path()));
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        assert_eq!(
            call.output.actions,
            vec![HostAction::Notify("helper".into())]
        );
    }

    #[test]
    fn events_dispatch_with_no_handlers_resets_failures() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_events_plugin(
            dir.path(),
            &["on_document_save", "on_document_open"],
            r#"local M = {}
            M.boom = function(ctx) error("boom") end
            function M.activate(ctx)
                luban.events.on("on_document_save", M.boom)
            end
            return M"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let call = runtime.activate(input(&manifest, dir.path()));
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        let call = runtime.dispatch_event("on_document_save", input(&manifest, dir.path()));
        assert!(!call.is_ok());
        assert_eq!(runtime.consecutive_failures(), 1);
        // A dispatch with no registered handlers is a successful (no-op)
        // dispatch and resets the consecutive-failure counter like any other
        // success.
        let call = runtime.dispatch_event("on_document_open", input(&manifest, dir.path()));
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        assert!(call.output.actions.is_empty());
        assert_eq!(runtime.consecutive_failures(), 0);
    }

    // ---- timers (spec §9) ----
    //
    // Due-ness is checked with `Instant` arithmetic instead of sleeps so the
    // tests stay fast and CI-stable: `due_timer_ids` is a pure function of the
    // `now` argument.

    #[test]
    fn timers_clamp_below_minimum() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return { tick = function(ctx) luban.notify("tick") end }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let registered = Instant::now();
        // 1ms is below MIN_TIMER_MS and is clamped up to 50ms.
        runtime.register_timer("t", 1, "tick", true).unwrap();
        // Not due 49ms after registration...
        assert!(runtime
            .due_timer_ids(registered + Duration::from_millis(49))
            .is_empty());
        // ...but due well after the clamped interval.
        assert_eq!(
            runtime.due_timer_ids(registered + Duration::from_millis(1000)),
            vec!["t".to_string()]
        );
    }

    #[test]
    fn timers_fire_once_for_after() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"local M = {}
            M.count = 0
            function M.tick(ctx, payload)
                if payload ~= nil then error("payload must be nil") end
                M.count = M.count + 1
                luban.notify("tick " .. M.count)
            end
            return M"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        runtime.register_timer("t1", 50, "tick", true).unwrap();
        assert_eq!(
            runtime.due_timer_ids(Instant::now() + Duration::from_millis(1000)),
            vec!["t1".to_string()]
        );
        let call = runtime
            .fire_timer("t1", input(&manifest, dir.path()))
            .unwrap();
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        // The callback ran exactly once, with the dispatch context and a nil
        // payload, and its module counter incremented.
        assert_eq!(
            call.output.actions,
            vec![HostAction::Notify("tick 1".into())]
        );
        // One-shot: the entry is gone after firing.
        assert!(runtime
            .due_timer_ids(Instant::now() + Duration::from_millis(1000))
            .is_empty());
        let error = runtime
            .fire_timer("t1", input(&manifest, dir.path()))
            .unwrap_err();
        assert!(error.contains("timer 't1' is not active"), "{error}");
    }

    #[test]
    fn timers_repeat_for_every() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"local M = {}
            M.count = 0
            function M.tick(ctx)
                M.count = M.count + 1
                luban.notify("tick " .. M.count)
            end
            return M"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        runtime.register_timer("t", 1000, "tick", false).unwrap();
        let call = runtime
            .fire_timer("t", input(&manifest, dir.path()))
            .unwrap();
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        assert_eq!(
            call.output.actions,
            vec![HostAction::Notify("tick 1".into())]
        );
        // Repeating: the entry was reinserted before dispatch with
        // next_due = now + interval, so it is not due right after firing...
        assert!(runtime.due_timer_ids(Instant::now()).is_empty());
        // ...but due again one interval later.
        assert_eq!(
            runtime.due_timer_ids(Instant::now() + Duration::from_millis(1000)),
            vec!["t".to_string()]
        );
        // And it keeps firing.
        let call = runtime
            .fire_timer("t", input(&manifest, dir.path()))
            .unwrap();
        assert_eq!(
            call.output.actions,
            vec![HostAction::Notify("tick 2".into())]
        );
    }

    #[test]
    fn timers_unknown_id_errors() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(dir.path(), r#"return { tick = function(ctx) end }"#);
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let error = runtime
            .fire_timer("ghost", input(&manifest, dir.path()))
            .unwrap_err();
        assert!(error.contains("timer 'ghost' is not active"), "{error}");
    }

    #[test]
    fn timers_cap_at_sixteen() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                fill = function(ctx)
                    for i = 1, 16 do
                        luban.timer.every("t" .. i, 100, function() end)
                    end
                end,
                extra = function(ctx) luban.timer.every("t17", 100, function() end) end,
                overwrite = function(ctx) luban.timer.every("t1", 200, function() end) end,
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        run_command(
            &mut runtime,
            "fill",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert_eq!(
            runtime
                .due_timer_ids(Instant::now() + Duration::from_millis(1000))
                .len(),
            MAX_TIMERS
        );
        // A 17th distinct id raises at the cap.
        let error = command_error(
            &mut runtime,
            "extra",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert!(
            error.contains(&format!("too many active timers (limit {MAX_TIMERS})")),
            "{error}"
        );
        // Overwriting an existing id still works at the cap.
        run_command(
            &mut runtime,
            "overwrite",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert_eq!(
            runtime
                .due_timer_ids(Instant::now() + Duration::from_millis(1000))
                .len(),
            MAX_TIMERS
        );
    }

    #[test]
    fn timers_overwrite_same_id() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                first = function(ctx) luban.notify("first") end,
                second = function(ctx) luban.notify("second") end,
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        runtime.register_timer("t", 50, "first", true).unwrap();
        runtime.register_timer("t", 50, "second", true).unwrap();
        // The duplicate registration replaced the first entry: only the second
        // callback runs.
        let call = runtime
            .fire_timer("t", input(&manifest, dir.path()))
            .unwrap();
        assert_eq!(
            call.output.actions,
            vec![HostAction::Notify("second".into())]
        );
        // And there is exactly one (one-shot) entry: no second fire.
        let error = runtime
            .fire_timer("t", input(&manifest, dir.path()))
            .unwrap_err();
        assert!(error.contains("timer 't' is not active"), "{error}");
    }

    #[test]
    fn timers_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"local M = {}
            M.tick = function(ctx)
                luban.notify("tick")
                luban.timer.cancel("self")
            end
            M.quiet = function(ctx) end
            function M.cancel_other(ctx) luban.timer.cancel("other") end
            function M.cancel_absent(ctx) luban.timer.cancel("ghost") end
            return M"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        // Cancel of an absent id is a no-op.
        run_command(
            &mut runtime,
            "cancel_absent",
            "test.run",
            input(&manifest, dir.path()),
        );
        // A repeating timer whose callback cancels itself mid-dispatch: the
        // entry was reinserted before dispatch and the handler's cancel then
        // removed it, so it never fires again.
        runtime.register_timer("self", 1000, "tick", false).unwrap();
        let call = runtime
            .fire_timer("self", input(&manifest, dir.path()))
            .unwrap();
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        assert_eq!(call.output.actions, vec![HostAction::Notify("tick".into())]);
        assert!(runtime
            .due_timer_ids(Instant::now() + Duration::from_millis(1000))
            .is_empty());
        let error = runtime
            .fire_timer("self", input(&manifest, dir.path()))
            .unwrap_err();
        assert!(error.contains("timer 'self' is not active"), "{error}");
        // A cancel from another dispatch removes the timer; firing it errors.
        runtime
            .register_timer("other", 1000, "quiet", true)
            .unwrap();
        run_command(
            &mut runtime,
            "cancel_other",
            "test.run",
            input(&manifest, dir.path()),
        );
        let error = runtime
            .fire_timer("other", input(&manifest, dir.path()))
            .unwrap_err();
        assert!(error.contains("timer 'other' is not active"), "{error}");
    }

    #[test]
    fn timers_failure_counts_toward_quarantine() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return { boom = function(ctx) error("boom") end }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        runtime.register_timer("t", 50, "boom", true).unwrap();
        // fire_timer's Err variant is reserved for unknown ids; a failing
        // callback is contained in RuntimeCall::error.
        let call = runtime
            .fire_timer("t", input(&manifest, dir.path()))
            .unwrap();
        assert!(!call.is_ok());
        let error = call.error.unwrap();
        assert!(error.contains("boom"), "{error}");
        // The failure names the timer id as the failing entry.
        assert!(error.contains("failed in 't'"), "{error}");
        assert_eq!(runtime.consecutive_failures(), 1);
    }

    #[test]
    fn timers_receive_context_and_actions() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                tick = function(ctx, payload)
                    if payload ~= nil then error("payload must be nil") end
                    luban.notify("text:" .. ctx.document.text)
                end,
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        runtime.register_timer("t", 50, "tick", true).unwrap();
        // fire_timer forces the Event context regardless of what the host
        // passes (spec §6), so the default User input below dispatches fine.
        let call = runtime
            .fire_timer("t", input(&manifest, dir.path()))
            .unwrap();
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        // The callback sees the dispatch context's document and its notify
        // surfaces in the RuntimeCall output.
        assert_eq!(
            call.output.actions,
            vec![HostAction::Notify("text:abc".into())]
        );
    }

    #[test]
    fn timers_validate_id_and_interval() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                empty_id = function(ctx) luban.timer.after("", 100, function() end) end,
                long_id = function(ctx)
                    luban.timer.after(string.rep("x", 129), 100, function() end)
                end,
                negative = function(ctx) luban.timer.after("t", -1, function() end) end,
                zero = function(ctx) luban.timer.after("t0", 0, function() end) end,
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        for entry in ["empty_id", "long_id"] {
            let error = command_error(
                &mut runtime,
                entry,
                "test.run",
                input(&manifest, dir.path()),
            );
            assert!(error.contains("invalid timer id"), "{entry}: {error}");
        }
        let error = command_error(
            &mut runtime,
            "negative",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert!(
            error.contains("timer interval must be non-negative"),
            "{error}"
        );
        // ms == 0 is not an error: it is clamped up to MIN_TIMER_MS.
        let before = Instant::now();
        run_command(
            &mut runtime,
            "zero",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert!(runtime
            .due_timer_ids(before + Duration::from_millis(49))
            .is_empty());
        assert_eq!(
            runtime.due_timer_ids(before + Duration::from_millis(1000)),
            vec!["t0".to_string()]
        );
    }

    #[test]
    fn timers_success_resets_quarantine_counter() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return { tick = function(ctx) luban.notify("tick") end }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        // Seed one failure through the host-side bookkeeping the pump uses.
        runtime.record_failure();
        assert_eq!(runtime.consecutive_failures(), 1);
        // A successful timer fire counts as a successful dispatch and resets
        // the consecutive-failure counter like any other success.
        runtime.register_timer("t", 50, "tick", true).unwrap();
        let call = runtime
            .fire_timer("t", input(&manifest, dir.path()))
            .unwrap();
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        assert_eq!(runtime.consecutive_failures(), 0);
    }

    #[test]
    fn register_timer_rejects_unknown_handler_and_invalid_id() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(dir.path(), r#"return { tick = function(ctx) end }"#);
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        // A missing handler errors naming the handler.
        let error = runtime
            .register_timer("t", 50, "no_such_handler", true)
            .unwrap_err();
        assert!(error.contains("no_such_handler"), "{error}");
        // Invalid ids are rejected on the Rust path too, matching the Lua API.
        let error = runtime.register_timer("", 50, "tick", true).unwrap_err();
        assert!(error.contains("invalid timer id"), "{error}");
        let error = runtime
            .register_timer(&"x".repeat(MAX_ID_BYTES + 1), 50, "tick", true)
            .unwrap_err();
        assert!(error.contains("invalid timer id"), "{error}");
        // Nothing was registered along the way.
        assert!(runtime
            .due_timer_ids(Instant::now() + Duration::from_millis(1000))
            .is_empty());
    }

    // ---- sandboxed in-plugin require (spec §8) ----
    //
    // Modules are plain files under the plugin tempdir next to main.lua;
    // `write_plugin` only writes plugin.json and main.lua, so extra module
    // files are added with `fs::write` before `PluginRuntime::new`.

    #[test]
    fn require_loads_local_module() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.lua"), "return { answer = 42 }").unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                run = function(ctx) luban.notify(tostring(require('lib').answer)) end
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let output = run_command(
            &mut runtime,
            "run",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert_eq!(output.actions, vec![HostAction::Notify("42".into())]);
    }

    #[test]
    fn require_nested_modules() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.lua"), "return { answer = 42 }").unwrap();
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        // `sub.mod` maps to `sub/mod.lua`; the module itself requires another
        // module (nested require through the same sandboxed loader).
        fs::write(
            dir.path().join("sub/mod.lua"),
            "return require('lib').answer",
        )
        .unwrap();
        // A bare single-segment name maps to `<name>.lua`, independently of a
        // `sub/` directory also existing.
        fs::write(dir.path().join("sub.lua"), "return 'bare'").unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                run = function(ctx)
                    luban.notify(tostring(require('sub.mod')))
                    luban.notify(require('sub'))
                    -- `lib` was already loaded by `sub.mod`: the cache serves it.
                    luban.notify(tostring(require('lib').answer))
                end
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let output = run_command(
            &mut runtime,
            "run",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert_eq!(
            output.actions,
            vec![
                HostAction::Notify("42".into()),
                HostAction::Notify("bare".into()),
                HostAction::Notify("42".into()),
            ]
        );
    }

    #[test]
    fn require_caches_per_vm() {
        let dir = tempfile::tempdir().unwrap();
        // The module counts its own loads in a global; globals are shared
        // between main.lua and modules (same Lua state).
        fs::write(
            dir.path().join("counted.lua"),
            "__G_loads = (__G_loads or 0) + 1\nreturn {}",
        )
        .unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                run = function(ctx)
                    require('counted')
                    require('counted')
                    require('counted')
                    luban.notify(tostring(__G_loads))
                end
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let output = run_command(
            &mut runtime,
            "run",
            "test.run",
            input(&manifest, dir.path()),
        );
        // Three requires, but the module body ran exactly once.
        assert_eq!(output.actions, vec![HostAction::Notify("1".into())]);
    }

    #[test]
    fn require_rejects_escape_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                parent = function(ctx) require('../evil') end,
                absolute = function(ctx) require('/etc/passwd') end,
                uppercase = function(ctx) require('Lib') end,
                empty_segment = function(ctx) require('a..b') end,
                leading_dot = function(ctx) require('.hidden') end,
                empty = function(ctx) require('') end,
                too_long = function(ctx) require(string.rep('a', 300)) end,
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        for entry in [
            "parent",
            "absolute",
            "uppercase",
            "empty_segment",
            "leading_dot",
            "empty",
            "too_long",
        ] {
            let error = command_error(
                &mut runtime,
                entry,
                "test.run",
                input(&manifest, dir.path()),
            );
            assert!(
                error.contains("module name") || error.contains("invalid module"),
                "{entry}: {error}"
            );
        }
    }

    #[test]
    fn require_circular_dependency_raises() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("self.lua"), "return require('self')").unwrap();
        fs::write(dir.path().join("a.lua"), "return require('b')").unwrap();
        fs::write(dir.path().join("b.lua"), "return require('a')").unwrap();
        fs::write(dir.path().join("lib.lua"), "return { answer = 7 }").unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                itself = function(ctx) require('self') end,
                mutual = function(ctx) require('a') end,
                after = function(ctx) luban.notify(tostring(require('lib').answer)) end,
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let error = command_error(
            &mut runtime,
            "itself",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert!(
            error.contains("module 'self': circular dependency"),
            "{error}"
        );
        let error = command_error(
            &mut runtime,
            "mutual",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert!(error.contains("module 'a': circular dependency"), "{error}");
        // The failed circular loads did not poison the loader: a normal module
        // still loads afterwards.
        let output = run_command(
            &mut runtime,
            "after",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert_eq!(output.actions, vec![HostAction::Notify("7".into())]);
    }

    #[test]
    fn require_module_shares_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        // A module sees the main script's globals (`luban`) and its notify
        // surfaces as a host action of the dispatching call...
        fs::write(
            dir.path().join("sharer.lua"),
            "luban.notify('from module')\nreturn {}",
        )
        .unwrap();
        // ...and the sandbox still hides `io` inside modules.
        fs::write(
            dir.path().join("io_touch.lua"),
            "local handle = io.open('x')\nreturn handle",
        )
        .unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                run = function(ctx) require('sharer') end,
                touch_io = function(ctx) require('io_touch') end,
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let output = run_command(
            &mut runtime,
            "run",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert_eq!(
            output.actions,
            vec![HostAction::Notify("from module".into())]
        );
        let error = command_error(
            &mut runtime,
            "touch_io",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert!(error.contains("nil value"), "{error}");
    }

    #[test]
    fn require_missing_module_errors() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return { run = function(ctx) require('nope') end }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let error = command_error(
            &mut runtime,
            "run",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert!(error.contains("module 'nope'"), "{error}");
    }

    #[test]
    fn require_budget_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("heavy.lua"),
            "for i = 1, 10000000 do end\nreturn {}",
        )
        .unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return { run = function(ctx) require('heavy') end }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        // A 10M-iteration loop is far below the default 50M dispatch budget;
        // shrink the budget so the same invariant — module code runs inside
        // the dispatching call and consumes its instruction budget — is
        // proven deterministically in milliseconds.
        runtime.instruction_limit = 1_000_000;
        let call = runtime.call_command("run", "test.run", input(&manifest, dir.path()));
        assert!(!call.is_ok());
        let error = call.error.unwrap();
        assert!(error.contains("instruction limit exceeded"), "{error}");
    }

    /// The module file itself must not be a symlink pointing outside the
    /// plugin directory: `resolve_module_path` canonicalizes an existing file
    /// and rejects targets outside the canonical plugin dir (symlinked parent
    /// directories were already rejected).
    #[test]
    fn require_rejects_symlinked_module_file() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let dir = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            fs::write(outside.path().join("target.lua"), "return 'pwned'").unwrap();
            symlink(
                outside.path().join("target.lua"),
                dir.path().join("evil.lua"),
            )
            .unwrap();
            let manifest = write_plugin(
                dir.path(),
                r#"return { run = function(ctx) require('evil') end }"#,
            );
            let mut runtime =
                PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
            let error = command_error(
                &mut runtime,
                "run",
                "test.run",
                input(&manifest, dir.path()),
            );
            assert!(error.contains("escapes the plugin directory"), "{error}");
        }
    }

    /// A directory inside the plugin directory that is itself a symlink must
    /// not become a module path component: the resolved parent canonicalizes
    /// to the link target, which lies outside the plugin dir.
    #[test]
    fn require_rejects_symlinked_parent_directory() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let dir = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            fs::write(outside.path().join("evil.lua"), "return 'pwned'").unwrap();
            symlink(outside.path(), dir.path().join("link")).unwrap();
            let manifest = write_plugin(
                dir.path(),
                r#"return { run = function(ctx) require('link.evil') end }"#,
            );
            let mut runtime =
                PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
            let error = command_error(
                &mut runtime,
                "run",
                "test.run",
                input(&manifest, dir.path()),
            );
            assert!(error.contains("escapes the plugin directory"), "{error}");
        }
    }

    /// An error raised in a deeply nested require chain must not accumulate
    /// one mlua Display prefix (or traceback blob) per nesting level: the
    /// status-bar summary would otherwise become pure
    /// `runtime error: runtime error: ...` noise.
    #[test]
    fn require_deep_chain_error_stays_readable() {
        let dir = tempfile::tempdir().unwrap();
        const DEPTH: usize = 60;
        // chain_0 raises; chain_k requires chain_{k-1}, so the error crosses
        // DEPTH require nesting levels before surfacing in the entry.
        fs::write(dir.path().join("chain_0.lua"), "error('boom')").unwrap();
        for k in 1..DEPTH {
            fs::write(
                dir.path().join(format!("chain_{k}.lua")),
                format!("return require('chain_{}')", k - 1),
            )
            .unwrap();
        }
        let manifest = write_plugin(
            dir.path(),
            &format!(
                "return {{ run = function(ctx) require('chain_{}') end }}",
                DEPTH - 1
            ),
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let error = command_error(
            &mut runtime,
            "run",
            "test.run",
            input(&manifest, dir.path()),
        );
        assert!(error.contains("boom"), "{error}");
        // The mlua Display prefix must not stack once per nesting level (the
        // top-level pipeline contributes a constant number, as for any host
        // API error).
        let prefixes = error.matches("runtime error: ").count();
        assert!(
            prefixes <= 3,
            "error prefixes stack per require level ({prefixes}): {error}"
        );
        // Generous bound pinning that the message stays bounded instead of
        // embedding one traceback blob per nesting level.
        assert!(
            error.len() < 30_000,
            "error grew unbounded: {} chars",
            error.len()
        );
    }
}

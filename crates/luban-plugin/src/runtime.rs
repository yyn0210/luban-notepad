//! Sandboxed Lua plugin runtime.
//!
//! Each plugin owns one Lua state. The runtime exposes a small `luban` table and
//! never gives plugins access to `io`, `os`, `package`, `debug`, `require` or
//! dynamic code loading. Document and status changes are collected as actions
//! and applied by the application after the callback returns, so a plugin error
//! cannot leave a half-applied host action.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use mlua::{HookTriggers, Lua, LuaOptions, StdLib, Table, Value, VmState};
use serde_json::{Map, Value as JsonValue};

use crate::manifest::PluginManifest;
use crate::permissions::PermissionSet;

pub const DEFAULT_INSTRUCTION_LIMIT: u64 = 50_000_000;
const HOOK_STEP: u64 = 10_000;
const MEMORY_LIMIT: usize = 64 * 1024 * 1024;
const MAX_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PANEL_LINES: usize = 5_000;
const MAX_PANEL_LINE_BYTES: usize = 4 * 1024;

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

#[derive(Clone, Debug)]
pub struct CallInput {
    pub plugin: PluginIdentity,
    pub document: Option<DocumentSnapshot>,
    pub settings: Map<String, JsonValue>,
    pub allowed_roots: Vec<PathBuf>,
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelLine {
    pub text: String,
    pub command: Option<String>,
}

struct CallState {
    plugin: PluginIdentity,
    permissions: PermissionSet,
    document: Option<DocumentSnapshot>,
    settings: Map<String, JsonValue>,
    allowed_roots: Vec<PathBuf>,
    status_ids: BTreeSet<String>,
    actions: Vec<HostAction>,
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
            "require",
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
        };
        let status_ids: BTreeSet<String> = manifest
            .contributes
            .status_bar
            .iter()
            .map(|item| item.id.clone())
            .collect();
        let state = Rc::new(RefCell::new(CallState::from_input(initial, &status_ids)));
        state.borrow_mut().permissions = permissions.clone();
        install_luban_table(&lua, state.clone())
            .map_err(|error| format!("cannot initialize plugin API: {error}"))?;
        let handler_limit_hit = limit_hit.clone();
        let error_handler = lua
            .create_function(move |lua, message: Value| {
                let message = if handler_limit_hit.get() {
                    "plugin instruction limit exceeded".to_string()
                } else {
                    lua.coerce_string(message)?
                        .map(|text| text.to_string_lossy())
                        .unwrap_or_else(|| "plugin error".to_string())
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
        self.invoke(entry, Some(command_id), input)
            .map_value(|_| ())
    }

    pub fn call_panel(&mut self, entry: &str, input: CallInput) -> RuntimeCall<PanelContent> {
        let call = self.invoke(entry, None, input);
        match call.value {
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
        }
    }

    fn invoke(
        &mut self,
        entry: &str,
        command_id: Option<&str>,
        input: CallInput,
    ) -> RuntimeCall<Value> {
        let status_ids: BTreeSet<String> = self
            .manifest
            .contributes
            .status_bar
            .iter()
            .map(|item| item.id.clone())
            .collect();
        let mut call_state = CallState::from_input(input, &status_ids);
        call_state.permissions = self.permissions.clone();
        let state = Rc::new(RefCell::new(call_state));
        let output = match install_luban_table(&self.lua, state.clone())
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
    fn from_input(input: CallInput, status_ids: &BTreeSet<String>) -> Self {
        Self {
            plugin: input.plugin,
            permissions: PermissionSet::empty(),
            document: input.document,
            settings: input.settings,
            allowed_roots: input.allowed_roots,
            status_ids: status_ids.clone(),
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
/// the message handler is a Rust function backed by `Lua::traceback`.
fn call_with_traceback(
    lua: &Lua,
    function: &mlua::Function,
    handler: &mlua::Function,
    context: mlua::Table,
) -> mlua::Result<Value> {
    let xpcall: mlua::Function = lua.globals().get("xpcall")?;
    let result: mlua::MultiValue = xpcall.call((function.clone(), handler.clone(), context))?;
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

fn install_luban_table(lua: &Lua, state: Rc<RefCell<CallState>>) -> mlua::Result<()> {
    let luban = lua.create_table()?;

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

    let notify_state = state.clone();
    luban.set(
        "notify",
        lua.create_function(move |_, message: String| {
            notify_state
                .borrow_mut()
                .actions
                .push(HostAction::Notify(message));
            Ok(())
        })?,
    )?;

    let json = lua.create_table()?;
    json.set(
        "encode",
        lua.create_function(move |lua, value: Value| match lua_to_json(&value) {
            Ok(value) => match serde_json::to_string(&value) {
                Ok(text) => Ok((Value::String(lua.create_string(&text)?), None)),
                Err(error) => Ok((Value::Nil, Some(error.to_string()))),
            },
            Err(error) => Ok((Value::Nil, Some(error))),
        })?,
    )?;
    json.set(
        "decode",
        lua.create_function(move |lua, text: String| {
            match serde_json::from_str::<JsonValue>(&text) {
                Ok(value) => match json_to_lua(lua, &value) {
                    Ok(value) => Ok((value, None)),
                    Err(error) => Ok((Value::Nil, Some(error.to_string()))),
                },
                Err(error) => Ok((Value::Nil, Some(error.to_string()))),
            }
        })?,
    )?;
    luban.set("json", json)?;

    let document_state = state.clone();
    luban.set(
        "get_document",
        lua.create_function(move |lua, ()| {
            let state = document_state.borrow();
            match &state.document {
                Some(document) => Ok((Value::Table(document_to_lua(lua, document)?), None)),
                None => Ok((Value::Nil, Some("unsupported document".to_string()))),
            }
        })?,
    )?;

    let set_document_state = state.clone();
    luban.set(
        "set_document_text",
        lua.create_function(
            move |_, (text, select_start, select_end): (String, Option<usize>, Option<usize>)| {
                let mut state = set_document_state.borrow_mut();
                let selection = select_start.map(|start| (start, select_end.unwrap_or(start)));
                match replace_document(&mut state, text, selection) {
                    Ok(()) => Ok((Value::Boolean(true), None)),
                    Err(error) => Ok((Value::Nil, Some(error))),
                }
            },
        )?,
    )?;

    let insert_state = state.clone();
    luban.set(
        "insert_text",
        lua.create_function(move |_, text: String| {
            let mut state = insert_state.borrow_mut();
            match insert_document(&mut state, text) {
                Ok(()) => Ok((Value::Boolean(true), None)),
                Err(error) => Ok((Value::Nil, Some(error))),
            }
        })?,
    )?;

    let replace_selection_state = state.clone();
    luban.set(
        "replace_selection",
        lua.create_function(move |_, text: String| {
            let mut state = replace_selection_state.borrow_mut();
            match insert_document(&mut state, text) {
                Ok(()) => Ok((Value::Boolean(true), None)),
                Err(error) => Ok((Value::Nil, Some(error))),
            }
        })?,
    )?;

    let settings = lua.create_table()?;
    let settings_state = state.clone();
    settings.set(
        "get",
        lua.create_function(move |lua, key: String| {
            let state = settings_state.borrow();
            match state.settings.get(&key) {
                Some(value) => Ok((json_to_lua(lua, value)?, None::<String>)),
                None => Ok((Value::Nil, None::<String>)),
            }
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
                return Ok((
                    Value::Nil,
                    Some(format!("status item '{id}' is not declared")),
                ));
            }
            state.actions.push(HostAction::SetStatus { id, text });
            Ok((Value::Boolean(true), None))
        })?,
    )?;
    luban.set("status", status)?;

    let fs = lua.create_table()?;
    let read_state = state.clone();
    fs.set(
        "read",
        lua.create_function(move |lua, path: String| {
            let state = read_state.borrow();
            if !state.permissions.filesystem() {
                return Ok((
                    Value::Nil,
                    Some("plugin does not have the filesystem permission".to_string()),
                ));
            }
            match resolve_allowed_path(&state, &path, false) {
                Ok(path) => match fs::metadata(&path) {
                    Ok(metadata) if metadata.len() > MAX_FILE_BYTES => Ok((
                        Value::Nil,
                        Some(format!("file is larger than {MAX_FILE_BYTES} bytes")),
                    )),
                    Ok(_) => match fs::read_to_string(&path) {
                        Ok(text) => Ok((Value::String(lua.create_string(&text)?), None)),
                        Err(error) => Ok((Value::Nil, Some(error.to_string()))),
                    },
                    Err(error) => Ok((Value::Nil, Some(error.to_string()))),
                },
                Err(error) => Ok((Value::Nil, Some(error))),
            }
        })?,
    )?;
    let write_state = state.clone();
    fs.set(
        "write",
        lua.create_function(move |_, (path, text): (String, String)| {
            let state = write_state.borrow();
            if !state.permissions.filesystem() {
                return Ok((
                    Value::Nil,
                    Some("plugin does not have the filesystem permission".to_string()),
                ));
            }
            if text.len() > MAX_DOCUMENT_BYTES {
                return Ok((
                    Value::Nil,
                    Some(format!("text is larger than {MAX_DOCUMENT_BYTES} bytes")),
                ));
            }
            match resolve_allowed_path(&state, &path, true) {
                Ok(path) => match luban_core::atomic_write::write_atomic(&path, text.as_bytes()) {
                    Ok(()) => Ok((Value::Boolean(true), None)),
                    Err(error) => Ok((Value::Nil, Some(error.to_string()))),
                },
                Err(error) => Ok((Value::Nil, Some(error))),
            }
        })?,
    )?;
    luban.set("fs", fs)?;

    luban.set(
        "data_dir",
        lua.create_function(move |lua, ()| {
            let state = state.borrow();
            if let Err(error) = fs::create_dir_all(&state.plugin.data_dir) {
                return Ok((
                    Value::Nil,
                    Some(format!("cannot create plugin data directory: {error}")),
                ));
            }
            Ok((
                Value::String(
                    lua.create_string(state.plugin.data_dir.to_string_lossy().as_bytes())?,
                ),
                None,
            ))
        })?,
    )?;

    lua.globals().set("luban", luban)?;
    Ok(())
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

fn lua_to_json(value: &Value) -> Result<JsonValue, String> {
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
        Value::Table(table) => table_to_json(table),
        _ => Err("value cannot be encoded as JSON".to_string()),
    }
}

fn table_to_json(table: &Table) -> Result<JsonValue, String> {
    let mut entries = Vec::new();
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, value) = pair.map_err(|error| format!("cannot read Lua table: {error}"))?;
        entries.push((key, lua_to_json(&value)?));
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
            lines: vec![PanelLine {
                text: text
                    .to_str()
                    .map_err(|error| error.to_string())?
                    .to_string(),
                command: None,
            }],
            truncated: false,
        }),
        Value::Table(table) => {
            let title: Option<String> = table.get("title").ok().flatten();
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
            })
        }
        _ => Err("panel function must return a string, an array, or { title, lines }".to_string()),
    }
}

fn parse_panel_lines(value: Value) -> Result<(Vec<PanelLine>, bool), String> {
    let mut lines = Vec::new();
    let mut truncated = false;
    match value {
        Value::String(text) => {
            lines.push(PanelLine {
                text: text
                    .to_str()
                    .map_err(|error| error.to_string())?
                    .to_string(),
                command: None,
            });
        }
        Value::Table(table) => {
            for value in table.sequence_values::<Value>() {
                let value = value.map_err(|error| format!("cannot read panel line: {error}"))?;
                if lines.len() >= MAX_PANEL_LINES {
                    truncated = true;
                    break;
                }
                match value {
                    Value::String(text) => lines.push(PanelLine {
                        text: text
                            .to_str()
                            .map_err(|error| error.to_string())?
                            .to_string(),
                        command: None,
                    }),
                    Value::Table(line) => {
                        let text: String = line
                            .get("text")
                            .map_err(|error| format!("panel line text: {error}"))?;
                        let command: Option<String> = line.get("command").ok().flatten();
                        if text.len() > MAX_PANEL_LINE_BYTES {
                            return Err("panel line is too long".to_string());
                        }
                        lines.push(PanelLine { text, command });
                    }
                    _ => return Err("panel lines must be strings or { text, command }".to_string()),
                }
            }
        }
        _ => return Err("panel lines must be a string or an array".to_string()),
    }
    Ok((lines, truncated))
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

    fn write_plugin(dir: &Path, main: &str) -> PluginManifest {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("plugin.json"),
            r#"{
                "id": "com.example.test",
                "name": "Test",
                "version": "1.0.0",
                "api_version": 1,
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
                    luban.notify("hello")
                    luban.insert_text("X")
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
                    assert(require == nil)
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
                    local ok, err = luban.fs.write(ctx.plugin.data_dir .. "/state.txt", "hello")
                    assert(ok, err)
                    local text = assert(luban.fs.read(ctx.plugin.data_dir .. "/state.txt"))
                    assert(text == "hello")
                end
            }"#,
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), manifest.permissions()).unwrap();
        let call = runtime.call_command("run", "test.run", input(&manifest, dir.path()));
        assert!(call.is_ok(), "{:?}", call.error);
        assert_eq!(
            fs::read_to_string(dir.path().join("data/state.txt")).unwrap(),
            "hello"
        );
        let mut runtime =
            PluginRuntime::new(&manifest, dir.path(), PermissionSet::empty()).unwrap();
        let call = runtime.call_command("run", "test.run", input(&manifest, dir.path()));
        assert!(!call.is_ok());
    }

    #[test]
    fn json_round_trips_and_panels_parse() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_plugin(
            dir.path(),
            r#"return {
                run = function(ctx) luban.notify(luban.json.encode({ a = 1, b = "x", c = { true } })) end,
                list = function(ctx)
                    return { title = "Demo", lines = { "one", { text = "two", command = "test.run" } } }
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
        assert_eq!(panel.lines[1].command.as_deref(), Some("test.run"));
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
}

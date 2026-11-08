# 插件系统 v2 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 按 `docs/superpowers/specs/2026-11-08-plugin-system-v2-design.md` 交付 v2 插件平台：api_version=2、命名空间化 fail-fast API、事件系统、定时器、沙箱 require、面板 args 交互，并将 markdown/json-preview 插件重写为 v2 原生 2.0.0。

**Architecture:** 宿主进程内常驻 mlua VM（已存在）+ 宿主侧 50ms 事件泵（队列 + 去抖 + 定时器到期）。运行时把文档/光标/设置/命令请求收集为 `HostAction`，宿主在分发返回后应用；`on_edit` 只读 + 级联抑制在宿主侧强制执行。

**Tech Stack:** Rust（mlua 0.12 lua54 vendored、serde_json）、GPUI（luban-app）、Lua 5.4（插件）。

**约定（全计划适用）：**
- 工作目录：宿主 `/home/yangyn/workspaces/luban-notepad`；markdown 插件 `/home/yangyn/workspaces/luban-notepad-plugin-markdown`；json 插件 `/home/yangyn/workspaces/luban-notepad-plugin-json-preview`；store `/home/yangyn/workspaces/luban-notepad-store`。
- 本机 Lua 解释器：`~/.local/lua54/usr/bin/lua5.4`。
- 每个任务结束跑 `cargo test -p luban-plugin`（或指明范围）并提交。
- mlua 特性 = `["lua54", "vendored"]`（非 Send 模式）；`PluginRuntime` 已有 `plugin: Table` / `error_handler: mlua::Function` 字段先例，`mlua::Function`/`Table` 值可存入结构体字段。

---

## 文件结构总览

```
crates/luban-plugin/src/
  manifest.rs        [改] API_VERSION=2；events 字段与校验；拒载 v1
  runtime.rs         [改] CallContextKind、HostAction 扩展、PanelCommand、
                          fail-fast API 重写、events/timers 注册表、
                          dispatch_event/fire_timer、沙箱 require
crates/luban-app/src/
  plugin_host.rs     [改] 分发包装 + 隔离计数 + 监听者查询
  plugin_events.rs   [新] PluginEvent 队列 + 50ms 泵 + on_edit 去抖
  app/plugin_ui.rs   [改] 点击不清面板+args、apply_plugin_call 执行器
  app/plugin_manager_ui.rs [改] 审批/管理 UI 展示 events 与 v2 错误
docs/PLUGINS.md     [重写] v2 参考
docs/ROADMAP.md     [改] 延后项
两插件仓库            [重写] v2 原生 2.0.0
```

---

## Phase 1 — luban-plugin：manifest v2

### Task 1: manifest v2（API_VERSION=2、events 字段、拒载 v1）

**Files:**
- Modify: `crates/luban-plugin/src/manifest.rs`
- Test: `crates/luban-plugin/src/manifest.rs`（文件内 `#[cfg(test)]`）

- [ ] **Step 1: 写失败测试**

在 `manifest.rs` 测试模块追加（保留现有测试，本任务末统一把 fixture 的 `"api_version": 1` 全部改为 2）：

```rust
#[test]
fn v2_manifest_parses_events() {
    let dir = write_plugin_dir(
        r#"{
        "id": "com.example.events", "name": "Events", "version": "1.0.0",
        "api_version": 2, "main": "main.lua",
        "events": ["on_edit", "on_document_save"]
    }"#,
        "return {}",
    );
    let manifest = PluginManifest::load(&dir).unwrap();
    assert_eq!(manifest.events, vec!["on_edit", "on_document_save"]);
}

#[test]
fn v1_manifest_is_rejected_with_update_hint() {
    let dir = write_plugin_dir(
        r#"{ "id": "com.example.old", "name": "Old", "version": "1.0.0",
             "api_version": 1, "main": "main.lua" }"#,
        "return {}",
    );
    let error = PluginManifest::load(&dir).unwrap_err();
    assert!(error.contains("api_version 1"), "{error}");
    assert!(error.contains("v2"), "{error}");
}

#[test]
fn unknown_event_name_is_rejected() {
    let dir = write_plugin_dir(
        r#"{ "id": "com.example.e", "name": "E", "version": "1.0.0",
             "api_version": 2, "main": "main.lua", "events": ["on_boom"] }"#,
        "return {}",
    );
    let error = PluginManifest::load(&dir).unwrap_err();
    assert!(error.contains("on_boom"), "{error}");
}
```

> 若现有测试助手名不同（如 `write_plugin`），沿用文件内既有助手；`grep -n "fn write" crates/luban-plugin/src/manifest.rs` 确认。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p luban-plugin manifest` → 期望编译失败（`manifest.events` 不存在）。

- [ ] **Step 3: 实现**

`manifest.rs`：

```rust
/// v2 事件面。运行时 `luban.events.on` 只允许注册此处声明的事件。
pub const EVENT_NAMES: &[&str] = &[
    "on_document_open",
    "on_document_save",
    "on_active_document_changed",
    "on_edit",
];

pub const API_VERSION: u32 = 2;   // 由 1 改为 2
```

`PluginManifest` 增加字段（放在 `permissions` 之后）：

```rust
    #[serde(default)]
    pub events: Vec<String>,
```

`PluginManifest::load` 的版本校验块（现为 `api_version > API_VERSION` 拒绝 / `<` 警告）整体替换为：

```rust
        if self.api_version != API_VERSION {
            return Err(format!(
                "plugin api_version {} is not supported; this host requires api_version 2 (v1 plugins must be updated for the v2 host)",
                self.api_version
            ));
        }
        for event in &self.events {
            if !EVENT_NAMES.contains(&event.as_str()) {
                return Err(format!(
                    "unknown event '{event}' in manifest events (allowed: {})",
                    EVENT_NAMES.join(", ")
                ));
            }
        }
```

同文件既有的 `load` 允许未知字段行为保持不变。把 `manifest.rs` 与 `runtime.rs`、`registry.rs`、`install.rs` 测试里所有 `"api_version": 1` 改为 `"api_version": 2`；`plugin_host.rs` 测试助手 `api_version: 1` → `2`（`crates/luban-app/src/plugin_host.rs:445`）。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p luban-plugin -p luban-app` → 全绿（其余失败留给后续任务，本任务只允许 manifest 相关断言失败归零；若 runtime 测试因两值返回约定未改而失败，属 Task 3 范围，记录跳过）。

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(plugins): require api_version 2 and add declarative events"
```

---

## Phase 2 — luban-plugin：runtime v2

### Task 2: 调用上下文与 HostAction 扩展（类型先行）

**Files:**
- Modify: `crates/luban-plugin/src/runtime.rs`
- Test: `crates/luban-plugin/src/runtime.rs`

- [ ] **Step 1: 定义类型（附编译性测试）**

`runtime.rs` 顶部常量区追加：

```rust
const MAX_EVENT_HANDLERS: usize = 32;
const MAX_TIMERS: usize = 16;
const MIN_TIMER_MS: u64 = 50;
const MAX_NOTIFY_PER_CALL: usize = 8;
const MAX_NOTIFY_BYTES: usize = 1024;
const MAX_COMMAND_DEPTH: u8 = 4;
const MAX_PANEL_ARGS_BYTES: usize = 4 * 1024;
```

新增枚举与 CallInput 扩展：

```rust
/// 分发来源。Event/Edit 上下文受级联抑制；Edit 额外只读。
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
    /// 本调用由 command.run 链的第几层发起（用户/CLI/面板点击 = 0）。
    pub command_depth: u8,
    pub context_kind: CallContextKind,
}
```

`HostAction` 替换为：

```rust
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
    /// 重渲染插件已打开的面板（未打开则忽略）。
    RefreshPanel {
        id: String,
    },
    /// 设置活动文档光标（1-based，宿主钳制）。
    SetCursor {
        line: usize,
        col: usize,
    },
    /// 写回插件设置（宿主按声明 schema 校验并持久化）。
    SetSetting {
        key: String,
        value: JsonValue,
    },
    /// 经宿主命令系统运行命令；depth 已含本次 +1。
    RunCommand {
        id: String,
        args: Option<JsonValue>,
        depth: u8,
    },
}
```

面板命令统一为表（删除字符串形态）：

```rust
/// 面板行/头部动作携带的命令引用。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelCommand {
    pub id: String,
    /// JSON 兼容参数，原样传给命令入口第二参数。
    pub args: Option<JsonValue>,
}

pub struct PanelAction {
    pub label: String,
    pub command: PanelCommand,
}

pub struct PanelLine {
    pub text: String,
    pub command: Option<PanelCommand>,
    pub style: PanelLineStyle,
    pub indent: u8,
    pub spans: Vec<PanelSpan>,
}
```

编译性测试（追加到测试模块）：

```rust
#[test]
fn context_kind_defaults_to_user() {
    assert_eq!(CallContextKind::default(), CallContextKind::User);
}
```

- [ ] **Step 2: 让全 crate 编译**

对 `runtime.rs` 内所有 `PanelLine { command: None/Some(...) }` 构造点与 `parse_panel*` 的 String 解析改为 `PanelCommand`（本任务只做类型替换：`Some(command)` → `Some(PanelCommand { id: command, args: None })`；`parse_panel_actions`/`parse_panel_line` 的完整表解析在 Task 7）。`crates/luban-app` 内匹配 `HostAction` 的 `match` 各处（`plugin_ui.rs:111-140` 等）新增分支暂用：

```rust
luban_plugin::runtime::HostAction::RefreshPanel { .. }
| luban_plugin::runtime::HostAction::SetCursor { .. }
| luban_plugin::runtime::HostAction::SetSetting { .. }
| luban_plugin::runtime::HostAction::RunCommand { .. } => {}
```

Run: `cargo test -p luban-plugin -p luban-app` → 编译通过，既有用例绿。

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat(plugins): v2 call context kinds and host action surface"
```

### Task 3: fail-fast 命名空间 API（install_luban_table 重写）

**Files:**
- Modify: `crates/luban-plugin/src/runtime.rs`（`install_luban_table` 及各解析函数）
- Test: `crates/luban-plugin/src/runtime.rs`

**错误约定实现方式：** mlua 闭包把两值返回 `Ok((Value::Nil, Some(msg)))` 改为
`Err(mlua::Error::RuntimeError(msg))`；成功路径直接返回值。既有
`call_with_traceback` + `error_handler` 已捕获并带 traceback，无需改动。

- [ ] **Step 1: 重写失败测试**

把现有测试中断言 `(nil, err)`/`(value, nil)` 的用例整体改写为 fail-fast 断言。代表性用例（完整替换 `json_round_trips_and_panels_parse` 中的 API 调用部分与 `filesystem_requires_permission_and_stays_in_roots`）：

```rust
#[test]
fn document_api_is_namespaced_and_fail_fast() {
    let (mut runtime, _dir) = plugin_with_document("hello");
    // 无文档时 get 返回 nil 而非报错
    let lua_code = "local d = luban.document.get(); return d == nil";
    // set_text 在只读/无文档上下文抛错：
    let err = run_command_error(&mut runtime, "write", "luban.document.set_text('x', 0, 0)");
    assert!(err.contains("unsupported document"), "{err}");
}
```

> `plugin_with_document` / `run_command_error` 若无现成助手，按文件内既有测试助手风格新增（参考 `input()` 助手 `runtime.rs:1344`）。测试脚本统一通过 `runtime.call_command("entry", "cmd", None, input)` 驱动，entry 函数体即 `lua_code`。

- [ ] **Step 2: 运行确认失败** — `cargo test -p luban-plugin` → 旧 API 断言失败。

- [ ] **Step 3: 重写 install_luban_table**

新结构（完整骨架，函数体给出代表性的，其余按同一模式机械替换；签名一览表在 Step 4）：

```rust
fn install_luban_table(lua: &Lua, state: Rc<RefCell<CallState>>) -> mlua::Result<()> {
    let luban = lua.create_table()?;
    luban.set("version", 2)?;

    // log：不失败，保持原实现不变
    luban.set("log", /* 原实现 */)?;

    // notify：资源超限静默丢弃
    let notify_state = state.clone();
    luban.set(
        "notify",
        lua.create_function(move |_, message: String| {
            let mut state = notify_state.borrow_mut();
            if message.len() > MAX_NOTIFY_BYTES {
                message.truncate(MAX_NOTIFY_BYTES);
            }
            let count = state
                .actions
                .iter()
                .filter(|a| matches!(a, HostAction::Notify(_)))
                .count();
            if count >= MAX_NOTIFY_PER_CALL {
                log::warn!("plugin '{}' notify dropped (limit {MAX_NOTIFY_PER_CALL})", state.plugin.id);
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
            let value = lua_to_json(&value).map_err(mlua::Error::RuntimeError)?;
            let text = serde_json::to_string(&value).map_err(mlua::Error::RuntimeError)?;
            Ok(Value::String(lua.create_string(&text)?))
        })?,
    )?;
    json.set(
        "decode",
        lua.create_function(move |lua, text: String| {
            let value: JsonValue =
                serde_json::from_str(&text).map_err(mlua::Error::RuntimeError)?;
            json_to_lua(lua, &value)
        })?,
    )?;
    luban.set("json", json)?;

    // document.*
    let document_state = state.clone();
    let document = lua.create_table()?;
    document.set(
        "get",
        lua.create_function(move |lua, ()| {
            let state = document_state.borrow();
            match &state.document {
                Some(document) => Ok(Value::Table(document_to_lua(lua, document)?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;
    let set_state = state.clone();
    document.set(
        "set_text",
        lua.create_function(move |_, (text, start, end): (String, Option<usize>, Option<usize>)| {
            let mut state = set_state.borrow_mut();
            ensure_document_writable(&state)?;
            let selection = match (start, end) {
                (None, None) => None,
                (Some(s), Some(e)) => Some((s, e)),
                _ => return Err(mlua::Error::RuntimeError(
                    "set_text expects (text, sel_start, sel_end) with both offsets".into(),
                )),
            };
            replace_document(&mut state, text, selection).map_err(mlua::Error::RuntimeError)?;
            Ok(())
        })?,
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

    // settings.get + settings.set
    let settings = lua.create_table()?;
    settings.set("get", /* 原实现改 fail-fast：键不存在返回 nil */)?;
    let set_state = state.clone();
    settings.set(
        "set",
        lua.create_function(move |_, (key, value): (String, Value)| {
            let mut state = set_state.borrow_mut();
            let json = lua_to_json(Value::Table(value_table) /* 见下方说明 */)
                .map_err(mlua::Error::RuntimeError)?;
            validate_setting(&state, &key, json.clone()).map_err(mlua::Error::RuntimeError)?;
            state.actions.push(HostAction::SetSetting { key, value: json });
            Ok(())
        })?,
    )?;
    luban.set("settings", settings)?;

    // status / syntax：同模式改 fail-fast（status.set 未声明 id 抛错；syntax.highlight 超帽抛错）
    // fs.read / fs.write：同模式改 fail-fast
    // panel.open / panel.refresh
    let panel = lua.create_table()?;
    let panel_state = state.clone();
    panel.set("open", /* 原 id 校验改 fail-fast */)?;
    let refresh_state = state.clone();
    panel.set(
        "refresh",
        lua.create_function(move |_, id: String| {
            if id.is_empty() || id.len() > 128 {
                return Err(mlua::Error::RuntimeError("invalid panel id".into()));
            }
            refresh_state
                .borrow_mut()
                .actions
                .push(HostAction::RefreshPanel { id });
            Ok(())
        })?,
    )?;
    luban.set("panel", panel)?;

    // command.run
    let run_state = state.clone();
    luban.set(
        "command",
        lua.create_function(move |lua, (id, args): (String, Option<Value>)| {
            if id.is_empty() || id.len() > 128 {
                return Err(mlua::Error::RuntimeError("invalid command id".into()));
            }
            let mut state = run_state.borrow_mut();
            let args = match args {
                Some(value) => Some(lua_to_json(value).map_err(mlua::Error::RuntimeError)?),
                None => None,
            };
            let depth = state.command_depth + 1;
            if depth > MAX_COMMAND_DEPTH {
                return Err(mlua::Error::RuntimeError(format!(
                    "command.run exceeds the maximum depth of {MAX_COMMAND_DEPTH}"
                )));
            }
            state.actions.push(HostAction::RunCommand { id, args, depth });
            Ok(())
        })?,
    )?;

    // luban.data_dir：原实现改 fail-fast
    lua.globals().set("luban", luban)?;
    Ok(())
}
```

`set_text` 签名说明：`value_table` 处实际实现为把 settings.set 的第二参数直接以
`Value` 传入 `lua_to_json(&value)`——闭包参数写
`(key, value): (String, Value)`，函数体里 `lua_to_json(&value)`（需把
`lua_to_json` 首参改为 `&Value`，同步调整 `json.encode` 调用点）。

新增助手：

```rust
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

fn validate_setting(state: &CallState, key: &str, value: JsonValue) -> Result<(), String> {
    // CallState 增加 schemas: Vec<SettingContribution>（from_input 时从 manifest 复制）。
    let schema = state
        .schemas
        .iter()
        .find(|setting| setting.key == key)
        .ok_or_else(|| format!("setting '{key}' is not declared"))?;
    if !luban_plugin_manifest_value_is_valid(schema, &value) {
        return Err(format!("invalid value for setting '{key}'"));
    }
    Ok(())
}
```

`luban_plugin_manifest_value_is_valid` 即
`luban_plugin::manifest::setting_value_is_valid`（crate 内直接
`crate::manifest::setting_value_is_valid`）。`CallState::from_input` 增加
`schemas`（从调用方传入：`invoke`/`new` 处 `manifest.contributes.settings.clone()`）与
`command_depth: input.command_depth`、`context_kind: input.context_kind`。

**v2 API 签名一览（全部函数，实现不得偏离）：**

| Lua API | 行为 |
|---|---|
| `luban.version` | 常量 `2` |
| `luban.log(level, msg)` | 不变 |
| `luban.notify(msg)` | >1KiB 截断；每分发 >8 条丢弃 + warn 日志 |
| `luban.json.encode(t)` / `decode(s)` | 失败抛错 |
| `luban.document.get()` | 无文档 → nil |
| `luban.document.set_text(text, s?, e?)` | Edit 上下文/无文档 → 抛错 |
| `luban.document.insert(text)` | 同上 |
| `luban.document.set_cursor(line, col)` | 1-based，0 值抛错；push SetCursor |
| `luban.settings.get(key)` | 未声明键 → nil |
| `luban.settings.set(key, v)` | 未声明/非法 → 抛错；push SetSetting |
| `luban.status.set(id, text)` | 未声明 id → 抛错 |
| `luban.syntax.highlight(text, lang)` | >64KiB → 抛错 |
| `luban.fs.read(path)` / `write(path, text)` | 无权限/越界 → 抛错；成功返回文本/true |
| `luban.data_dir()` | 失败抛错 |
| `luban.panel.open(id)` | 非法 id → 抛错 |
| `luban.panel.refresh(id)` | 非法 id → 抛错；push RefreshPanel |
| `luban.command.run(id, args?)` | 非法 id/深度超限 → 抛错；push RunCommand |

同时**删除**旧扁平入口：`get_document`、`set_document_text`、`insert_document`、
顶层 `panel.open` 之外的旧字段（以删除后 `grep -n "luban.set(" runtime.rs` 对照
上表为准：version/log/notify/json/document/settings/status/syntax/fs/panel/command/data_dir）。

- [ ] **Step 4: 更新全部既有测试到 v2 约定**

`runtime.rs` 测试模块逐个改写：两值断言 → 直接断言值；错误断言
`call.error` 包含预期消息。覆盖上表全部 17 项的至少一个正/反用例（fail-fast
抛错经 `RuntimeCall.error` 断言包含消息片段）。

- [ ] **Step 5: 运行确认通过**

Run: `cargo test -p luban-plugin` → 全绿。

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat(plugins): namespaced fail-fast Lua API v2"
```

### Task 4: 事件注册与分发

**Files:**
- Modify: `crates/luban-plugin/src/runtime.rs`
- Test: `crates/luban-plugin/src/runtime.rs`

- [ ] **Step 1: 失败测试**

```rust
#[test]
fn events_register_dispatch_and_are_contained() {
    let (manifest, dir) = manifest_with_events(&["on_edit"]);
    let mut runtime = PluginRuntime::new(&manifest, &dir, Default::default()).unwrap();
    write_main(
        &dir,
        r#"
        local M = {}
        local seen = {}
        function M.on_edit(doc) seen[#seen+1] = doc.text; error("boom") end
        function M.collect() return #seen end
        M.internal = M.on_edit
        return M
        "#,
    );
    // 重新加载（测试助手直接重建 runtime）
    let mut runtime = PluginRuntime::new(&manifest, &dir, Default::default()).unwrap();
    runtime.register_event("on_edit", "on_edit").unwrap();
    let call = runtime.dispatch_event("on_edit", edit_input("abc"));
    assert!(call.error.is_some()); // handler 抛错被包含，不影响宿主
    // 再次注册 + 分发仍安全（连续失败隔离由宿主计数，见 Task 8）
}
```

> 测试助手 `write_main` 重写插件目录 main.lua 后重建 runtime；若无则新增。

- [ ] **Step 2: 运行确认失败**

- [ ] **Step 3: 实现**

`PluginRuntime` 增加字段：

```rust
    event_handlers: Rc<RefCell<BTreeMap<String, Vec<mlua::Function>>>>,
    timers: Rc<RefCell<BTreeMap<String, TimerEntry>>>,
    consecutive_failures: Cell<u32>,
```

```rust
#[derive(Clone)]
struct TimerEntry {
    callback: mlua::Function,
    next_due: std::time::Instant,
    interval: Option<std::time::Duration>,
}
```

`new()` 初始化两个 Rc 表；`install_luban_table` 签名追加
`events: Rc<RefCell<...>>, timers: Rc<RefCell<...>>` 两个参数并挂接：

```rust
    let events = lua.create_table()?;
    let on_events = events_registry.clone();
    let on_allowed: BTreeSet<String> = state.borrow().allowed_events.clone();
    let on_state = state.clone();
    events.set(
        "on",
        lua.create_function(move |_, (name, handler): (String, mlua::Function)| {
            if !on_allowed.contains(&name) {
                return Err(mlua::Error::RuntimeError(format!(
                    "event '{name}' is not declared in the manifest"
                )));
            }
            let mut registry = on_events.borrow_mut();
            let total: usize = registry.values().map(Vec::len).sum();
            if total >= MAX_EVENT_HANDLERS {
                return Err(mlua::Error::RuntimeError(format!(
                    "too many event handlers (limit {MAX_EVENT_HANDLERS})"
                )));
            }
            registry.entry(name).or_default().push(handler);
            Ok(())
        })?,
    )?;
    let off_events = events_registry.clone();
    events.set(
        "off",
        lua.create_function(move |_, name: String| {
            off_events.borrow_mut().remove(&name);
            Ok(())
        })?,
    )?;
    luban.set("events", events)?;
```

`CallState` 增加 `allowed_events: BTreeSet<String>`（invoke/new 时从
`manifest.events` 复制）。

`PluginRuntime` 方法：

```rust
    /// 供 activate 阶段之外的显式注册（当前仅测试使用）。
    pub fn register_event(&mut self, event: &str, handler_name: &str) -> Result<(), String> { /* 从 plugin 表取函数放入 registry */ }

    /// 分发事件：依次调用全部处理器；单个失败记录并继续。
    pub fn dispatch_event(&mut self, event: &str, input: CallInput) -> RuntimeCall<()> {
        let handlers = self.event_handlers.borrow().get(event).cloned().unwrap_or_default();
        if handlers.is_empty() { return RuntimeCall { output: Default::default(), value: Some(()), error: None }; }
        // 复用 invoke 的 CallState 准备路径（抽出 prepare_call(input)），预算重置，
        // 逐个 call_with_traceback(handler, (context, Value::Nil))；
        // 任一失败 → error = Some(首条错误)；全部成功 → error = None
        // 末尾 record_outcome(error.is_none())
    }

    fn record_outcome(&self, ok: bool) {
        if ok { self.consecutive_failures.set(0); }
        else { self.consecutive_failures.set(self.consecutive_failures.get().saturating_add(1)); }
    }
    pub fn should_quarantine(&self) -> bool { self.consecutive_failures.get() >= 10 }
```

`invoke()` 抽出公共准备逻辑为 `prepare_call(input: CallInput) -> Rc<RefCell<CallState>>`
（含 context_kind/command_depth/schemas/allowed_events 传递），命令/面板/事件/定时器
四条路径共用；命令与面板路径也改为末尾 `record_outcome`。**注意**：把 Task 3 中
`install_luban_table` 的调用点同步传入新参数。

- [ ] **Step 4: 运行确认通过** — `cargo test -p luban-plugin`

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(plugins): event registry, dispatch and failure quarantine"
```

### Task 5: 定时器

**Files:**
- Modify: `crates/luban-plugin/src/runtime.rs`
- Test: `crates/luban-plugin/src/runtime.rs`

- [ ] **Step 1: 失败测试**

```rust
#[test]
fn timers_clamp_and_fire_once() {
    let (manifest, dir) = manifest_with_events(&[]); // 定时器不需要事件声明
    let mut runtime = PluginRuntime::new(&manifest, &dir, Default::default()).unwrap();
    write_main(&dir, r#"
        local M = {}
        local fired = 0
        function M.tick() fired = fired + 1 end
        function M.count() return fired end
        return M
    "#);
    let mut runtime = PluginRuntime::new(&manifest, &dir, Default::default()).unwrap();
    // 经 activate 里的 timer.every 注册在 Task 9 插件侧验证；此处直接测 API：
    runtime.register_timer("t1", 10, "tick", true).unwrap(); // <50ms 被钳制
    assert!(runtime.due_timer_ids(std::time::Instant::now()).is_empty());
    std::thread::sleep(std::time::Duration::from_millis(60));
    assert_eq!(runtime.due_timer_ids(std::time::Instant::now()), vec!["t1"]);
    runtime.fire_timer("t1", user_input()).unwrap();
    // one_shot：不再到期
    assert!(runtime.due_timer_ids(std::time::Instant::now()).is_empty());
    assert!(runtime.fire_timer("ghost", user_input()).is_err());
}
```

> `register_timer` 为测试专用公共方法（绕过 Lua 注册）；生产路径走
> `luban.timer.after/every`。

- [ ] **Step 2: 运行确认失败**

- [ ] **Step 3: 实现**

`install_luban_table`：

```rust
    let timer = lua.create_table()?;
    timer.set("after", install_timer_fn(timers.clone(), state.clone(), true)?)?;
    timer.set("every", install_timer_fn(timers.clone(), state.clone(), false)?)?;
    timer.set(
        "cancel",
        lua.create_function(move |_, id: String| {
            timers.borrow_mut().remove(&id);
            Ok(())
        })?,
    )?;
    luban.set("timer", timer)?;
```

```rust
fn install_timer_fn(
    timers: Rc<RefCell<BTreeMap<String, TimerEntry>>>,
    state: Rc<RefCell<CallState>>,
    one_shot: bool,
) -> mlua::Result<mlua::Function> {
    // 闭包 (id, ms, handler)：ms < 50 钳制到 50；len(timers) >= 16 抛错；
    // 重复 id 覆盖；TimerEntry { callback, next_due: Instant::now() + ms, interval: one_shot.then(|| None) 或 every 的 Some(ms) }
}
```

`PluginRuntime`：

```rust
    pub fn due_timer_ids(&self, now: std::time::Instant) -> Vec<String> { /* 顺序返回到期 id */ }
    pub fn fire_timer(&mut self, id: &str, input: CallInput) -> Result<RuntimeCall<()>, String> {
        // 取出 entry（every 的重排 next_due = now + interval；after 移除），
        // 经 prepare_call 分发 callback(context, Nil)，record_outcome
    }
```

- [ ] **Step 4: 运行确认通过** — `cargo test -p luban-plugin`

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(plugins): sandboxed timers"
```

### Task 6: 沙箱 require

**Files:**
- Modify: `crates/luban-plugin/src/runtime.rs`
- Test: `crates/luban-plugin/src/runtime.rs`

- [ ] **Step 1: 失败测试**

```rust
#[test]
fn require_loads_local_modules_and_blocks_escape() {
    let (manifest, dir) = minimal_manifest();
    std::fs::write(dir.join("lib.lua"), "return { answer = 42 }").unwrap();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/mod.lua"), "return require('lib').answer").unwrap();
    std::fs::write(dir.join("evil.lua"), "return 1").unwrap();
    write_main(&dir, r#"
        local M = {}
        function M.answer() return require('lib').answer end
        function M.nested() return require('sub/mod')() end   -- sub/mod 返回函数？不：返回 42
        function M.escape() return require('../evil') end
        function M.absolute() return require('/etc/passwd') end
        function M.badname() return require('Lib') end
        function M.cached() require('lib'); require('lib'); return require('lib').answer end
        return M
    "#);
    // 逐项断言：answer=42、nested=42、escape/absolute/badname 抛错（含 'require'）、cached=42（缓存不重复加载——用 package 计数模块验证）
}
```

- [ ] **Step 2: 运行确认失败**

- [ ] **Step 3: 实现**

`new()` 中沙箱黑名单移除 `require` 的置 Nil（`globals.set("require", Value::Nil)` 名单里删掉 `"require"`），改为：

```rust
    let module_cache: Rc<RefCell<std::collections::HashMap<String, Value>>> =
        Rc::new(RefCell::new(std::collections::HashMap::new()));
    let plugin_dir = dir.clone();
    let require = lua.create_function(move |lua, name: String| {
        if let Some(value) = module_cache.borrow().get(&name) {
            return Ok(value.clone());
        }
        let path = resolve_module_path(&plugin_dir, &name).map_err(mlua::Error::RuntimeError)?;
        let source = fs::read_to_string(&path).map_err(|error| {
            mlua::Error::RuntimeError(format!("module '{name}': {error}"))
        })?;
        let value: Value = lua
            .load(&source)
            .set_name(format!("@{name}.lua"))
            .eval()
            .map_err(mlua::Error::RuntimeError)?;
        module_cache.borrow_mut().insert(name, value.clone());
        Ok(value)
    })?;
    globals.set("require", require)?;
```

```rust
fn resolve_module_path(plugin_dir: &Path, name: &str) -> Result<PathBuf, String> {
    if name.is_empty() || name.len() > 256 {
        return Err("invalid module name".into());
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'_' || b == b'/')
    {
        return Err(format!("module name '{name}' may only contain [a-z0-9._/]"));
    }
    if name.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
        return Err(format!("module name '{name}' must be relative"));
    }
    let relative: PathBuf = name.split('.').collect(); // "lib.parser" -> lib/parser.lua 由调用方补后缀
    // 注意：'/' 与 '.' 双语义混乱，规定：只允许 '.' 分隔（模块名不含 '/'），上面 sub/mod 测试改为 sub.mod
    let mut path = plugin_dir.to_path_buf();
    for part in name.split('.') {
        path.push(part);
    }
    path.set_extension("lua");
    // 封闭性：父目录 canonicalize 后必须仍在 plugin_dir 内
    let parent = path.parent().unwrap();
    let resolved_parent = parent
        .canonicalize()
        .map_err(|error| format!("module '{name}': {error}"))?;
    if !resolved_parent.starts_with(&plugin_dir.canonicalize().unwrap_or_else(|_| plugin_dir.to_path_buf())) {
        return Err(format!("module '{name}' escapes the plugin directory"));
    }
    Ok(path)
}
```

> 上文测试里 `require('sub/mod')` 应改为 `require('sub.mod')`——**模块名只允许 `.` 分隔**。

- [ ] **Step 4: 运行确认通过** — `cargo test -p luban-plugin`

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(plugins): sandboxed in-plugin require"
```

### Task 7: 面板 args 解析

**Files:**
- Modify: `crates/luban-plugin/src/runtime.rs`
- Test: `crates/luban-plugin/src/runtime.rs`

- [ ] **Step 1: 失败测试**

```rust
#[test]
fn panel_lines_and_actions_parse_table_commands_with_args() {
    // parse 面板内容：
    //   lines[1].command = { id = "json.fold.toggle", args = { path = "$.a" } }
    //   lines[2].command = "plain"      -> 抛错（v2 仅表形态）
    //   lines[3].command = { id = "x" } -> args = None
    //   actions[1].command 同表形态；args 序列化 > 4KiB -> args 丢弃并告警
}
```

- [ ] **Step 2: 运行确认失败**

- [ ] **Step 3: 实现**

`parse_panel_line` 中 command 分支：

```rust
    let command = match line.get("command")? {
        Value::Nil => None,
        Value::Table(command) => {
            let id: String = command.get("id")?;
            if id.is_empty() || id.len() > MAX_PANEL_ACTION_COMMAND_BYTES {
                return Err("invalid command id".into());
            }
            let args = match command.get::<Value>("args")? {
                Value::Nil => None,
                value => match lua_to_json(&value) {
                    Ok(json) => {
                        let serialized = serde_json::to_string(&json).unwrap_or_default();
                        if serialized.len() > MAX_PANEL_ARGS_BYTES {
                            log::warn!("panel command '{id}' args dropped (over 4 KiB)");
                            None
                        } else {
                            Some(json)
                        }
                    }
                    Err(_) => None,
                },
            };
            Some(PanelCommand { id, args })
        }
        _ => return Err("line command must be a table { id, args }".into()),
    };
```

`parse_panel_actions` 的 command 字段同模式。`RuntimeCall<PanelContent>` 不变。

- [ ] **Step 4: 运行确认通过** — `cargo test -p luban-plugin`

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(plugins): panel commands carry typed args"
```

### Task 8: args 传参与入口第二参数（runtime 侧收口）

**Files:**
- Modify: `crates/luban-plugin/src/runtime.rs`
- Test: `crates/luban-plugin/src/runtime.rs`

- [ ] **Step 1: 失败测试**

```rust
#[test]
fn command_entry_receives_args_as_second_parameter() {
    // entry 函数：function M.run(ctx, args) return (args and args.n) or 0 end
    // call_command("run", "t.args", Some(json!({"n": 7})), input) -> value == 7
    // call_command("run", "t.args", None, input) -> value == 0
}
```

- [ ] **Step 2: 运行确认失败**

- [ ] **Step 3: 实现**

```rust
    pub fn call_command(
        &mut self,
        entry: &str,
        command_id: &str,
        args: Option<JsonValue>,
        input: CallInput,
    ) -> RuntimeCall<()> {
        // invoke(entry, Some(command_id), args, input) 后 map_value(|_| ())
    }

    fn invoke(
        &mut self,
        entry: &str,
        command_id: Option<&str>,
        args: Option<JsonValue>,
        input: CallInput,
    ) -> RuntimeCall<Value> {
        // prepare_call 后：
        let args_value = match args {
            Some(json) => json_to_lua(&self.lua, &json).unwrap_or(Value::Nil),
            None => Value::Nil,
        };
        // call_with_traceback(&self.lua, &function, &self.error_handler, (context, args_value))
    }
```

`call_panel` 调 `invoke(entry, None, None, input)`；`dispatch_event` /
`fire_timer` 传 `None`。旧 `call_command` 签名的调用方
（`plugin_host.rs:298`）在 Task 9 同步更新。

- [ ] **Step 4: 运行确认通过** — `cargo test -p luban-plugin`

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(plugins): entries receive args as second parameter"
```

---

## Phase 3 — luban-app：宿主集成

### Task 9: PluginHost 分发扩展与隔离计数

**Files:**
- Modify: `crates/luban-app/src/plugin_host.rs`
- Modify: `crates/luban-app/src/app/plugin_ui.rs`（仅 `run_plugin_command` 适配新签名）
- Test: `crates/luban-app/src/plugin_host.rs`

- [ ] **Step 1: 失败测试**

```rust
#[test]
fn host_tracks_consecutive_failures() {
    // 构造带失败命令的插件目录（api_version 2），PluginHost::load 后：
    // run_command 连续失败 10 次 -> host.quarantine_due(id) == true
    // 一次成功 -> 计数清零
}
```

> 现有测试用 `entry()` 助手构造 manifest（`plugin_host.rs:437`），`api_version` 改 2 后即可复用；需要真实 Lua 目录的用例写在 `crates/luban-plugin` 侧，宿主侧只测计数逻辑时可用假 runtime——若 `PluginRuntime` 不可注入，则把计数逻辑抽成 `struct FailureTracker`（`record(ok)` / `should_quarantine()`）单测，宿主持有 `BTreeMap<String, FailureTracker>`。**采用 FailureTracker 方案**，`PluginRuntime::record_outcome/should_quarantine` 保留供插件 crate 直测。

- [ ] **Step 2: 运行确认失败**

- [ ] **Step 3: 实现**

`plugin_host.rs`：

```rust
#[derive(Default)]
struct FailureTracker {
    consecutive: u32,
}
impl FailureTracker {
    fn record(&mut self, ok: bool) {
        if ok { self.consecutive = 0; } else { self.consecutive = self.consecutive.saturating_add(1); }
    }
    fn should_quarantine(&self) -> bool { self.consecutive >= 10 }
}

pub struct PluginHost {
    // ...既有字段...
    failures: BTreeMap<String, FailureTracker>,
}
```

- `run_command` 适配新签名 `(plugin_id, entry, command_id, args, input)`，
  末尾 `self.failures.entry(id).or_default().record(call.is_ok())`。
- `panel_content` 同样记录。
- 新增：

```rust
    pub fn listeners_of(&self, event: &str) -> Vec<String> {
        self.plan
            .order
            .iter()
            .filter(|id| {
                self.runtimes.contains_key(*id)
                    && self
                        .manifest(id)
                        .is_some_and(|manifest| manifest.events.iter().any(|e| e == event))
            })
            .cloned()
            .collect()
    }

    pub fn dispatch_event(&mut self, plugin_id: &str, event: &str, input: CallInput) -> Option<RuntimeCall<()>> {
        let runtime = self.runtimes.get_mut(plugin_id)?;
        let call = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.dispatch_event(event, input)
        }))
        .unwrap_or_else(|_| panic_call(()));
        self.failures.entry(plugin_id.to_string()).or_default().record(call.is_ok());
        Some(call)
    }

    pub fn due_timer_ids(&self, plugin_id: &str, now: std::time::Instant) -> Vec<String> { /* runtime 委托 */ }
    pub fn fire_timer(&mut self, plugin_id: &str, id: &str, input: CallInput) -> Option<RuntimeCall<()>> { /* 同 dispatch_event 模式 + 记录 */ }
    pub fn quarantine_due(&self, plugin_id: &str) -> bool { self.failures.get(plugin_id).is_some_and(|t| t.should_quarantine()) }
```

`plugin_ui.rs` 的 `run_plugin_command` 调用点改为传 `args`（新参数
`args: Option<serde_json::Value>`，暂传 `None`）。

- [ ] **Step 4: 运行确认通过** — `cargo test -p luban-app`

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(plugins): host dispatch extensions and failure quarantine"
```

### Task 10: 事件泵 plugin_events.rs

**Files:**
- Create: `crates/luban-app/src/plugin_events.rs`
- Modify: `crates/luban-app/src/app/mod.rs`（字段与 spawn）、`app/tabs.rs`、`app/save.rs`、`app/plugin_ui.rs`
- Test: `crates/luban-app/src/plugin_events.rs`

- [ ] **Step 1: 队列与去抖状态机的单测（先写）**

```rust
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub struct EditSignal {
    pub changed: bool,
    pub emit_after: Duration,
}

/// 纯逻辑：给定每次轮询的代数快照，决定是否发射 on_edit。
/// 简化自 watch_plugin_panel 的 (watch, tab.id, edit_generation, revision) 四元组。
#[derive(Clone, PartialEq)]
pub struct EditGeneration(pub u64);

pub struct EditDebouncer {
    last: Option<EditGeneration>,
    changed_at: Option<Instant>,
}
impl EditDebouncer {
    pub fn new() -> Self { Self { last: None, changed_at: None } }
    /// stable_after：本次轮询距上次变化是否已稳定达标
    pub fn poll(&mut self, generation: Option<EditGeneration>, now: Instant) -> bool {
        match generation {
            None => { self.last = None; self.changed_at = None; false }
            Some(generation) => match self.last.take() {
                None => { self.last = Some(generation); false }
                Some(previous) if previous != generation => {
                    self.last = Some(generation);
                    self.changed_at = Some(now);
                    false
                }
                Some(_) => {
                    self.last = Some(generation);
                    self.changed_at.take().is_some_and(|since| now.duration_since(since) >= Duration::from_millis(150))
                }
            },
        }
    }
}

#[test]
fn edit_debouncer_emits_once_after_stability() {
    let mut d = EditDebouncer::new();
    let t0 = Instant::now();
    assert!(!d.poll(Some(EditGeneration(1)), t0));
    assert!(!d.poll(Some(EditGeneration(2)), t0)); // 变化
    assert!(!d.poll(Some(EditGeneration(2)), t0 + Duration::from_millis(100)));
    assert!(d.poll(Some(EditGeneration(2)), t0 + Duration::from_millis(160))); // 发射
    assert!(!d.poll(Some(EditGeneration(2)), t0 + Duration::from_millis(200))); // 不重复
}
```

- [ ] **Step 2: 运行确认通过（纯逻辑先绿）** — `cargo test -p luban-app plugin_events`

- [ ] **Step 3: 事件类型与泵**

```rust
//! Application-side plugin event pump.
//!
//! Collects app events into a queue and dispatches them to plugin runtimes on
//! a 50 ms tick: at most 8 dispatches per tick, on_edit after 150 ms of edit
//! stability, due timers fire on the same tick.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use luban_plugin::runtime::{CallContextKind, CallInput, DocumentSnapshot};

#[derive(Clone, Debug)]
pub enum PluginEvent {
    DocumentOpened(DocumentSnapshot),
    DocumentSaved(DocumentSnapshot),
    ActiveDocumentChanged(Option<DocumentSnapshot>),
}

pub const PUMP_TICK: Duration = Duration::from_millis(50);
pub const MAX_DISPATCHES_PER_TICK: usize = 8;
pub const EDIT_STABLE_AFTER: Duration = Duration::from_millis(150);

/// LubanApp 字段：
///   plugin_event_queue: VecDeque<PluginEvent>,
///   plugin_event_depth: u32,           // >0 表示处于事件上下文（级联抑制）
///   plugin_pump_started: bool,
///   plugin_edit_debouncer: EditDebouncer,
```

emit 帮助方法（挂在 `LubanApp`，plugin_events.rs 的 `impl LubanApp` 块）：

```rust
    pub(super) fn plugin_emit(&mut self, event: PluginEvent) {
        if self.plugin_event_depth > 0 {
            return; // 级联抑制
        }
        self.plugin_event_queue.push_back(event);
    }
```

发射点（各一行，包在既有成功路径内）：

- `app/mod.rs` `open_path`（`mod.rs:1698`）打开成功返回 true 前：
  `self.plugin_emit(PluginEvent::DocumentOpened(snapshot))`——快照用
  `plugin_ui.rs:55` 的 `plugin_document_snapshot`（打开后光标即文档当前态）。
- `app/save.rs` `save_active` 保存成功后：
  `self.plugin_emit(PluginEvent::DocumentSaved(snapshot))`。
- `app/tabs.rs` `activate_tab_index`（`tabs.rs:816`）尾部：
  `self.plugin_emit(PluginEvent::ActiveDocumentChanged(self.plugin_document_snapshot(cx)))`
  （无活动文档时为 `PluginEvent::ActiveDocumentChanged(None)`）。

泵循环（spawn 于 `reload_plugins_now` 之后与 `LubanApp::new` 尾部，`plugin_pump_started` 防重复）：

```rust
    fn start_plugin_pump(&mut self, window: gpui::Window, cx: &mut gpui::Context<Self>) {
        if self.plugin_pump_started { return; }
        self.plugin_pump_started = true;
        cx.spawn_in(window, async move |this, cx| {
            let mut tick = tokio_interval_placeholder(); // 用 background_executor().timer(PUMP_TICK) 循环
            loop {
                cx.background_executor().timer(PUMP_TICK).await;
                if this.update(cx, |app, _cx| app.plugin_pump_generation()).is_err() {
                    break; // 应用已退出
                }
                let _ = this.update_in(cx, |app, window, cx| app.plugin_pump_tick(window, cx));
            }
        }).detach();
    }
```

`plugin_pump_tick(window, cx)`：

```rust
    fn plugin_pump_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = Instant::now();
        // 1) 队列事件（≤8）
        let mut budget = MAX_DISPATCHES_PER_TICK;
        while budget > 0 {
            let Some(event) = self.plugin_event_queue.pop_front() else { break };
            self.dispatch_plugin_event(event, window, cx);
            budget -= 1;
        }
        // 2) on_edit 去抖（代数快照同 watch_plugin_panel：tab.id/edit_generation/revision）
        let generation = self.plugin_edit_generation();
        if self.plugin_edit_debouncer.poll(generation, now) {
            self.dispatch_plugin_edit(window, cx);
        }
        // 3) 到期定时器（预算共用）
        self.fire_due_plugin_timers(&mut budget, now, window, cx);
        // 4) 隔离检查
        self.enforce_plugin_quarantine(cx);
    }
```

`dispatch_plugin_event`：对 `self.plugin_host.listeners_of(name)` 逐插件构造
`CallInput { context_kind: Event, command_depth: 0, .. }` 调
`host.dispatch_event`，经 Task 11 的 `apply_plugin_call` 应用动作
（`plugin_event_depth += 1` 包裹）。
`dispatch_plugin_edit`：`listeners_of("on_edit")` + `context_kind: Edit`。
`fire_due_plugin_timers`：遍历活跃插件 `due_timer_ids` → `fire_timer`
（`context_kind: Event`）。
`enforce_plugin_quarantine`：对 `quarantine_due(id)` 的插件：置
`settings.disabled_plugins`、`reload_plugins(window, cx)`、
`set_status("plugin '{id}' disabled after repeated failures")`、
`log::error!`。

- [ ] **Step 4: 运行确认通过** — `cargo test -p luban-app`；手工冒烟见 Task 15。

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(plugins): 50ms event pump with edit debounce and timers"
```

### Task 11: HostAction 执行器（点击不清面板、args、cursor/settings/run/refresh）

**Files:**
- Modify: `crates/luban-app/src/app/plugin_ui.rs`
- Modify: `crates/luban-app/src/app/command_ui.rs`
- Test: `crates/luban-app/src/plugin_host.rs`（执行器逻辑中可单测的部分）

- [ ] **Step 1: apply_plugin_call 统一执行器**

把 `run_plugin_command`（`plugin_ui.rs:77-153`）的动作映射部分抽成
`apply_plugin_call(&mut self, call, depth, window, cx) -> bool`，完整实现：

```rust
    fn apply_plugin_call(
        &mut self,
        call: luban_plugin::runtime::RuntimeCall<()>,
        depth: u8,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.plugin_event_depth = self.plugin_event_depth.saturating_add(1);
        let mut ok = call.error.is_none();
        for action in call.output.actions {
            match action {
                HostAction::SetDocumentText { text, selection } => {
                    let (start, end) = selection.unwrap_or((0, 0));
                    self.replace_document_text(text, start, end, window, cx);
                }
                HostAction::Notify(message) => self.set_status(message, cx),
                HostAction::SetStatus { id, text } => {
                    self.plugin_host.set_status(/* plugin_id 需入参 */, &id, text);
                    cx.notify();
                }
                HostAction::OpenPanel { id } => { /* 原 OpenPanel 分支不变 */ }
                HostAction::RefreshPanel { id } => {
                    // 仅当该插件面板已打开时重渲染，否则忽略
                    if let Some(panel) = self.plugin_panel.as_ref() {
                        if panel.plugin_id == plugin_id && panel.panel_id == id {
                            self.refresh_plugin_panel(window, cx);
                        }
                    }
                }
                HostAction::SetCursor { line, col } => self.set_plugin_cursor(line, col, window, cx),
                HostAction::SetSetting { key, value } => {
                    self.apply_plugin_setting(plugin_id, &key, value, cx); // plugin_manager_ui.rs:9 同款三件套
                }
                HostAction::RunCommand { id, args, depth: run_depth } => {
                    if run_depth > luban_plugin::runtime::MAX_COMMAND_DEPTH {
                        self.set_status(format!("command '{id}' exceeded run depth"), cx);
                        ok = false;
                        continue;
                    }
                    if crate::commands::spec(&id).is_some() {
                        if args.is_some() { log::warn!("command '{id}' is built-in; args ignored"); }
                        self.plugin_event_depth += 1;
                        let ran = self.run_command(&id, window, cx);
                        self.plugin_event_depth -= 1;
                        ok &= ran;
                    } else if let Some(command) = self.plugin_host.command(&id).cloned() {
                        ok &= self.run_plugin_command(&command.plugin_id, &command.entry, &id, args, run_depth, window, cx);
                    } else {
                        self.set_status(format!("command '{id}' not found"), cx);
                        ok = false;
                    }
                }
            }
        }
        if let Some(error) = call.error {
            ok = false;
            self.set_status(error, cx);
        }
        self.plugin_event_depth = self.plugin_event_depth.saturating_sub(1);
        ok
    }
```

> `plugin_id` 需作为参数进入本函数（`run_plugin_command` 已有）；
> `set_plugin_cursor`：把 1-based line/col 换算为字符偏移（复用
> `plugin_document_snapshot` 反向的 rope 定位，`tabs.rs` 中
> `offset_to_position` 的对偶 `position_to_offset`，若无则新增于
> `plugin_ui.rs`），设置 `active_tab().doc.cursor` 并 `cx.notify()`。

- [ ] **Step 2: 点击行命令不清面板 + args 透传**

`plugin_ui.rs:1304` 行命令鼠标按下分支整体替换：

```rust
        match line.command.clone() {
            Some(command) => element
                .cursor_pointer()
                .hover(move |this| this.bg(colors.secondary))
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        // v2：面板保持打开、滚动位置保留、不抢焦点
                        if !this.run_command_with_args(&command.id, command.args.clone(), window, cx) {
                            this.set_status(
                                tr_args("status.command_failed", &[("command", command.id.clone())]),
                                cx,
                            );
                        }
                    });
                })
                .into_any_element(),
            None => element.into_any_element(),
        }
```

`run_command_with_args(id, args, window, cx)`：查
`self.plugin_host.command(id)`，命中插件命令则
`run_plugin_command(plugin_id, entry, id, args, /*depth*/ 0, window, cx)`，
未命中回落 `self.run_command(id, window, cx)`。行渲染处的 `line.command`
闭包捕获从 `String` 改为 `PanelCommand`（clone 进闭包）。

面板头部动作（`plugin_ui.rs` 中 `panel.actions` 渲染处，grep
`actions`）同样改传 `command.id` + `command.args`。

- [ ] **Step 3: run_plugin_command 签名扩展**

```rust
    pub(super) fn run_plugin_command(
        &mut self,
        plugin_id: &str,
        entry: &str,
        command_id: &str,
        args: Option<serde_json::Value>,
        command_depth: u8,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // CallInput { command_depth, context_kind: User, .. } 构造，
        // self.plugin_host.run_command(plugin_id, entry, command_id, args, input)
        // 之后改为 self.apply_plugin_call(call, command_depth, window, cx)
    }
```

既有调用点（菜单/键位/CLI 路径）传 `None, 0`。

- [ ] **Step 4: 运行确认通过** — `cargo test -p luban-app`；Xvfb 冒烟在 Task 15。

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(plugins): action executor, sticky panels with args, cursor and settings write-back"
```

### Task 12: 审批与插件管理 UI 的 v2 展示

**Files:**
- Modify: `crates/luban-app/src/app/plugin_manager_ui.rs`
- Modify: `crates/luban-app/src/app/plugin_ui.rs`（审批对话框所在处，grep `permissions` 定位）

- [ ] **Step 1: 审批对话框追加事件清单**

在审批内容渲染处（显示 permissions 的块之后）追加：

```rust
            if !manifest.events.is_empty() {
                dialog = dialog.child(shared::Label::new(
                    tr_args(
                        "plugins.approval.events",
                        &[("events", manifest.events.join(", "))],
                    ),
                ));
            }
```

i18n 文案加入 `crates/luban-app/locales/*`（跟随现有
`plugins.approval.*` 键所在文件，英文
`"This plugin will listen to: {{events}}"`，中文
`"此插件会监听：{{events}}"`；以 locales 目录实际结构为准）。

- [ ] **Step 2: LoadError 信息原样展示**

v1 插件拒载信息（Task 1 生成）已经过 `PluginHost::load` 进 `errors` 与
`PluginState::LoadError`，确认插件管理器 UI 对 `LoadError` 的展示路径
（`plugin_manager_ui.rs:80` 附近）无长度截断即可；若无截断则本步骤为零改动验证。

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat(plugins): show declared events in approval dialog"
```

### Task 13: 文档

**Files:**
- Rewrite: `docs/PLUGINS.md`
- Modify: `docs/ROADMAP.md`
- Modify: `CHANGELOG.md`

- [ ] **Step 1: PLUGINS.md 重写为 v2 参考**

保留现有章节骨架（目录/清单/Lua 入口/贡献点/权限），按 spec 更新：
manifest 示例用 `api_version: 2` + `events`；API 章节整体替换为 Task 3
签名一览表 + 错误约定两段；新增章节「事件」「定时器」「require」「面板 args」
「settings 写回」「command.run」「版本治理」（照抄 spec §6/§9/§8/§7/§5/§11 的
条款，改写为开发者视角）；删除 `luban.panel.version` 与 feature-detect 章节。

- [ ] **Step 2: ROADMAP 追加「不做清单」**

在 ROADMAP 合适小节追加：编辑器装饰、虚拟文档/只读 tab、文件监听/剪贴板事件
源、WASM/进程外隔离、选择性重建、插件脚手架生成器。

- [ ] **Step 3: CHANGELOG 增补 preview.17 未发布条目**（跟随现有格式）。

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "docs: plugin system v2 reference and roadmap"
```

---

## Phase 4 — 插件重写（v2 原生 2.0.0）

### Task 14: markdown 插件 v2.0.0

**Files（仓库 `/home/yangyn/workspaces/luban-notepad-plugin-markdown`）：**
- Modify: `plugin.json`、`main.lua`（装配层）
- Create: `parser.lua`、`renderer.lua`、`highlight.lua`
- Modify: `tests/run.lua`
- Create: `CHANGELOG.md` 条目、git tag `v2.0.0`

- [ ] **Step 1: 拆模块（函数清单已核实，按行号平移）**

- `parser.lua`：`normalize_text`(27)、`split_lines`(35)、`is_blank`(46)、
  `trim`(50)、`ltrim`(54)、`atx_level`(621)、`fence_info`(629)、`is_thematic`(641)、
  `is_setext_underline`(651)、`list_marker`(655)、`split_table_row`(679)、
  `table_aligns`(708)、`is_html_block_start`(745)、`is_table_start`(753)、
  `is_block_start`(765)、`parse_blocks`(791) 及其私有依赖；导出
  `M.parse(text, opts) -> blocks`。
- `inline.lua`（从 parser 内拆出）：`code_run_length`(214)、
  `skip_code_span`(222)、`find_matching_bracket`(243)、`find_close_paren`(268)、
  `parse_link_destination`(307)、`can_open_emphasis`(324)、
  `find_emphasis_close`(341)、`is_html_tag`(380)、`parse_inline_into`(397)。
- `renderer.lua`：`span_style`(982)、`to_panel_spans`(1005)、`make_line`(1034)、
  `split_spans_at_newlines`(1053)、`split_long_segment`(1079)、`push_line`(1110)
  及其后全部行装配函数；导出 `M.render(blocks, opts) -> { lines = ... }`。
- `highlight.lua`：封装 `luban.syntax.highlight` 调用与预算控制（原 opts 里的
  highlight 逻辑）。
- `main.lua` 保留：清单声明的 commands/panels 入口、缓存管理、装配
  `local parser = require('parser')` 等。

**迁移规则**：函数体平移不改逻辑；跨模块引用改为 `require`；两值/裸返回不涉及
（插件内部本来用 Lua 异常与返回值）。

- [ ] **Step 2: manifest 升 v2**

```json
{
  "id": "io.github.yyn0210.markdown-preview",
  "name": "Markdown Preview",
  "version": "2.0.0",
  "api_version": 2,
  "main": "main.lua",
  "description": "...",
  "author": "...",
  "permissions": [],
  "events": ["on_edit"],
  "contributes": { /* 既有命令/面板/设置原样保留 */ }
}
```

- [ ] **Step 3: on_edit 缓存 + 交互命令**

`main.lua` 增补：

```lua
local M = {}
local cache = { revision = nil, text_hash = nil, blocks = nil }

function M.on_activate(ctx)
  if luban.events then luban.events.on("on_edit", M.on_edit) end
end

local function hash_text(text)
  -- FNV-1a 32 位足够：文档变更检测
end

function M.on_edit(doc)
  -- 只更新缓存；渲染仍由面板入口返回（宿主 edit 刷新触发）
  cache.text_hash = doc and hash_text(doc.text) or nil
  cache.blocks = nil
end

local function ensure_blocks(doc)
  local h = doc and hash_text(doc.text) or nil
  if cache.blocks == nil or cache.text_hash ~= h then
    cache.blocks = parser.parse(doc.text, M.opts())
    cache.text_hash = h
  end
  return cache.blocks
end

function M.toggle_heading(ctx, args)
  -- args = { index = N }；M.state.collapsed[N] 取反；luban.panel.refresh('preview')
end

function M.goto_source(ctx, args)
  -- args = { line = N }；luban.document.set_cursor(args.line, 1)
end
```

渲染处为标题行与正文行附带
`command = { id = "markdown.toggle-heading", args = { index = N } }` /
`{ id = "markdown.goto-source", args = { line = N } }`；两个命令加入清单
`contributes.commands`（`entry: "toggle_heading"` / `"goto_source"`，不进
`menus`）。

- [ ] **Step 4: 测试**

`tests/run.lua` 的 luban stub 增补 `luban.events`/`luban.timer`/
`luban.document.set_cursor`/`luban.panel.refresh`/`require`（简单 dofile 版）；
新增用例：模块加载、on_edit 缓存失效、toggle_heading 折叠状态、
goto_source 的 cursor 调用记录。Run: `~/.local/lua54/usr/bin/lua5.4 tests/run.lua` → 全绿。

- [ ] **Step 5: 打包与发布**

`scripts/package.py`（无需改动，zip 已含多文件）→ 检查 zip 内容包含 4 个
模块文件；commit；`git tag v2.0.0 && git push origin v2.0.0`；GitHub release
自动出包后记录 sha256 与 size。

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat: rewrite for plugin API v2 (modules, on_edit cache, heading collapse, source jump)"
```

### Task 15: json-preview 插件 v2.0.0

**Files（仓库 `/home/yangyn/workspaces/luban-notepad-plugin-json-preview`）：**
- Modify: `plugin.json`、`main.lua`
- Create: `json/parser.lua`、`json/tree.lua`、`json/path.lua`、`json/meta.lua`
- Modify: `tests/run.lua`

- [ ] **Step 1: 拆模块（函数清单已核实）**

- `json/parser.lua`：`fail`(159)、`skip_ws`(163)、`is_digit`(178)、`is_hex`(182)、
  `parse_string`(187)、`parse_number`(291)、`parse_literal`(340)、
  `parse_object`(358)、`parse_array`(443)、`parse_json`(518)、
  `compute_line_starts`(543)、`line_number_at`(553)、`locate`(567)、
  `escape_codepoint`(594)；导出 `M.parse(text)`（保留范围/顺序/重复键语义）。
- `json/display.lua`：宽度与展示工具（27-155）+ `encode_string`(615)、
  `display_string`(626)、`display_number`(652)、`describe_node`(676)、
  `count_duplicates`(676 区域)、`ordered_pairs`(693)。
- `json/path.lua`：`trim`(726)、`shorten_path`(732)、`is_identifier`(743)、
  `append_key_path`(747)、`append_index_path`(754)、`child_paths`(760)、
  `array_child_paths`(769)、`walk_containers`(773) 与既有 JSONPath 解析/求值部分。
- `json/tree.lua`：折叠状态与树渲染：`clear_collapsed`(789)、`add_collapsed`(796)、
  `remove_collapsed`(807)、`document_key`(814)、`roots_signature`(823)、
  `foldable`(845)、`seed_fold_level`(854)、`ensure_fold_state`(868) 及其后渲染行
  函数。
- `json/meta.lua`：元信息统计（原 metadata 入口函数体）。
- `main.lua`：入口与装配。

- [ ] **Step 2: manifest 升 v2**

同 Task 14 Step 2 模式；`events: ["on_edit"]`；命令清单不变
（`json.fold.toggle`/`json.fold.all`/`json.expand.all`/`json.metadata`/
`json.path.show`/`json.path.extract` 等，以现清单为准）。

- [ ] **Step 3: 折叠状态主存内存 + 逐节点 args**

`json/tree.lua` 改造：

```lua
-- ensure_fold_state：激活时一次性从 data/state.json 读回（io 通过 luban.fs.read），
-- 之后折叠变更只改内存，并在变更时写穿（luban.fs.write('state.json', json)）
function M.toggle_fold_by_path(path)
  if is_collapsed(path) then remove_collapsed(path) else add_collapsed(path) end
  persist_fold_state()
end
```

树渲染为每个可折叠容器行附带
`command = { id = "json.fold.toggle", args = { path = fold_path } }`；
`json.fold.toggle(ctx, args)` 改为优先用 `args.path`（无 args 时保留光标法作
键盘路径）；面板行渲染不因折叠变化清空（宿主保证），入口末尾不再需要
`luban.panel.open` 补偿调用。

- [ ] **Step 4: 测试** — 同 Task 14 Step 4 模式；新增：模块拆分后 42+ 用例全绿、
  fold args 路由、状态写穿（stub fs 记录写入）。

- [ ] **Step 5: 打包与发布** — 同 Task 14 Step 5：tag `v2.0.0`、记录 sha256/size。

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat: rewrite for plugin API v2 (modules, in-memory fold state, per-node args)"
```

### Task 16: store 发布与端到端验证

**Files（仓库 `/home/yangyn/workspaces/luban-notepad-store`）：**
- Modify: `plugins/io.github.yyn0210.markdown-preview.json`、
  `plugins/io.github.yyn0210.json-preview.json`、`catalog/index.json`

- [ ] **Step 1: 宿主真机冒烟（先于 store 提交）**

构建宿主 debug 版并用 headless 环境驱动（参考
`LUBAN_NOTEPAD_CONFIG_DIR=/tmp/luban-v2-test DISPLAY=:99 luban-notepad`）：

1. 手工拷贝 markdown/json 2.0.0 到插件目录 + `.installed.json` 记录
   （空权限集 = SHA-256 of empty input）。
2. `--goto file --command markdown.preview` 打开预览 → 点击标题折叠路径的命令
   直调 `--command`：`--command "markdown.toggle-heading"`（args 仅面板点击
   才有，键盘路径用光标法仍可用）。
3. 编辑文档 → 观察 on_edit 去抖刷新（日志 `luban_plugin` target）。
4. v1 拒载验证：拷贝一份 `api_version: 1` 的旧插件 → 启动 → 插件管理器显示
   "api_version 1 is not supported..."，无崩溃。
5. `cargo fmt --all -- --check && cargo clippy --workspace -- -D warnings &&
   cargo test --workspace && cargo build --release`。

- [ ] **Step 2: store 条目更新**

编辑两份 `plugins/<id>.json` 追加 2.0.0 版本（sha256/size 取自 tag 包）；
`python3 build_catalog.py && python3 validate.py --download`；commit + push `main`。

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "release: plugins 2.0.0 for host api_version 2"
```

---

## Phase 5 — 宿主发布

### Task 17: 宿主版本发布 preview.17

**Files:**
- Modify: `Cargo.toml`（workspace version）、`CHANGELOG.md`

- [ ] **Step 1: 本地检查全绿**（fmt/clippy/test/build，同 Task 16 Step 1.5）
- [ ] **Step 2: 版本号与 CHANGELOG 定稿**，按记忆 release 流程：
  `git commit -m "Release 1.0.0-preview.17"` → tag `v1.0.0-preview.17` → push →
  CI 出全平台资产。
- [ ] **Step 3: 验收对照 spec §13 五条标准逐条核对**，结果记录到 CHANGELOG
  或验收说明。

---

## Self-Review 记录

- **Spec 覆盖**：spec §4 manifest/事件→Task 1/4/10；§5 API→Task 3/8；§6 事件规则→
  Task 4/10/11（级联抑制 = plugin_event_depth，只读 = CallContextKind::Edit）；§7
  面板→Task 7/8/11；§8 require→Task 6；§9 定时器→Task 5/10；§10 安全矩阵→
  Task 3（notify 帽）/4（隔离）/5（定时器钳制）/6（require 封闭）/7（args 帽）+
  既有防线不改动；§11 治理→Task 13；§12 插件→Task 14/15/16；§13 测试→各任务
  TDD 步骤 + Task 16/17 汇总；§1 延后项→Task 13 ROADMAP。
- **类型一致性**：`PanelCommand{id,args}` 全文一致；`CallContextKind{User,Event,Edit}`
  一致；`call_command(entry, command_id, args, input)` 新签名在 Task 8/9/11 对齐；
  `MAX_COMMAND_DEPTH` 定义于 Task 2、使用于 Task 3/11。
- **已知风险**：Task 4 的 `mlua::Function` 存储依赖既有 `error_handler` 字段先例，
  若编译器报生命周期错误，按 `plugin: Table` 同款处理（值持 Rc 句柄）；Task 10
  泵 spawn 点需 `window`，从 `reload_plugins(window, cx)`（有 window）与
  `LubanApp::new(window, ...)` 双路启动并以 `plugin_pump_started` 防重。

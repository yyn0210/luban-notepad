# 插件开发

Luban Notepad 1.0 的插件由一个 JSON 清单和一个沙箱化 Lua 文件组成。插件不能
链接 Rust 代码，也不能直接访问 `io`、`os`、`package`、`debug`、`require` 或
网络/子进程；所有能力都通过宿主提供的 `luban` API 暴露。

## 插件目录

```text
<config-dir>/plugins/
  .installed.json        # 宿主维护，插件不可写
  <plugin-id>/
    plugin.json
    main.lua
    data/                # 插件数据目录，首次写入时创建
```

配置文件目录由 `LUBAN_NOTEPAD_CONFIG_DIR` 覆盖；否则使用系统配置目录下的
`luban-notepad/`（Windows 便携版使用可执行文件旁的 `data/`）。

## 清单

```json
{
  "id": "com.example.hello",
  "name": "Hello",
  "version": "1.0.0",
  "api_version": 1,
  "main": "main.lua",
  "description": "Example plugin",
  "author": "Example",
  "permissions": ["filesystem"],
  "dependencies": [
    { "id": "com.example.base", "version": ">=1.0.0, <2.0.0" }
  ],
  "activate": "activate",
  "deactivate": "deactivate",
  "contributes": {
    "commands": [
      {
        "id": "hello.greet",
        "label": "Hello: Greet",
        "description": "Insert a greeting",
        "entry": "greet",
        "default_keybinding": "ctrl+alt+h",
        "when": "edit_text"
      }
    ],
    "menus": [{ "menu": "plugins", "command": "hello.greet" }],
    "status_bar": [
      { "id": "hello.status", "text": "Hello", "command": "hello.greet", "alignment": "left" }
    ],
    "settings": [
      {
        "key": "punctuation",
        "type": "string",
        "label": "Punctuation",
        "default": "!",
        "max_length": 8
      },
      {
        "key": "times",
        "type": "number",
        "label": "Repeat count",
        "default": 1,
        "min": 1,
        "max": 10,
        "step": 1
      },
      {
        "key": "mode",
        "type": "enum",
        "label": "Mode",
        "default": "normal",
        "values": ["normal", "loud"]
      }
    ],
    "panels": [
      { "id": "hello.list", "title": "Hello", "entry": "list", "placement": "right" }
    ]
  }
}
```

字段说明：

- `id`：全局唯一，匹配 `[a-z0-9][a-z0-9._-]*`，推荐反向域名风格。
- `version`：SemVer；`api_version` 必须不大于宿主支持的主版本（当前为 1）。
- `main`：相对插件目录的 `.lua` 文件，不能逃逸插件目录，最大 2 MiB。
- `dependencies`：按 `id` + SemVer 范围解析；缺失、不兼容或依赖环会禁用插件。
- `permissions`：`filesystem` 可用；`network`、`process` 可声明但本版本不提供
  对应 API。未知权限会被忽略并记录警告。
- `activate` / `deactivate`：插件表中的函数名，可选。

## Lua 入口

`main.lua` 必须返回一个表。命令、面板和生命周期函数按表键查找：

```lua
local M = {}

function M.activate(ctx)
  luban.log("info", "hello activated for " .. ctx.plugin.id)
end

function M.greet(ctx)
  local document = assert(luban.get_document())
  luban.insert_text("Hello" .. luban.settings.get("punctuation") .. "\n")
end

function M.list(ctx)
  return {
    title = "Hello",
    lines = {
      { text = "Greet", command = "hello.greet" },
      "Read-only line",
    },
  }
end

function M.deactivate(ctx)
end

return M
```

沙箱只开放基础库、`string`、`table`、`math` 和 `utf8`。每次回调最多执行
50,000,000 条 Lua 指令（约每 10,000 条检查一次），Lua 内存上限为 64 MiB；
超过限制会终止回调并在状态栏显示错误。回调通过 `xpcall` 执行，错误信息包含
Lua 栈回溯和出错位置；完整错误写入应用日志，状态栏保留至多 2 KiB 并在
截断时标注原始长度。

## 宿主 API

| API | 说明 |
| --- | --- |
| `luban.log(level, message)` | `debug`/`info`/`warn`/`error` 写入应用日志。 |
| `luban.notify(message)` | 非模态状态消息。 |
| `luban.json.encode(value)` | Lua 值转 JSON 字符串。 |
| `luban.json.decode(text)` | JSON 字符串转 Lua 值；JSON `null` 转为 `nil`。 |
| `luban.get_document()` | 返回 `{ path, text, language, line, column, selection_start, selection_end, modified }`，或 `nil, error`。 |
| `luban.set_document_text(text, start?, end?)` | 替换全文并按字符偏移设置选区。 |
| `luban.insert_text(text)` | 在光标/选区处插入或替换。 |
| `luban.replace_selection(text)` | 替换选区。 |
| `luban.settings.get(key)` | 读取插件设置。 |
| `luban.status.set(id, text)` | 更新已声明的状态栏项。 |
| `luban.fs.read(path)` | 读取文本文件（需要 `filesystem` 权限）。 |
| `luban.fs.write(path, text)` | 原子写入文件（需要 `filesystem` 权限）。 |
| `luban.data_dir()` | 返回并创建插件数据目录。 |

文档 API 只对普通、可编辑全文的文本文档有效。大文件、磁盘引擎、部分范围、
非文本文档和只读差异标签返回 `nil, "unsupported document"`，不会修改内容。
写入走现有保存/撤销/外部修改保护路径；插件不能绕过保存流程直接写当前文件，
除非显式使用 `luban.fs.write`。

`luban.fs` 的允许范围：所有已打开工作区根、当前活动文档所在目录（没有工作区
时）和插件自己的数据目录。路径在写入前规范化；超出范围的路径返回
`nil, "path outside allowed roots"`。单文件读取上限 64 MiB。

## 贡献点

### 命令

命令 ID 必须全局唯一，不能与内置命令或其他插件命令冲突；冲突的命令会被跳过
并记录错误。`entry` 是插件表中的函数名。`when` 可选：

- `always`（默认）
- `document`：有活动文档时才可用。
- `save`：文档可保存时才可用。
- `edit_text`：活动文档是可编辑全文时才可用。

`default_keybinding` 使用现有快捷键语法（例如 `ctrl+alt+h`）。无效绑定或与
内置默认绑定冲突时，该绑定被忽略并记录警告；命令仍可从命令面板执行。用户
可以在快捷键编辑器中覆盖插件命令；冲突检测与内置命令一致。插件命令可被宏
录制和回放。

### 状态栏

`status_bar` 项按声明顺序显示在左侧或右侧。`text` 是初始文本；插件可用
`luban.status.set(id, text)` 动态更新。`command` 可选，点击状态栏项执行该命令。

### 设置

支持 `boolean`、`string`、`number` 和 `enum`。设置值全局保存（不随工作区分开）
在 `settings.json` 的 `plugin_settings[<plugin-id>]` 下；缺失或非法值回退默认值。
插件在回调的 `ctx.settings` 和 `luban.settings.get` 中读取当前值。

### 面板

`panels` 由宿主渲染为模型态浮层（右缘或下缘）。`entry` 返回：

- 字符串数组：每项一行。
- `{ title = "...", lines = [ ... ] }`，行可以是字符串或
  `{ text = "...", command = "hello.greet" }`。

面板最多显示 5,000 行，超过时截断并提示。点击带 `command` 的行执行命令。
面板不会阻止编辑、保存或标签切换。

> API 版本 1 只接受 `menus[].menu = "plugins"`；插件命令统一放在“插件”菜单。

## 权限与批准

安装或更新插件时，管理器展示声明的权限并要求批准。批准结果（权限集合的
SHA-256 哈希）保存在 `<config-dir>/plugins/.installed.json`。权限集合变化后，
插件会变成“需要批准权限”并停止加载，直到用户重新批准。手动复制到
`plugins/` 的插件同样需要批准。

安全模式（`--safe-mode` 或设置中的“启用插件”关闭）不加载任何插件，也不执行
`activate`；用户语法、主题和片段仍然加载。

## 插件管理器

首选项 → 插件 → “管理插件”打开管理器：

- 列表显示名称、版本、作者、描述、ID、权限、状态和依赖错误。
- 启用/禁用、批准权限、卸载。卸载需要二次确认；如果其他已安装插件依赖它，
  确认框会列出这些插件，卸载后它们将因缺少依赖停止加载。
- 从本地目录、`.zip` 或 Git URL 安装；更新的插件默认保留数据和设置。
- 安装会覆盖同 ID 的已安装插件时，管理器先要求再次点击“安装”确认；不想替换时
  可使用“更新”从记录源重新安装。
- 从安装记录中的源更新（Git 重新 clone，本地目录重新复制，zip 优先读取记录的
  归档路径）。zip 安装时会把归档缓存到
  `<config-dir>/plugins/.sources/<id>.zip`，原归档被删除或移动后“更新”仍可用；
  卸载会删除该缓存。
- 安装/更新在临时目录中校验清单、API 版本、依赖和路径，再原子替换；失败不会
  留下半安装目录。zip/Git 中的绝对路径、`..` 穿越和逃逸符号链接会被拒绝。

插件在安装时不会执行代码；只有批准并启用后才会加载 Lua。

## 限制

- 不支持 WASM 插件、网络请求或子进程。
- 插件是单文件 Lua；不提供 `require`/多文件模块。
- 不提供任意 GPUI 视图注入；面板是宿主渲染的文本/列表浮层。
- 插件命令在 UI 线程同步执行；长时间计算受指令上限保护，但会阻塞界面直到
  回调返回。
- 插件设置是全局设置，不随工作区切换。

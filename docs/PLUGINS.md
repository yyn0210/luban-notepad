# 插件开发

Luban Notepad 的插件由一个 JSON 清单、一个沙箱化 Lua 入口和若干可选的 Lua
模块组成。插件不能链接 Rust 代码，也不能直接访问 `io`、`os`、`package`、
`debug` 或网络/子进程；所有能力都通过宿主提供的 `luban` API 暴露。本文描述
`api_version: 2` 的插件系统（事件驱动、命名空间化 API）；从 v1 迁移见文末
对照表。

## 插件目录

```text
<config-dir>/plugins/
  .installed.json        # 宿主维护，插件不可写
  <plugin-id>/
    plugin.json
    main.lua
    lib/…                # require 加载的模块，可选
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
  "api_version": 2,
  "main": "main.lua",
  "description": "Example plugin",
  "author": "Example",
  "permissions": ["filesystem"],
  "events": ["on_edit", "on_document_save"],
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
- `version`：SemVer。
- `api_version`：必须等于 `2`。宿主只加载相等值：v1 清单拒绝加载并明确提示
  更新到 v2 宿主版本；大于 2 的值同样拒绝（前瞻性校验）。
- `events`：声明式事件准入清单。运行时 `luban.events.on` 只允许注册这里声明过
  的事件；未知事件名拒绝加载。审批界面会展示“此插件会监听：…”。可用事件见
  下文“事件”一节。
- `main`：相对插件目录的 `.lua` 文件，不能逃逸插件目录，最大 2 MiB。
- `dependencies`：按 `id` + SemVer 范围解析；缺失、不兼容或依赖环会禁用插件。
- `permissions`：`filesystem` 可用；`network`、`process` 可声明但本版本不提供
  对应 API。未知权限会被忽略并记录警告。
- `activate` / `deactivate`：插件表中的函数名，可选。
- `contributes`：命令、菜单、状态栏、设置、面板贡献点，结构与 v1 相同。

## Lua 入口

`main.lua` 必须返回一个表。命令、面板和生命周期函数按表键查找：

```lua
local M = {}

-- Runs once per activation. Rehydrate everything a VM rebuild wiped:
-- event handlers, timers, caches.
function M.activate(context)
  luban.log("info", "hello activated for " .. context.plugin.id)
  luban.events.on("on_edit", M.on_edit)
  luban.timer.after("hello.welcome", 200, function(context)
    luban.notify("Hello ready")
  end)
end

function M.on_edit(context)
  local document = context.document
  if document == nil then
    return
  end
  luban.status.set("hello.status", document.line .. ":" .. document.column)
end

-- Command entries receive the panel-click args as the second parameter.
function M.greet(context, args)
  local name = (args and args.name) or "world"
  luban.document.insert("Hello, " .. name .. "!\n")
end

function M.list(context)
  return {
    title = "Hello",
    lines = {
      { text = "Greet", command = { id = "hello.greet", args = { name = "panel" } } },
      "Read-only line",
    },
  }
end

function M.deactivate(context)
end

return M
```

入口签名：

- 命令入口 `entry(context, args)`：`args` 是面板行/头部动作携带的参数表；没有
  参数时为 `nil`。注意 `args = {}`（空表）与 `nil` 不同。
- 事件处理器、定时器回调、面板入口与 `activate`/`deactivate` 都以
  `(context, nil)` 调用：第二参数固定为 `nil`，多余的形参被忽略，单参数入口
  保持可用。事件载荷（文档快照表）通过 `context.document` 传递。

`context` 表字段：

- `context.plugin`：`{ id, version, data_dir }`。
- `context.command`：当前命令 ID（仅命令入口有）。
- `context.document`：文档快照表 `{ path, text, language, line, column,
  selection_start, selection_end, modified }`，无活动文档时为 `nil`。
- `context.settings`：当前插件设置值。

生命周期：`main.lua` 在每次激活时加载一次。VM 状态（全局变量、事件处理器、
定时器、`require` 模块缓存）在同一个插件计划构建内跨分发保留；重新加载、
启用/禁用、安装/更新或应用重启都会重建 VM 并清零全部状态——插件必须在
`activate()` 里重水合：注册事件处理器、重建定时器、恢复缓存。

沙箱只开放基础库、`string`、`table`、`math` 和 `utf8`（`require` 是宿主提供的
受控实现，见下文）。每次分发最多执行 50,000,000 条 Lua 指令（约每 10,000 条
检查一次），Lua 内存上限为 64 MiB；超过限制会终止当前分发并计入连续失败隔离。
分发通过 `xpcall` 执行，错误信息包含 Lua 栈回溯和出错位置；完整错误写入应用
日志，状态栏保留至多 2 KiB 并在截断时标注原始长度。

## 宿主 API

| API | 语义 |
| --- | --- |
| `luban.version` | 宿主特性等级 = 2；加性演进只升这个值，见“版本治理”。 |
| `luban.log(level, message)` | `debug`/`info`/`warn`/`error` 写入应用日志。 |
| `luban.notify(message)` | 状态栏非模态提示；每分发最多 8 条、单条 1 KiB，超限静默丢弃并记录告警日志。 |
| `luban.json.encode(t)` / `luban.json.decode(s)` | Lua 值 ↔ JSON 字符串；失败抛错。encode 拒绝超过 128 层嵌套与循环表；JSON `null` 解码为 `nil`。 |
| `luban.document.get()` | 文档快照表（同 `context.document`）；无文档返回 `nil`。 |
| `luban.document.set_text(text, sel_start?, sel_end?)` | 全文替换并按字符偏移设置选区；两个偏移必须同时给或同时省。 |
| `luban.document.insert(text)` | 在光标/选区处插入（选区被替换），光标移到插入文本之后。 |
| `luban.document.set_cursor(line, col)` | 1-based 光标定位，宿主钳制到文档边界。 |
| `luban.settings.get(key)` / `luban.settings.set(key, value)` | `get` 读取当前值；`set` 仅限已声明键，类型/范围校验，立即持久化且同一分发内读后写可见。 |
| `luban.status.set(id, text)` | 更新已声明的状态栏项。 |
| `luban.syntax.highlight(text, lang)` | 用当前语法主题高亮，返回 `{ text, role }` 数组；单次上限 64 KiB。 |
| `luban.fs.read(path)` / `luban.fs.write(path, text)` | 需要 `filesystem` 权限并受允许根目录限制；读上限 64 MiB，写为原子写。 |
| `luban.data_dir()` | 返回并创建插件数据目录路径。 |
| `luban.panel.open(id)` / `luban.panel.refresh(id)` | `open` 打开自己声明的面板（已打开则保留滚动位置）；`refresh` 仅在面板已打开时重渲染，关闭状态是静默 no-op。 |
| `luban.command.run(id, args?)` | 运行任意命令；`args` 传给目标插件命令入口；链深最多 4，见“command.run”。 |
| `luban.events.on(name, fn)` / `luban.events.off(name)` | `fn` 为函数值；`on` 仅限清单 `events` 声明过的事件，每插件最多 32 个处理器；`off` 移除该事件全部处理器。 |
| `luban.timer.after(id, ms, fn)` / `luban.timer.every(id, ms, fn)` / `luban.timer.cancel(id)` | 单次/循环定时器，见“定时器”。 |

文档 API 只对普通、可编辑全文的文本文档有效。大文件、磁盘引擎、部分范围、
非文本文档和只读差异标签抛出 "unsupported document" 错误，不会修改内容。
写入走现有保存/撤销/外部修改保护路径；插件不能绕过保存流程直接写当前文件，
除非显式使用 `luban.fs.write`。

`luban.fs` 的允许范围：所有已打开工作区根、当前活动文档所在目录（没有工作区
时）和插件自己的数据目录。路径在写入前规范化；超出范围的路径抛出
"path outside allowed roots" 错误。

## 错误约定

v2 用三类约定取代 v1 的 `(nil, err)` 双值返回：

- **校验类错误 fail-fast**：参数非法（`set_text` 只给一个偏移、`set_cursor`
  传 0 等）、未声明键（`status.set`/`settings.set` 的未知 ID/键、`events.on`
  注册未声明事件）、超出编程性上限（定时器超过 16 个、事件处理器超过 32 个、
  `command.run` 深度超过 4）、在 `on_edit` 上下文写文档——一律 `error()` 抛出。
  宿主经 pcall + traceback 捕获并计入连续失败隔离。
- **资源类超限静默丢弃**：notify 洪泛、面板 args 超过 4 KiB、面板行数/行宽帽
  ——丢弃超出部分并记录告警日志，不中断插件，命令照常执行。
- **变更类函数返回 nil**：mutator（`document.set_text`/`insert`/`set_cursor`、
  `status.set`、`panel.open`/`refresh`、`fs.write`、`settings.set`、
  `command.run`、事件与定时器注册）成功只返回 `nil`；getter 返回值；失败抛错。

插件代码里 `pcall` 捕获到的宿主错误对象是 userdata（消息在 `__tostring`
之后），显示经 `tostring` 转换。错误文本不是 API 契约：不要对宿主错误消息做
模式匹配，按“是否抛错”分支即可；插件自己 `error("message")` 抛出的字符串
错误仍然是字符串。

## 事件

| 事件 | 触发时机 | 载荷（`context.document`） |
| --- | --- | --- |
| `on_document_open` | 文档标签打开 | 文档快照表 |
| `on_document_save` | 保存完成 | 文档快照表 |
| `on_active_document_changed` | 活动 tab 切换/关闭 | 文档快照表或 `nil` |
| `on_edit` | 编辑停止约 150ms 后（去抖，与面板 `refresh: "edit"` 同源） | 文档快照表 |

- 注册：`luban.events.on(name, fn)`，`fn` 为函数值（不是函数名字符串）；只能
  注册清单 `events` 声明过的事件；每插件处理器总数不超过 32。同一事件的多个
  处理器按注册顺序运行，共享同一次分发的指令预算；单个处理器失败不会阻止其余
  处理器，整次分发计为一次失败。`luban.events.off(name)` 移除该事件的全部
  处理器。
- **只读规则**：`on_edit` 上下文中 `luban.document.set_text` / `insert` 抛出
  只读错误——从根上掐断“编辑 → 事件 → 编辑”风暴；其余 API 不受限。
- **级联抑制与上下文继承**：事件/定时器上下文内产生的一切动作不再发射事件；
  `luban.command.run` 链继承当前分发的上下文——`on_edit` 链上的嵌套命令同样
  只读，`on_save` 等事件链可编辑但不再触发事件（formatter-on-save 合法），
  反馈环不可能出现。
- **预算**：事件泵每 50ms 走一帧，每帧最多 8 次分发（事件与到期定时器共享）；
  每次分发与命令一样有 50M 指令 + 64 MiB 预算。

## 定时器

```lua
luban.timer.after(id, ms, fn)   -- one-shot
luban.timer.every(id, ms, fn)   -- repeating
luban.timer.cancel(id)          -- remove; unknown ids are a no-op
```

- 每插件最多 16 个活动定时器；重复 id 覆盖旧条目。
- 最小间隔 50ms，低于钳制到 50ms；负值抛错；超大间隔可以注册但永不触发，
  仍占一个槽位直到 `cancel` 或 VM 重建。
- 回调签名 `fn(context)`：以 `(context, nil)` 调用，多余参数忽略。定时器始终
  在事件上下文运行——文档/命令动作可用，但不发射事件（级联抑制）。
- 到期定时器与事件共用同一条分发队列（每帧最多 8 次分发），同等指令预算、
  错误隔离与级联抑制。
- VM 重建后定时器清零；在 `activate()` 里重建，与事件注册同模式。

## 多文件 require

沙箱内的 `require("lib.parser")` 加载并执行 `<plugin-dir>/lib/parser.lua`：

- 模块名只能包含 `[a-z0-9._]`，按点分隔、每段非空、总长最多 256 字节；含
  `..` 段、绝对路径或逃逸插件目录的名字全部拒绝。作为纵深防御，解析出的
  父目录与文件本身都做符号链接检查，目标必须在插件目录内。
- 模块与 `main.lua` 在同一个 Lua 状态里：同一沙箱、共享 globals；`package`
  库仍然不可见。模块缓存存活期 = VM 存活期；模块代码在发起 require 的分发里
  执行，计入该次分发的指令预算。
- 清单与打包零改动：zip 里多带 `.lua` 文件即可。

注意事项：

- `require('main')` 会把 `main.lua` 当模块再次求值——顶层副作用跑两遍。
- 返回 `nil` 的模块按 `nil` 缓存（标准 Lua 的 require 返回 `true`）：副作用
  模块应显式返回一个表或 `true`。
- 循环依赖抛出 "circular dependency" 错误。

## 贡献点

### 命令

命令 ID 必须全局唯一，不能与内置命令或其他插件命令冲突；冲突的命令会被跳过
并记录错误。`entry` 是插件表中的函数名，以 `entry(context, args)` 调用。
`when` 可选：

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
插件在 `context.settings` 和 `luban.settings.get` 中读取当前值；写回用
`luban.settings.set`，语义见“settings 写回”。

## 面板

面板由清单 `contributes.panels` 声明，宿主渲染为模型态浮层（右缘或下缘）。
清单支持两个可选字段：`width`（右侧面板宽度，280–1200，缺省 380，宿主会再按
窗口宽度收敛）和 `refresh`（`manual` 缺省，或 `edit`——面板打开期间跟随活动
文档的编辑和标签切换自动重算，编辑停止约 150ms 后刷新，面板关闭即停止）。

面板入口 `entry(context)` 返回字符串、字符串数组（每项一行）或
`{ title = "...", lines = [ ... ], actions = [ ... ] }`。行可以是字符串或：

```lua
{
  text = "纯文本回退",              -- 必填
  style = "text",                  -- text|muted|h1..h6|code|quote|list|table|rule|image
  indent = 0,                      -- 0..8
  spans = {                        -- 可选；拼接后必须等于 text
    { text = "粗体", style = "bold" },
    { text = "fn", role = "keyword" },
  },
  command = { id = "json.fold.toggle", args = { path = "$.a[0]" } },
  image = "docs/logo.png",          -- style = "image" 时的本地图片源
  tight = false,                   -- true：段内续行，不追加段间距
  break_before = false,            -- true：同种块的新块起点
  cells = {                        -- style = "table" 时的结构化单元格
    { text = "名称", header = true, weight = 8, align = "left" },
    { text = "42", align = "right", weight = 3 },
  },
}
```

### 块级渲染

宿主把连续的同类行合并成块控件，而不是逐行当作文本：

- 连续的 `style = "code"` 行渲染为一个圆角代码卡片；`break_before = true`
  开启下一张卡片（用于相邻的围栏代码块），不会插入空行。
- `style = "table"` 且 `cells` 非空的行渲染为真实表格：连续行合并为一张表，
  表头行带底色并与内容行画分隔线，偶数内容行带斑马纹。`cells` 中每项是
  字符串或 `{ text, spans?, align?, header?, weight? }`：
  - `align`：`left`（默认）、`center`、`right`。
  - `header`：表头单元格，加粗并置于表头行。
  - `weight`：列宽权重，1–32，默认 1；按内容最宽的列分配面板宽度。
  - 每行最多 32 个单元格；单元格 `text` 与行一样受 4 KiB 上限约束。
  - `text` 仍是必填的回退文本；老宿主忽略 `cells`，按等宽 `table` 行渲染。
- `tight = true` 用于同一段落内的硬换行或超长行拆分：宿主去掉该行底部
  段间距，使换行不产生段落间隙。
- `style = "image"` 行渲染本地图片：`image` 是相对于当前文档目录的路径（也接受
  目录内的绝对路径），宿主只读取规范化后仍位于文档目录内、且扩展名受图片解码器
  支持的文件；`text` 作为 alt 文本。远程 URL、`data:`、缺失文件与越界路径按
  alt 文本回退，不发起网络请求。
- 行命令与代码/表格内容可以同时使用；点击整行仍执行 `command`。

插件可读取只读能力表 `luban.features` 做渐进增强（缺失或 `false` 表示老宿主）：

| 字段 | 含义 |
| --- | --- |
| `panel_tables` | 面板支持 `cells` 结构化表格行 |
| `panel_tight_lines` | 面板识别 `tight` 续行标志 |
| `panel_images` | 面板渲染 `style = "image"` 的本地图片行 |

`command` 统一为 `{ id, args }` 表（字符串形态已移除）：`id` 是要执行的命令，
`args` 是 JSON 兼容的 Lua 表，宿主作为目标命令入口的第二参数传入。规则：

- `args` 序列化后超过 4 KiB 时丢弃 args 并记录告警日志，命令仍执行——处理器
  必须容忍 `nil` args。
- `args = {}` 传空表，与省略 `args`（得到 `nil`）不同。
- 点击带 `command` 的行执行命令；**点击行命令与头部动作都不清面板、滚动位置
  保留**。面板不会阻止编辑、保存或标签切换。

可选 `actions`：在面板标题旁显示一排扁平操作按钮，不占用正文行：

```lua
return {
  title = "解析结果",
  lines = { ... },
  actions = {
    { label = "折叠", command = { id = "json.fold.toggle-all" } },
    { label = "元数据", command = { id = "json.metadata" } },
  },
}
```

最多 8 个操作，`label` 不超过 64 字节，`command` 同样是 `{ id, args }` 表。

span 的 `style` 是空格分隔的标志：`bold`、`italic`、`underline`、`strike`、
`code`、`link`、`muted`；`role` 使用 `luban.syntax.highlight` 返回的语义角色，
宿主用当前语法主题着色。spans 总字节不超过 4 KiB，且拼接必须等于 `text`，
否则整行按 `text` 渲染。

容量帽：最多 5,000 行、单行 4 KiB、512 个 spans、每行 32 个表格单元格、
8 个操作，超出时截断或拒绝并提示。

“插件”菜单按插件分组：一级只显示插件名称（清单 `name`），展开后先列出
`contributes.menus` 中声明的命令，再列出该插件的面板。菜单只接受
`menus[].menu = "plugins"`。未声明的命令仍可从命令面板、快捷键或面板行执行；
没有菜单命令也没有面板的插件不会出现在菜单中。面板标题与同一子菜单中已显示
命令的标签同名时（忽略大小写、空格和标点），只显示命令，不重复显示该面板。

## settings 写回

`luban.settings.set(key, value)`：

- 仅限清单 `contributes.settings` 声明过的键；未知键抛错。
- 值按声明 schema 校验类型与范围（`string` 的 `max_length`、`number` 的
  `min`/`max`/`step`、`enum` 的 `values`），非法值抛错。
- 校验通过后立即持久化到 `settings.json` 的 `plugin_settings[<plugin-id>]`，
  首选项里的插件设置控件同步更新；同一分发内 `set` 之后的 `get` 立即返回
  新值（读后写可见）。

## command.run

`luban.command.run(id, args?)` 运行任意命令（内置或插件命令）：

- `args`（可选表）转换为 JSON 后传给目标插件命令入口的 `entry(context, args)`；
  内置命令忽略 args 并记录告警日志。
- 深度限制：用户、CLI（`--command`）或面板点击发起的命令是深度 0，链上每跳
  +1，最多 4 层；超出抛错。解析顺序 plugin-first，与用户路径一致：插件命令
  可以遮蔽同 ID 的内置命令，被遮蔽的内置命令不会绕过深度帽。
- 在事件/定时器上下文中发起时，整条链继承该上下文（只读与级联抑制规则见
  “事件”一节）。

## 权限与批准

安装或更新插件时，管理器展示声明的权限并要求批准；同时展示清单 `events`
（“此插件会监听：…”）。批准结果（权限集合的 SHA-256 哈希）保存在
`<config-dir>/plugins/.installed.json`。权限集合变化后，插件会变成“需要批准
权限”并停止加载，直到用户重新批准。手动复制到 `plugins/` 的插件同样需要批准。

## 隔离与安全

- 沙箱库集不变：基础库、`string`、`table`、`math`、`utf8`；不提供 `io`、
  `os`、`package`、`debug` 与动态代码加载。每次分发 50M 指令 + 每 VM
  64 MiB 内存。
- **连续失败隔离**：任一插件连续 10 次分发失败（命令/事件/定时器合计，任一
  成功即清零）→ 自动停用 + 一次状态栏通知，写入禁用列表持久化；在插件管理器
  重新启用即可恢复（计数清零）。
- 安全模式（`--safe-mode` 或设置中的“启用插件”关闭）不加载任何插件，也不执行
  `activate`；用户语法、主题和片段仍然加载。
- `api_version: 1` 的插件拒绝加载并提示更新到 v2 宿主版本。

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

## 插件商店

插件管理器顶部有“已安装 / 商店”两个页签。商店从目录仓库读取插件元数据，支持
搜索、查看详情、安装和更新；也可以用“插件 → 插件商店…”或命令 `plugins.store`
直接打开。

- 目录地址默认指向 `yyn0210/luban-notepad-store` 的 `catalog/index.json`，可在
  商店页修改或重置；支持 HTTPS、`file://` 和本地绝对路径。
- 目录在后台获取并缓存到 `<config-dir>/plugins/.store/`；网络失败时显示同源
  缓存和错误，已安装插件不受影响。
- 只展示当前 OS/CPU 有可用产物的插件；作者可以发布 `universal` 包或只覆盖部分
  平台。产物优先精确匹配当前目标，缺失时回退 `universal`。
- 下载后校验目录声明的 SHA-256 和大小，再交给现有 zip 安装事务；权限批准、
  依赖校验和安全模式与手动安装完全一致。
- 更新保留插件数据和设置；权限集合变化时重新进入需要批准状态。

目录格式、目标平台、缓存位置、发布流程和故障排查见
[docs/PLUGIN_STORE.md](PLUGIN_STORE.md)。

## 版本治理

- `api_version`：破坏性主版本。宿主只加载相等值；Lua API 面的不兼容变更必须
  升它。
- `luban.version`：加性特性等级（当前为 2）。新增 API、事件或贡献点只升它，
  老宿主可安全忽略更高的值，插件探测后自行降级。
- 插件探测加性能力：

```lua
if (luban.version or 1) >= 2 then
  -- use the newer additive capability
end
```

## 从 v1 迁移

| v1 | v2 |
| --- | --- |
| `"api_version": 1` | `"api_version": 2`；宿主拒载 1 并提示更新。 |
| `luban.get_document()` | `luban.document.get()` |
| `luban.set_document_text(text, start?, end?)` | `luban.document.set_text(text, sel_start?, sel_end?)` |
| `luban.insert_text(text)` | `luban.document.insert(text)` |
| `luban.replace_selection(text)` | 移除；用 `set_text` / `insert` 表达。 |
| `(nil, "error")` 双值返回 | 校验类 `error()` 抛错；mutator 成功返回 `nil`。 |
| `command = "hello.greet"`（面板行/操作） | `command = { id = "hello.greet", args = {...} }`。 |
| `luban.panel.version` feature-detect | 移除；v2 宿主均支持 actions/宽度/自动刷新。 |
| 无事件/定时器/require | 清单声明 `events`；在 `activate()` 里注册处理器与定时器；`require` 拆模块。 |

迁移时同步删除全部 feature-detect 与旧宿主降级分支：v2 宿主保证本文描述的
全部 API 存在，加性能力用 `(luban.version or 1) >= 2` 探测。

## 限制

- 不支持 WASM 插件、网络请求或子进程。
- 事件源目前只有文档生命周期（打开/保存/活动切换/编辑去抖）；文件监听、
  剪贴板等新事件源已延期，见 `docs/ROADMAP.md`。
- 不提供任意 GPUI 视图注入；面板是宿主渲染的富文本行浮层（标题、span、代码、
  引用、表格等），不能嵌入自定义控件或 HTML。
- 面板里的链接不可点击；需要交互的行只能绑定带 args 的命令。
- 插件命令在 UI 线程同步执行；长时间计算受指令上限保护，但会阻塞界面直到
  回调返回。
- 插件设置是全局设置，不随工作区切换。

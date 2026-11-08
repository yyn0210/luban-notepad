# 插件系统 v2 设计（破坏性重设计：事件驱动 + 交互升级）

- 日期：2026-11-08（修订版，替代同日首版草案）
- 状态：待用户审阅
- 基线：v1.0.0-preview.16（main @ 76e2bf8），设计 spec @ f780cd9
- 关联文档：`docs/PLUGINS.md`、`docs/PLUGIN_STORE.md`、`docs/ROADMAP.md`

## 0. 修订说明（相对首版草案）

用户确认：**历史插件可以不兼容，与旧稳定性冲突的地方直接改**。据此本版把首版的
"v1 兼容层"全部移除，改为干净的 v2 破坏性重设计：

1. `api_version` 升为 **2**，宿主只加载 2；v1 插件给出明确的不可用提示。
2. Lua API 面**按命名空间重组**（`luban.document.*` 等），不再在扁平 v1 API 上叠加。
3. 错误约定从 `(nil, err)` 两值返回改为 **fail-fast `error()`**（校验类）+ 静默
   丢弃（资源类），插件代码更简洁。
4. 面板行命令统一为表 `{ id, args }`，删除字符串/表双形态解析。
5. 事件改为**清单声明式**（`events` 字段为准入清单，运行时注册校验）。
6. 双预览插件 2.0.0 为 v2 原生重写，**删除全部 feature-detect 与 legacy 降级模块**。
7. 新增版本治理策略（api_version = 破坏性主版本，luban.version = 加性特性等级）。

保留首版结论：常驻 VM（已存在）、全量重建语义 + activate() 重水合（选择性重建
仍延后）、安全矩阵、事件集与只读/级联规则、定时器、settings 写回。

## 1. 背景与目标

1.0 已交付 Lua 插件平台（manifest + 沙箱化 Lua + 贡献点 + 插件商店）。两个预览
插件的开发暴露出一批结构性痛点：

- 面板行命令无参数、点击即清空面板并丢失滚动位置，插件只能用"光标位置"做交互
  的 workaround；
- 插件状态在插件计划重建（reload、启停/装卸、应用重启）时全部丢失，JSON 插件
  被迫用 data 目录序列化维持折叠状态；
- 无事件系统，自动刷新依赖 180ms 轮询；无 require，单文件已膨胀到约 2,950 行；
- settings 只能读不能写；无定时器；无光标控制；错误约定样板代码多。

本期目标：**最稳定**（沙箱与防线强化、边界全部有测试）、**最超前**（事件驱动、
命名空间化 API、明确版本治理）、**开发便捷**（fail-fast、模块化、无兼容包袱）。

## 2. 现状关键事实（设计依据）

- `PluginRuntime` 持有常驻 Lua 状态：`main.lua` 仅在 `PluginRuntime::new()` 加载
  一次，`invoke()` 在持久 `plugin` 表上调用入口函数（`runtime.rs:322-383`）。
- `PluginHost` 为全量重建（`plugin_host.rs`）：任意插件计划变更重建全部
  runtime/commands/panels/settings。
- 沙箱：库集 base/string/table/math/utf8；每分发指令预算 50,000,000（invoke 前
  重置）；每 VM 内存 64 MiB；错误经 traceback 截断回传。
- 权限：`filesystem` 唯一实现权限，`process`/`network` 保留声明位。
- 商店当前仅两个插件（markdown、json-preview），且均为本用户维护——升级断层
  窗口为零。

## 3. 总体架构

```
┌─ 主应用 update loop ─────────────────────────────────────────┐
│  PluginHost                                                  │
│  ├─ runtimes: 每插件一个常驻 PluginRuntime（不变）             │
│  ├─ 事件队列: AppEvent { DocumentOpened, DocumentSaved,      │
│  │     ActiveDocumentChanged, EditDebounced }                │
│  │     → 每帧最多泵 8 个分发，到期定时器走同一条队列            │
│  └─ 重建语义: 全量重建保留；activate() 重水合状态              │
└──────────────────────────────────────────────────────────────┘
```

- 不改：沙箱库集、权限模型、store/安装、菜单（每插件一级子菜单）、面板渲染核心、
  safe-mode。
- 改动落点：`luban-plugin/src/runtime.rs`（API 重排、事件注册表、定时器表、沙箱
  require、args 传参、HostAction 扩展）、`luban-plugin/src/manifest.rs`（v2 校验、
  events 字段）、`luban-app/src/plugin_host.rs`（事件泵、定时器队列、隔离计数）、
  `luban-app/src/app/plugin_ui.rs`（审批展示 events、隔离恢复入口）、
  `docs/PLUGINS.md` 重写。

## 4. v2 清单（manifest）

```json
{
  "id": "io.github.yyn0210.json-preview",
  "name": "JSON Preview",
  "version": "2.0.0",
  "api_version": 2,
  "main": "main.lua",
  "description": "...",
  "author": "...",
  "permissions": ["filesystem"],
  "events": ["on_edit", "on_document_save"],
  "dependencies": [],
  "activate": "on_activate",
  "deactivate": "on_deactivate",
  "contributes": {
    "commands": [ { "id": "...", "label": "...", "entry": "...",
                    "default_keybinding": "...", "when": "edit_text" } ],
    "menus": [ { "menu": "plugins", "command": "..." } ],
    "status_bar": [ { "id": "...", "text": "...", "alignment": "left" } ],
    "settings": [ { "key": "...", "type": "enum", "default": "...",
                    "values": ["..."] } ],
    "panels": [ { "id": "...", "title": "...", "entry": "...",
                  "placement": "right", "width": 380, "refresh": "edit" } ]
  }
}
```

- `api_version` 必须 `= 2`：`=1` 拒绝加载，错误信息明确提示"请更新插件到 v2 宿主
  版本"；`>2` 同样拒绝（前瞻性校验，行为不变）。
- `events`：声明式事件准入清单。运行时 `luban.events.on` 只允许注册此处声明过的
  事件；审批界面（安装/更新权限弹窗）展示"此插件会监听：编辑、保存"。未知事件
  名拒绝加载。
- `activate`/`deactivate`/`contributes` 各贡献点结构与 v1 相同（命令、菜单、状态
  栏、设置、面板），含 width/refresh/when 等既有字段。

## 5. Lua API v2（命名空间化）

```lua
luban.version                       -- 宿主特性等级 = 2（加性演进用）
luban.log(level, message)
luban.notify(message)               -- 每分发 ≤8 条、单条 1 KiB，超限丢弃并告警
luban.json.encode(table) / decode(text)
luban.document.get()                -- → 快照表；无文档返回 nil
luban.document.set_text(text, sel_start, sel_end)
luban.document.insert(text)
luban.document.set_cursor(line, col)          -- 1-based，越界钳制
luban.settings.get(key) / set(key, value)     -- set 仅限已声明键，类型/范围校验，
                                              -- 立即持久化并更新运行时值
luban.status.set(id, text)
luban.syntax.highlight(text, language)        -- 64 KiB 帽不变
luban.fs.*                                    -- 既有函数，permission-gated 不变
luban.panel.open(id) / refresh(id)
luban.command.run(id, args?)                  -- args 传给目标插件命令入口；
                                              -- 用户/CLI/面板点击发起深度 0，链上 +1，≤4
luban.events.on(name, fn) / off(name)         -- fn 为函数值；off 移除该事件全部
luban.timer.after(id, ms, fn) / every(id, ms, fn) / cancel(id)
```

- 入口签名统一：命令/面板入口 `entry(context, args)`；事件处理器 `fn(payload)`；
  定时器回调 `fn()`。v1 的 `get_document`/`set_document_text`/`insert_document`
  收拢为 `luban.document.*`；新 API 一律接受函数值（不再用函数键字符串）。
- `luban.panel.version` 废弃（v2 起面板 actions/宽度/自动刷新为基础能力）。

### 错误约定（新）

- **校验类错误 fail-fast**：参数非法、未声明键、超出编程性上限（定时器 >16、
  事件注册 >32、command.run 深度 >4）、`on_edit` 上下文写文档 → `error()` 抛出，
  经既有 pcall + traceback 捕获，计入连续失败隔离。
- **资源类超限静默**：notify 洪泛、panel args >4 KiB（丢弃 args 继续执行命令）、
  行数/行宽帽 → 丢弃 + 告警日志，不中断插件。
- 依据：编程错误应当立刻暴露（开发便捷），资源防护不应放大成插件崩溃（稳定）。

## 6. 事件系统

| 事件 | 触发时机 | payload |
|---|---|---|
| `on_document_open` | 文档 tab 打开 | 文档快照表 |
| `on_document_save` | 保存完成 | 文档快照表 |
| `on_active_document_changed` | 活动 tab 切换/关闭 | 文档快照表或 nil |
| `on_edit` | 编辑后 150ms 稳定去抖（与面板刷新同源信号） | 文档快照表 |

- **只读规则**：`on_edit` 上下文中 `luban.document.set_text` / `insert` 抛出
  只读错误——从根上掐断"编辑→事件→编辑"风暴。
- **级联抑制**：事件/定时器上下文内产生的一切动作（文档编辑、`command.run`）
  不再发射任何事件。formatter-on-save 合法，反馈环不可能出现。
- 预算：每分发 50M 指令 + 64 MiB，与命令一致。
- **连续失败隔离**：任一插件连续 10 次分发失败（命令/事件/定时器合计，任一成功
  清零）→ 自动停用 + 一次通知，插件菜单可手动恢复。

## 7. 面板交互 v2

- `PanelLine.command` 与 `PanelAction.command` 统一为表：
  ```lua
  command = { id = "json.fold.toggle", args = { path = "$.a[0]" } }
  ```
  args 为 JSON 兼容 Lua 表，宿主转换为入口第二参数 `entry(context, args)`。
- **点击行为**：点击行命令/面板动作不清空面板、滚动位置保留（宿主不再置
  `plugin_panel = None`；重渲染沿用 `luban.panel.open` 的滚动句柄复用）。
- `luban.panel.refresh(id)`：插件主动请求重渲染自己的面板，配合定时器实现
  推送式更新。
- args 上限 4 KiB：超限丢弃 args + 告警，命令仍执行。

## 8. 多文件 require

- 沙箱内 `require("lib.parser")` → `<plugin-dir>/lib/parser.lua`。
- 规则：仅相对模块路径（`[a-z0-9._/-]+`），拒绝绝对路径与含 `..` 的路径，解析
  结果不得逃逸插件目录；`package` 库仍不可见；模块缓存存活期 = VM 存活期；
  加载/执行耗时计入当前分发指令预算。
- 清单与打包零改动：zip 内多带 .lua 文件即可。

## 9. 定时器

```lua
luban.timer.after(id, ms, fn)   -- 单次
luban.timer.every(id, ms, fn)   -- 循环
luban.timer.cancel(id)
```

- 每插件 ≤16 个活动定时器；最小间隔 50ms（低于钳制）；重复 id 覆盖。
- 到期入事件泵同一条队列（每帧 ≤8 分发），同等预算、错误隔离与级联抑制。
- VM 重建后定时器清零，插件在 `activate()` 重建（与事件注册同模式）。

## 10. 安全矩阵（"不破坏主应用"承诺）

| 风险 | 防线 | 状态 |
|---|---|---|
| 无限循环 / CPU 抢占 | 每分发 50M 指令 hook | 既有 |
| 内存耗尽 | 64 MiB 每 VM | 既有 |
| 文件系统逃逸 | fs 权限根 + 原子写 | 既有 |
| 编辑→事件反馈环 | on_edit 只读 + 级联抑制 | 新增 |
| 事件/定时器洪泛 | 去抖 150ms、每帧 ≤8 分发、定时器 ≤16 且 ≥50ms | 新增 |
| 坏插件持续报错 | 连续 10 次失败自动停用 | 新增 |
| 通知洪水 | 每分发 ≤8 条、单条 1 KiB 截断 | 新增 |
| 命令递归 | command.run 深度 ≤4 | 新增 |
| require 逃逸 | 相对路径白名单语义 + 插件目录封闭 | 新增 |
| 面板输出失控 | 既有行数/行宽/spans 帽 + args 4 KiB | 既有+扩展 |
| 全禁用排查 | --safe-mode 不加载任何插件代码 | 既有 |
| v1 插件残留 | api_version=1 拒载 + 明确提示 | 新增 |

## 11. 版本治理（前瞻性承诺）

- `api_version`：破坏性主版本。宿主只加载相等值；API 面不兼容变更必须升它。
- `luban.version`：加性特性等级，从 2 起。加 API/加事件/加贡献点只升它，
  老宿主可安全忽略更高值（插件探测后自行降级）。
- 本期起 v1 API 面废弃；`PLUGINS.md` 重写为 v2 参考，并记录本节治理规则。

## 12. 两个预览插件（2.0.0，v2 原生重写）

### markdown（yyn0210/luban-notepad-plugin-markdown）

- 拆模块：`parser.lua`（GFM 解析）/ `renderer.lua`（面板渲染）/ `highlight.lua`
  （语法高亮封装），`main.lua` 只做装配与入口。
- 解析树缓存 VM 内存，`on_edit` 驱动刷新；点击标题折叠/展开章节（args 命令，
  滚动不丢）；点击段落跳转源行（`luban.document.set_cursor`）。
- 删除全部 feature-detect 分支与旧宿主降级代码。

### json-preview（yyn0210/luban-notepad-plugin-json-preview）

- 拆模块：`json/parser.lua`（RFC 8259 严格解析）/ `json/tree.lua`（树渲染）/
  `json/path.lua`（JSONPath）/ `json/meta.lua`（元信息）。
- 折叠状态主存 VM 内存；保留 data 目录持久化为重启/重建兜底（激活时读回一次，
  变更写穿，替代 v1.2.0 的每次读盘）。
- **逐节点折叠**：树行携带 `command = { id = "json.fold.toggle", args = { path } }`，
  点击三角即折叠/展开对应节点，滚动位置保留——光标法 workaround 根治。
- 删除全部 feature-detect 分支与旧宿主降级代码。

### 发布

- 双插件 2.0.0 + 宿主 preview.17 同批发布；store 条目更新、catalog 重新生成、
  `validate.py --download` 通过。

## 13. 测试与验收

### 测试

- `luban-plugin` crate：v2 API 全量正/反用例（错误约定：校验类抛错、资源类丢弃）；
  事件注册校验（清单未声明 → 拒绝）；只读规则与级联抑制；定时器钳制/上限；
  require 逃逸矩阵（`..`、绝对路径、目录外文件、模块缓存）；面板 args 解析与
  4 KiB 帽；settings.set 校验矩阵；notify/cursor/command.run 上限与深度；
  连续失败隔离；api_version=1 拒载提示。
- 应用级：Xvfb 真宿主冒烟——`--command` 驱动双插件；宿主侧断言点击不清面板、
  args 传参、事件去抖生效。
- 插件仓：两仓 `tests/run.lua` 扩展模块/事件/定时器用例（luban stub 同步重写为
  v2 形态）。

### 验收标准

1. v2 宿主加载 v1 插件时明确拒载并提示升级（不崩溃、不半加载）。
2. 每项新能力（事件、面板 args、require、定时器、settings.set、cursor.set、
   command.run、panel.refresh、events.on）至少一个真实插件用例覆盖。
3. 沙箱逃逸矩阵（require 逃逸、无限循环、内存超限、定时器/notify 滥用、命令
   递归、事件风暴）全部被拦截且有测试。
4. 双插件 2.0.0 在新宿主上功能完整：markdown 折叠/跳转、json 逐节点折叠、
   on_edit 刷新、无 legacy 代码路径。
5. fmt / clippy / tests / build 全绿；按发布流程出 preview.17，store 同步发布。

## 14. 明确不做（记入 ROADMAP）

- 编辑器装饰（高亮标记、gutter 图标、波浪线）
- 虚拟文档 / 自定义只读 tab
- 文件系统监听、剪贴板等新事件源
- WASM 后端、进程外隔离
- 选择性重建（VM 保留式重建）
- 插件脚手架生成器（可作为独立小改进随时插入）

## 15. 风险与缓解

| 风险 | 缓解 |
|---|---|
| 事件泵侵入 update loop 影响渲染帧率 | 每帧 ≤8 分发 + 指令帽兜底；队列空时零开销 |
| API 重排双插件全量重写的工作量 | 预估 main.lua 逻辑大部分平移；测试先行（stub 同步 v2） |
| 破坏 v1 后商店出现不可用插件 | 商店仅两个插件且同批发 2.0.0；拒载提示引导更新 |
| (nil,err)→error() 迁移改变插件代码风格 | 两插件由本变更一并重写，无第三方存量 |
| 点击不清面板行为变化 | 无 v1 存量插件，行为即新基线，文档化 |
| 常驻状态 + 注册表内存增长 | 64 MiB 帽 + 重建回收；注册/定时器数量上限 |

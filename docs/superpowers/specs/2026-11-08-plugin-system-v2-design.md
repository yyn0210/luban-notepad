# 插件系统 v2 设计（事件驱动 + 交互升级）

- 日期：2026-11-08
- 状态：待用户审阅
- 基线：v1.0.0-preview.16（main @ 76e2bf8）
- 关联文档：`docs/PLUGINS.md`、`docs/PLUGIN_STORE.md`、`docs/ROADMAP.md`

## 1. 背景与目标

1.0 已交付 Lua 插件平台（manifest + 沙箱化 Lua + 贡献点 + 插件商店）。两个预览
插件（markdown v1.0.1、json-preview v1.2.0）的开发过程暴露出一批结构性痛点：

- 面板行命令无参数、点击即清空面板并丢失滚动位置，插件只能用"光标位置"做交互
  的 workaround（JSON 折叠、Markdown 导航都受此限制）；
- 插件状态在插件计划重建（reload、启停/装卸任意插件、应用重启）时全部丢失，
  JSON 插件被迫用 data 目录序列化 + 2 KiB 路径哈希帽来维持折叠状态；
- 无事件系统（on_save/on_open…），自动刷新依赖 180ms 轮询 + 150ms 去抖；
- 无 require，单文件 main.lua 已膨胀到约 2,950 行；
- settings 只能读不能写；无定时器；无光标控制 API。

本期目标（产品视角）：

1. **开发便捷**：事件/定时器/require/settings 写回让常见插件形态不再需要
   workaround；模块化让插件代码可维护。
2. **可扩展能力强**：事件驱动 + 面板交互参数 + command.run 组合出新的插件形态。
3. **不破坏主应用**：既有沙箱不变，新增洪泛与失败隔离防线；v1 插件零改动兼容。

### 与讨论稿的差异

讨论稿 §1 曾包含"选择性重建（仅重建受影响插件的 VM）"。设计评审后降级为后续
ROADMAP 项：全量重建语义保留，插件以 `activate()` 重水合（rehydrate）状态为标准
模式（与 VS Code 扩展宿主重启模型一致）；JSON 插件保留 data 目录持久化作为重启
级别的兜底。这样本期不必重构 `PluginHost` 的重建路径，风险显著降低，用户可见
行为不受影响。

## 2. 现状关键事实（设计依据）

- `PluginRuntime` 持有常驻 Lua 状态：`main.lua` 仅在 `PluginRuntime::new()` 加载
  一次，`invoke()` 在持久 `plugin` 表上调用入口函数（`runtime.rs:322-383`）。
  模块级状态本来就跨调用存活；"无状态"感来自全量重建，而非每调用新建 VM。
- `PluginHost` 重建路径为全量重建（`plugin_host.rs`）：任意插件计划变更重建所有
  runtime/commands/panels/settings。
- 沙箱：库集 = base/string/table/math/utf8；每分发指令预算 50,000,000（invoke 前
  重置）；每 VM 内存 64 MiB；错误经 traceback 截断回传。
- `api_version` 保持 1；面板子版本 `luban.panel.version = 2` 已有 feature-detect
  先例（preview.2 / preview.8）。

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

- 不改：沙箱库集、权限模型（`filesystem` 唯一实现权限）、manifest 校验核心、
  store/安装、菜单（每插件一级子菜单）、面板渲染核心、safe-mode。
- 改动落点：`luban-plugin/src/runtime.rs`（事件注册表、定时器表、沙箱 require、
  入口第二参数、HostAction 扩展）、`luban-app/src/plugin_host.rs`（事件泵、定时器
  队列、隔离计数）、`luban-app/src/app/plugin_ui.rs`（审批展示 events、隔离恢复入口）、
  `luban-plugin/src/manifest.rs`（可选 events 字段）、`docs/PLUGINS.md`。

## 4. 事件系统

### 4.1 API

```lua
luban.on("on_document_save", "on_save")   -- (事件名, 插件表里的函数键)
luban.version                              -- = 2，宿主特性等级探测点
```

- `luban.version` 是**宿主特性等级**（本设计起为 2），与 manifest 的 `api_version`
  （插件声明的要求等级，保持 1）是两个不同的东西；探测写法
  `(luban.version or 1) >= 2`。
- `luban.on` / `luban.timer.set` 等新 API 均遵循宿主既有两值返回约定：
  `(结果, nil)` 成功、`(nil, 错误信息)` 失败。
- `fn_name` 必须是 main.lua 返回表中已存在的函数（注册时校验，不存在则返回错误）。
  与命令/面板入口"按表键查找"的设计一致；注册表由 VM 内部维护，宿主经统一调度
  入口分发。
- 同一事件可多次注册（按注册顺序调用）；每插件注册总数上限 32。
- 可在 activate 或任意分发上下文中注册。

### 4.2 事件集

| 事件 | 触发时机 | payload |
|---|---|---|
| `on_document_open` | 文档 tab 打开 | 文档快照表 |
| `on_document_save` | 保存完成 | 文档快照表 |
| `on_active_document_changed` | 活动 tab 切换/关闭 | 文档快照表或 nil |
| `on_edit` | 文档编辑后 150ms 稳定去抖（与面板刷新同源信号） | 文档快照表 |

- 文档快照表 = `luban.get_document()` 同款结构；`on_active_document_changed` 的
  nil 表示无活动文档，处理器必须判空。
- manifest 可选 `events: ["on_document_save", ...]` 字段：仅用于审批界面展示
  "此插件会监听保存/编辑"，未知事件名忽略并告警（与未知权限一致），不作运行时
  门槛。

### 4.3 上下文与编辑规则

- 事件处理器签名 `handler(context)`，context 与命令入口一致。
- **只读规则**：`on_edit` 上下文中 `luban.set_document_text` / `insert_document`
  返回错误（信息注明只读上下文）。从根上掐断"编辑→事件→编辑"风暴。
- 其余事件上下文允许编辑文档。
- **级联抑制**：事件/定时器上下文内产生的一切动作（文档编辑、`command.run`）
  不再发射任何事件。formatter-on-save 合法（on_save 里改文档，不再触发 on_edit），
  反馈环不可能出现。

### 4.4 预算与隔离

- 每次事件分发使用与命令相同的 50M 指令预算（invoke 前重置）+ 64 MiB 内存帽。
- 事件分发错误被捕获、记录日志（含插件 id 与事件名），不弹用户通知。
- **连续失败隔离**：任一插件连续 10 次分发失败（命令/事件/定时器合计计数，任一
  成功即清零）→ 自动停用（PluginState::Disabled）+ 一次用户通知，可在插件菜单
  手动重新启用。

## 5. 面板交互 v2

- `PanelLine.command` 与 `PanelAction.command` 升级为**字符串或表**：
  ```lua
  { command = "json.fold.toggle", args = { path = "$.a[0]" } }
  ```
  args 为 JSON 兼容 Lua 表，经宿主转换为入口函数第二参数：`entry(context, args)`。
  v1 单参入口不受影响（Lua 忽略多余实参）。
- **点击行为变更**：点击行命令/面板动作不再清空面板（宿主不再置
  `plugin_panel = None`）、滚动位置保留；重渲染沿用 `luban.panel.open` 已有的滚动
  句柄复用逻辑。v1 插件的可见差异：点击后未主动重开面板时显示旧内容而非空白
  （可接受，文档化）。
- 新增 `luban.panel.refresh(id)`：插件请求宿主重新渲染自己声明的面板（重调面板
  入口），配合定时器实现推送式更新。
- args 大小上限：每条 4 KiB（超限丢弃 args 并告警，命令仍执行）。

## 6. 多文件 require

- 沙箱内实现 `require("lib.parser")` → 解析为 `<plugin-dir>/lib/parser.lua`。
- 规则：仅相对模块路径（`[a-z0-9._/-]+` 形式的模块名，映射为 `.lua` 文件）；
  拒绝绝对路径与包含 `..` 的路径；解析结果不得逃逸插件目录；找不到文件返回
  错误。`package` 库仍不可见，`require` 的缓存表为 VM 内私有实现。
- 被加载模块与 main 同等沙箱环境；加载/执行耗时计入当前分发指令预算；模块缓存
  存活期 = VM 存活期（重建后重新加载）。
- 清单与打包零改动：zip 内多带 .lua 文件即可。

## 7. 定时器

```lua
luban.timer.set(id, ms, "fn_name", one_shot)  -- one_shot 缺省 false
luban.timer.clear(id)
```

- 回调为插件表函数键（注册语义同事件）。
- 上限：每插件 16 个活动定时器；最小间隔 50ms（低于钳制到 50ms）；重复 id 覆盖。
- 到期定时器入事件泵同一条队列（每帧最多 8 个分发），同等预算与错误隔离；
  计数计入连续失败隔离。
- VM 重建后定时器清零，插件须在 `activate()` 里重建（与事件注册同模式）。

## 8. settings 写回与小 API

- `luban.settings.set(key, value)`：key 必须是已声明的 settings 贡献项（未声明
  返回错误，与 `status.set` 同模式）；value 按声明类型校验（boolean/string/
  number/enum），number 钳制 min/max，enum 校验取值；立即持久化到插件 settings
  存储并更新运行时值（后续 `settings.get` 返回新值）。string 遵循声明的
  `max_length`。
- `luban.cursor.set(line, col)`：新 HostAction，设置活动文档光标（1-based，
  越界钳制）。Markdown 点击跳源行、JSON 路径定位的基础。
- `luban.command.run(id, args?)`：通过宿主命令系统运行任意命令（内置或其他
  插件命令）。实现为 `HostAction::RunCommand { id, args, depth }`，宿主在本分发
  返回后执行；目标为插件命令时 args 作为其入口第二参数传入。**深度上限 4**：
  用户/CLI/面板点击发起为 0，链上每次 run +1，超限返回错误并告警。
  事件上下文发起的 run 遵循级联抑制。

## 9. 兼容性策略

- `api_version` 保持 1 不变；宿主对 >1 的拒绝行为不变。
- 所有新能力经探测使用：`(luban.version or 1) >= 2`、`luban.on ~= nil` 等；
  `luban.panel.version` 保持 2。
- 现有 30 个插件 crate 测试**零修改全绿**作为兼容性证明。
- 行为变更清单（文档化到 PLUGINS.md）：点击行命令不再清空面板。

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

## 11. 两个预览插件优化（各升 v2.0.0）

### markdown（yyn0210/luban-notepad-plugin-markdown）

- 拆模块：`parser.lua`（GFM 解析）/ `renderer.lua`（面板渲染）/ `highlight.lua`
  （语法高亮封装），`main.lua` 只做装配与入口。
- 解析树缓存在 VM 内存，`on_edit` 驱动刷新（替代轮询感知路径），刷新仍走面板
  入口返回。
- 新交互：点击标题折叠/展开对应章节（args 命令，滚动不丢）；点击段落跳转源行
  （`cursor.set`）。
- 旧宿主降级：activate 里探测 `luban.version`，不满足时回落现有 v1 行为
  （保留 legacy 逻辑为模块）。

### json-preview（yyn0210/luban-notepad-plugin-json-preview）

- 拆模块：`json/parser.lua`（RFC 8259 严格解析）/ `json/tree.lua`（树渲染）/
  `json/path.lua`（JSONPath）/ `json/meta.lua`（元信息）。
- 折叠状态主存 VM 内存；data 目录持久化保留为重启/重建级兜底（激活时读回，
  变更写穿）。
- **逐节点折叠**：树行携带 `{ command = "json.fold.toggle", args = { path = ... } }`，
  点击三角即折叠/展开对应节点，滚动位置保留——v1.2.0 光标法 workaround 的根治。
- 旧宿主降级同 markdown。

### 发布

- 双插件升 2.0.0，store 更新条目并重新生成 catalog；对照新宿主验证完整功能、
  对照 preview.16 二进制验证旧宿主降级不坏。

## 12. 测试与验收

### 测试

- `luban-plugin` crate：事件注册/分发/nil payload/只读规则/级联抑制；定时器钳制
  与上限；require 逃逸矩阵（`..`、绝对路径、目录外文件）；面板 args 解析与 4 KiB
  帽；settings.set 校验矩阵；notify/cursor/command.run 上限与深度；连续失败隔离。
- 兼容性：现有 workspace 测试全部通过且不改任何 v1 用例（v1 套件即兼容证明）。
- 应用级：Xvfb 真宿主冒烟——`--command` 驱动双插件；验证点击不清面板、args 传参
  （宿主侧断言）、事件去抖生效。
- 插件仓：两仓 `tests/run.lua` 扩展模块/事件/定时器用例（luban stub 同步扩展）。

### 验收标准

1. v1 插件零改动在 v2 宿主上通过全部现有测试。
2. 每项新能力（事件、面板 args、require、定时器、settings.set、cursor.set、
   command.run、panel.refresh）至少一个真实插件用例覆盖。
3. 沙箱逃逸矩阵（require 逃逸、无限循环、内存超限、定时器/notify 滥用、
   命令递归）全部被拦截且有测试。
4. 双插件 2.0.0 在新宿主功能完整、在 preview.16 宿主优雅降级。
5. fmt / clippy / tests / build 全绿；按发布流程出 preview 版本并更新双插件与
   store。

## 13. 明确不做（记入 ROADMAP）

- 编辑器装饰（高亮标记、gutter 图标、波浪线）
- 虚拟文档 / 自定义只读 tab
- 文件系统监听、剪贴板等新事件源
- WASM 后端、进程外隔离、api_version 升版、选择性重建（VM 保留式重建）

## 14. 风险与缓解

| 风险 | 缓解 |
|---|---|
| 事件泵侵入 update loop 影响渲染帧率 | 每帧 ≤8 分发 + 指令帽兜底；事件队列空时零开销 |
| 常驻状态 + 事件注册导致的 VM 内存增长 | 既有 64 MiB 帽 + 重建回收；注册数上限 32 |
| 点击不清面板改变 v1 插件行为 | 影响仅为"显示旧内容而非空白"，文档化 |
| 双插件 2.0.0 降级路径回归 | 保留 legacy 模块 + preview.16 对照测试 |
| command.run 跨插件组合越权 | 深度帽 + 无新增权限面（命令本就用户可见） |

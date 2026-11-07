# vendor/gpui-pre-linux

`gpui-pre-linux` 0.3.9 的本地副本，叠加最小 X11 补丁。

## 为什么 vendored

上游 `gpui-pre` 0.3.9 的 X11 后端在窗口映射**之前**调用
`request_decorations`。对没有合成器的 X11 会话（`client_side_decorations_supported == false`），
上游把客户端装饰请求强制转换为服务端装饰并写入
`_MOTIF_WM_HINTS = [2, 0, 1, 0, 0]`（显式请求窗口管理器装饰），
于是 KWin 等窗口管理器在映射时就绘制原生标题栏。
本应用在窗口构建闭包/首次绘制之后才把属性改写为 `decorations=0`，
这段延迟（实测正常约 150ms，高负载下可达 600ms 以上）就是
“原生标题栏先显示一下再消失”的闪现。

应用侧任何映射后的改写本质上是竞态，无法确定性修复；
唯一确定性的方案是让 gpui 的 X11 后端在映射前写入无边框 hint。

第二处补丁涉及最大化：`PlatformWindow::zoom` 上游只发送
`_NET_WM_STATE` 的 `Toggle`。KWin 会把映射时宽度恰好等于工作区宽度的窗口
预先标记为 `_NET_WM_STATE_MAXIMIZED_HORZ`（已用 1px 差值的窗口验证），
此时 `Toggle` 会把窗口管理器已置位的横向状态反转掉、只打开纵向，
窗口变成“只铺满高度、不铺满宽度”。本应用恢复上次会话时经常打开这种全宽窗口，
因此最大化按钮会失效。补丁改为按期望状态显式 `Add`/`Remove` 两个方向。

同一补丁还处理 KWin 的另外两个行为：

- **还原几何**：KWin 对上述“映射时自动横向最大化”的窗口记录的常规几何是
  屏幕宽度的 2/3，直接 `Remove` 会把窗口缩到 2/3 宽。补丁在最大化前记录窗口
  在根窗口坐标系中的实际位置与大小，等窗口管理器确认取消最大化后，再用
  `ConfigureWindow` 恢复该几何。用户从最大化状态直接拖动窗口时（KWin 自己
  取消最大化）不会触发恢复，避免与拖动位置打架。
- **映射前请求**：`Window::new` 在 `map_window` 之前就调用 `zoom`（会话以
  最大化状态恢复）和 `toggle_fullscreen`（`WindowBounds::Fullscreen`），
  而窗口管理器会忽略未映射窗口的 `_NET_WM_STATE` 消息，导致启动时窗口
  没有最大化。补丁把映射前的请求暂存到 `map_window` 之后再发送。

## 补丁内容

修改 `src/linux/x11/window.rs`，并在 `src/linux/x11/client.rs` 中接入应用侧
拖动的 XInput2 事件（第 3 条）：

### 1. `PlatformWindow::request_decorations`

- X11 上收到 `WindowDecorations::Client` 时解释为**“应用自绘 chrome”**：
  - 立即写入 `_MOTIF_WM_HINTS = [2, 0, 0, 0, 0]`（flags=decorations,
    decorations=0，无边框）。该方法在 `Window::new` 内、`map_window` 之前执行，
    因此窗口管理器从 MapRequest 起看到的就是无边框窗口，不会绘制原生标题栏。
  - 平台继续**报告服务端装饰**（`window_decorations() == Decorations::Server`）：
    gpui-component 根视图因此不预留 20px 透明客户端阴影边距
    （无合成器时该边距会被画成黑色），有合成器与无合成器会话的外观保持一致。
  - 写入失败时不改报告值（保持上游“失败不改状态”的行为）。
- `request_decorations(Server)` 语义不变（窗口管理器装饰）。
- 不触碰 Wayland 后端与其他任何模块。

边框被完全移除后，KWin 也不再给这个窗口提供不可见的调整边框，鼠标拖动
边缘无法改变窗口大小。应用侧因此在
`crates/luban-app/src/app/mod.rs` 的 `client_decorated_frame` 里绘制四边
（5px）和四角（8px）的透明调整热区，悬停时显示调整方向光标，按下后由应用
自己跟踪拖动（见第 3 条）；最大化时两者都不显示，全屏时只保留 1px 边框、
不显示热区。

### 2. `PlatformWindow::zoom` / `toggle_fullscreen` / `map_window`

- 窗口已同时最大化横向和纵向时发送 `Remove`（还原），否则发送 `Add`
  （最大化），两个方向一起设置，不再依赖窗口管理器当前状态做 `Toggle`。
- 幂等：即使 `_NET_WM_STATE` 的事件还没被应用处理（本地状态过期），
  `Add` 也不会把已经置位的方向反转掉。
- 最大化前记录根窗口坐标与尺寸；应用主动取消最大化后，在
  `set_wm_properties` 确认两个方向都已取消时用 `ConfigureWindow` 恢复。
- `zoom`/`toggle_fullscreen` 在窗口映射前只记录请求，`map_window` 之后再发送。
- `X11WindowState` 新增 `mapped`、`pending_zoom`、`pending_fullscreen`、
  `restore_bounds`、`pending_restore` 五个字段。

`crates/luban-app/src/window_registry.rs` 相应地把 X11 手动装饰窗口的
装饰请求改为 `Client`；`crates/luban-app/src/main.rs` 保留映射后的
`apply_borderless_hint`/`remove_window_decorations`（含 120/350/800ms 重试）
作为窗口管理器重启等场景的兜底——补丁生效时它发现属性已是目标值，
不做任何写入。

### 3. `start_window_resize` 由应用自己跟踪拖动

`PlatformWindow::start_window_resize` 原本把整个调整手势交给窗口管理器
（`_NET_WM_MOVERESIZE`）。窗口管理器在拖动期间持有指针并绘制它自己的边缘
光标：KWin 会显示 `right_side`/`e-resize` 一类的**单向**光标，在 Deepin 默认
的 `bloom` 主题里看起来只是一个箭头，于是悬停时应用绘制的双向调整光标在按下
的瞬间消失。

补丁改为：

- `start_window_resize` 记录按下位置和窗口的根窗口几何，不再发送
  `_NET_WM_MOVERESIZE`；指针事件来自按键本身给客户端的隐式抓取
  （XInput2 `XinputMotion`/`XinputButtonRelease`），所以应用不需要额外抓取
  指针。
- 每次移动把新的绝对几何（含左/上边缘带来的位置变化）通过
  `_NET_MOVERESIZE_WINDOW`（gravity=StaticGravity，flags 指定 x/y/宽/高，
  source=应用）交给窗口管理器——客户端窗口被窗口管理器重新挂载到框架窗口
  里，直接 `ConfigureWindow` 只会移动框架内的客户端，不会移动屏幕上的窗口。
- 按下、移动、松开全程不改变光标：悬停时应用设置的双向/对角调整光标保持
  到拖动结束；窗口边缘跟随指针，热区始终在指针下方。
- 查询指针或几何失败时回退到原来的 `_NET_WM_MOVERESIZE` 路径。

`send_moveresize` 的 `_NET_WM_MOVERESIZE` 参数补丁保留（回退路径和
`start_window_move` 仍使用它）：`data.l[3]` 触发按钮写左键 `1`、`data.l[4]`
来源写直接用户操作 `1`，避免窗口管理器把拖动当成键盘触发而使用兜底光标。
`scripts/window-resize-check.sh` 会同时验证四边/四角的几何变化和“拖动期间
光标与悬停光标一致”。

## 与上游版本的关系

- vendored 源码与 `Cargo.lock` 锁定的 registry 版本 0.3.9 完全一致，
  仅叠加本补丁（上游 pristine 校验和：`094a2a91a2c820c890e6f405f1d789e6ae626c9db3897e060d9c5f40bb9d9442`，
  为 `find . -type f | sort | xargs sha256sum | sha256sum` 的输出）。
- 通过根 `Cargo.toml` 的 `[patch.crates-io]` 生效：

  ```toml
  [patch.crates-io]
  gpui-pre-linux = { path = "vendor/gpui-pre-linux" }
  ```

## 升级 gpui-pre 时的步骤

1. 提升 `gpui-pre` 相关依赖版本并更新 `Cargo.lock`。
2. 从 registry 缓存复制新版本的 `gpui-pre-linux` 源码覆盖本目录
   （`~/.cargo/registry/src/<hash>/gpui-pre-linux-<新版本>/`）。
3. 重新套用上面的补丁：在新的
   `src/linux/x11/window.rs` 的 `request_decorations` 开头加入 Client 分支
   （写入 `[2, 0, 0, 0, 0]` 并保持 `state.decorations = Server`），
   并把 `zoom` 的 `Toggle` 改为按 `state.maximized_vertical/horizontal`
   发送 `Add`/`Remove`（需要恢复 `WmHintPropertyState` 的 `Remove`/`Add` 变体）。
   最大化补丁还需要：`X11WindowState` 的 `mapped`/`pending_zoom`/
   `pending_fullscreen`/`restore_bounds`/`pending_restore` 字段、`map_window`
   末尾发送暂存请求、`zoom` 发 `Remove` 前记录 `window_root_geometry`、
   `set_wm_properties` 在确认取消最大化后按记录恢复几何。
   应用侧拖动补丁需要：`X11WindowState` 的 `client_resize` 字段、
   `start_window_resize` 记录几何并跳过 `_NET_WM_MOVERESIZE`、
   `X11WindowStatePtr::client_resize_motion`（按拖动边计算几何并发送
   `_NET_MOVERESIZE_WINDOW`，需要 `XcbAtoms` 增加该原子）、
   `X11WindowStatePtr::finish_client_resize`，以及 `client.rs` 中
   `XinputMotion`/`XinputButtonRelease` 分支（XI2 坐标是 FP1616，需除以
   65535 换成像素）。
   注意核对新版是否仍为“映射前调用 `request_decorations`”的顺序；
   如果上游已改为映射前写入无边框 hint、或 `zoom` 已显式设置状态、
   或上游已把映射前的状态请求延迟到 map 之后，可删除对应补丁；
   都不再需要时删除 vendored 目录。
4. 更新本 README 的校验和与版本号。

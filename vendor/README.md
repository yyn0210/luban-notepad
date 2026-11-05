# vendor/gpui-pre-linux

`gpui-pre-linux` 0.3.9 的本地副本，叠加一个最小 X11 补丁。

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

## 补丁内容

仅修改 `src/linux/x11/window.rs` 的 `PlatformWindow::request_decorations`：

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

`crates/luban-app/src/window_registry.rs` 相应地把 X11 手动装饰窗口的
装饰请求改为 `Client`；`crates/luban-app/src/main.rs` 保留映射后的
`apply_borderless_hint`/`remove_window_decorations`（含 120/350/800ms 重试）
作为窗口管理器重启等场景的兜底——补丁生效时它发现属性已是目标值，
不做任何写入。

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
   （写入 `[2, 0, 0, 0, 0]` 并保持 `state.decorations = Server`）。
   注意核对新版是否仍为“映射前调用 `request_decorations`”的顺序；
   如果上游已改为映射前写入无边框 hint，可直接删除补丁与 vendored 目录。
4. 更新本 README 的校验和与版本号。

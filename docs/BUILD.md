# 构建与运行

Luban Notepad 使用 GPUI Kit `0.8.0`（它固定了与之匹配的 GPUI 快照），并要求
较新的 stable Rust 工具链。仓库的 `rust-toolchain.toml` 会选择 stable 以及
`rustfmt`/`clippy` 组件。

## 快速开始

```bash
cargo build --release -p luban-app
./target/release/luban-notepad
```

调试构建：

```bash
cargo build -p luban-app
./target/debug/luban-notepad
```

## 打包

### Linux

`scripts/package-linux.sh` 会构建 release 二进制，并在 `dist/` 下生成可移植
压缩包和 Debian 安装包（若系统提供 `dpkg-deb`）：

```bash
scripts/package-linux.sh
```

产物：

- `dist/luban-notepad-<版本>-linux-<架构>.tar.gz`：二进制、README、许可证、
  更新日志以及 `.desktop`/图标文件。
- `dist/luban-notepad_<版本>_<架构>.deb`：安装到 `/usr/bin`、
  `/usr/share/applications` 和 `/usr/share/icons/hicolor/<尺寸>x<尺寸>/apps`。

已经构建过 release 二进制时，可加 `--skip-build` 复用现有产物。打包脚本使用
`CARGO_TARGET_DIR`（若已设置）定位二进制。

### macOS

`scripts/package-macos.sh` 会生成 `dist/Luban Notepad.app`，其中包含
`Contents/Resources/luban-notepad.icns` 和对应的 `Info.plist`，并打包为 zip：

```bash
scripts/package-macos.sh
```

同样支持 `--skip-build`。若系统提供 `codesign`，脚本会对 bundle 进行 ad-hoc
签名。

### Windows

`cargo build --release -p luban-app` 时，`crates/luban-app/build.rs` 会把
`packaging/windows/luban-notepad.ico` 编译进可执行文件。Windows 任务栏和窗口
图标直接来自该资源，无需额外的打包步骤。

`scripts/package-windows.ps1` 生成安装版和便携版两种产物：

```powershell
scripts/package-windows.ps1

# 只生成便携版（无需安装 Inno Setup）
scripts/package-windows.ps1 -SkipInstaller

# 只生成安装版
scripts/package-windows.ps1 -SkipPortable

# 复用已构建的 target\release\luban-notepad.exe
scripts/package-windows.ps1 -SkipBuild
```

产物：

- `dist/luban-notepad-<版本>-windows-<架构>-setup.exe`：Inno Setup 安装程序。
  默认按当前用户安装到 `%LOCALAPPDATA%\Programs\Luban Notepad`，无需管理员
  权限；安装向导也可选择为所有用户安装。包含开始菜单项、卸载程序、可选的
  桌面快捷方式和“使用鲁班编辑器打开”右键菜单，并在卸载时询问是否删除配置。
- `dist/luban-notepad-<版本>-windows-<架构>-portable.zip`：解压即用的便携版。
  压缩包内的 `luban-notepad.portable` 标记文件让应用把 `settings.json`、
  `session.json` 和数据写入程序目录下的 `data/`。

两种产物都会附带 `.sha256` 校验文件。安装版需要 Inno Setup 6
（`winget install JRSoftware.InnoSetup`）；脚本会自动查找 `ISCC.exe`，也可用
`INNO_SETUP_ISCC` 环境变量或 `-InnoSetupPath` 指定。若编译器带有
`ChineseSimplified.isl`（官方编译器放在 `Languages\Unofficial`），安装向导会
自动提供简体中文，否则使用英文。

平台图标由 `scripts/generate-platform-icons.py` 从 Linux hicolor PNG 集生成：
Windows 使用多尺寸 `.ico`，macOS 使用 `.icns`。

## 发布

发布由 `.github/workflows/release.yml` 驱动。将 `Cargo.toml` 中的版本改为目标
版本并提交后，推送同名标签：

```bash
git tag v1.0.0-preview.22
git push origin v1.0.0-preview.22
```

工作流会校验标签与 `Cargo.toml` 版本一致，构建 Linux（`.tar.gz`、`.deb`）、
Windows（安装版 `.exe`、便携版 `.zip`）以及 macOS arm64/x86_64（`.zip`），
然后创建 GitHub Release，自动生成发布说明并附带各产物的 SHA-256 校验文件。

也可以在 Actions 页面手动运行 `Release` 工作流，输入一个已经存在的标签。

## Linux

### 系统软件包（Debian/Ubuntu）

```bash
sudo apt-get update
sudo apt-get install -y \
  pkg-config clang cmake \
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
  libx11-dev libxcursor-dev libxrandr-dev libxi-dev \
  libxcb1-dev libxcb-xkb-dev \
  libfontconfig1-dev libfreetype-dev \
  libbz2-dev libpng-dev libbrotli-dev libffi-dev zlib1g-dev \
  libexpat1-dev uuid-dev \
  mesa-vulkan-drivers
```

这些包提供 GPUI 平台与文本后端所需的 XKB/Wayland/字体头文件，以及用于无界面
或无 GPU 机器的软件 Vulkan 驱动（lavapipe）。

Fedora 对应包：`libxkbcommon-devel libxkbcommon-x11-devel
wayland-devel libX11-devel libXcursor-devel libXrandr-devel libXi-devel
libxcb-devel mesa-vulkan-drivers freetype-devel fontconfig-devel
bzip2-devel libpng-devel brotli-devel libffi-devel zlib-devel expat-devel
uuid-devel clang cmake`。

Arch 对应包：`libxkbcommon wayland libx11 libxcursor libxrandr libxi
libxcb mesa freetype2 fontconfig bzip2 libpng brotli libffi zlib expat
util-linux-libs clang cmake`。

### 显示服务器

GPUI 支持 X11 和 Wayland。如果两者都可用，平台会选择当前活动会话。
对于仅 X11 的会话，无需额外环境变量。

### 渲染器

GPUI 使用 WGPU。在没有硬件 Vulkan 驱动的系统上，请安装
`mesa-vulkan-drivers`（lavapipe）。如果 Vulkan 不可用，可尝试：

```bash
WGPU_BACKEND=gl target/release/luban-notepad
```

## macOS

- 安装 Xcode Command Line Tools：`xcode-select --install`。
- 需要支持 Metal 的 Mac。

`scripts/package-macos.sh` 会构建 release 并生成可双击运行的
`dist/Luban Notepad.app`：

```bash
scripts/package-macos.sh
open "dist/Luban Notepad.app"
```

直接运行 `target/release/luban-notepad` 时没有 app bundle，Dock 会显示通用图标；
该二进制未签名，Gatekeeper 可能要求右键 → 打开。打包脚本生成的 `.app` 会进行
ad-hoc 签名。

## Windows

- 使用 `x86_64-pc-windows-msvc` 工具链安装 Rust。
- 安装 Visual Studio Build Tools，选择“使用 C++ 的桌面开发”工作负载，
  并安装 Windows SDK。
- 不需要额外的系统库；GPUI 使用 Win32、DirectWrite 和 WGPU（DirectX 12/Vulkan）。

```powershell
cargo build --release -p luban-app
.\target\release\luban-notepad.exe
```

构建时会自动嵌入 `packaging/windows/luban-notepad.ico`。

## CI 与冒烟测试

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

无界面 GUI 冒烟测试（Linux CI 使用）：

```bash
sudo apt-get install -y xvfb xauth
xvfb-run -a -s "-screen 0 1440x900x24" \
  env WGPU_BACKEND=vulkan LUBAN_NOTEPAD_CONFIG_DIR=/tmp/luban-ci \
  target/debug/luban-notepad --smoke-test
```

`--smoke-test` 会创建窗口、渲染初始编辑器，然后以退出码 0 退出。若无法创建窗口
或初始化 GPUI，则以非零码退出。

### GUI 集成测试（Xvfb + xdotool）

```bash
sudo apt-get install -y xvfb xauth xdotool
scripts/gui-integration.sh --require-display
```

脚本用真实键鼠事件驱动主窗口：鼠标点击定位并输入、`Ctrl+S` 落盘、`Ctrl+F`
查找替换、打开对话框切换标签、未保存关闭提示；断言使用保存后的文件内容、
窗口标题和进程状态，不比较截图。没有显示服务时脚本默认跳过并返回 0；
`--require-display` 把“跳过”变成失败，CI 使用该模式。

- `LUBAN_BIN` 指定应用二进制，默认 `$CARGO_TARGET_DIR/debug/luban-notepad`。
- `LUBAN_GUI_DISPLAY` 复用指定显示；默认总是启动私有 Xvfb，避免影响当前桌面。
- `LUBAN_GUI_ARTIFACTS` 指定失败截图与日志目录；CI 在失败时上传该目录。

### 窗口调整大小与光标检查（Xvfb + KWin）

```bash
sudo apt-get install -y xvfb xdotool kwin-x11
scripts/window-resize-check.sh --require-display
```

手动装饰的 X11 窗口由应用自己提供调整热区。脚本在私有 Xvfb + KWin 中拖动
四边和四角，断言窗口几何按方向变化；用 XFixes 读取悬停时的光标图像，断言
四边/四角的调整光标彼此对应（左右相同、上下相同、两组对角线各自相同）且都
不同于编辑区光标；再断言拖动期间（按键仍按住时）的光标与悬停光标完全一致，
快速按下并拖动以按下位置为锚点，按住时按下/松开其他鼠标键不中断拖动，缩到
最小尺寸时对边保持不动，并验证最大化后边缘拖动不改变窗口、还原后恢复原
几何。需要 `kwin_x11`、`libXfixes` 和 `python3`（仅标准库 ctypes），缺少时
默认跳过。

- `LUBAN_BIN` 指定应用二进制，默认 `$CARGO_TARGET_DIR/debug/luban-notepad`。
- `LUBAN_RESIZE_ARTIFACTS` 指定失败时复制应用日志的目录。

### 性能预算（夜间）

```bash
# 完整预算：200 MB GUI 打开、1 GB 打开/查找/保存、3 GB 内存增量
scripts/perf-nightly.sh

# 快速本地验证（缩放文件与引擎预算；GUI 打开预算仍按 4,500 ms 判定）
scripts/perf-nightly.sh --scale 0.02

# 验证超预算时的失败路径
scripts/perf-nightly.sh --self-test
```

基准使用 release 构建，输出逐项 JSON（实测值、预算、路线图目标），任一项超预算
时以非零码退出。`.github/workflows/nightly.yml` 每天定时运行并上传报告；普通
PR 不运行。`LUBAN_PERF_SCALE` 缩放文件与引擎预算（GUI 打开时间不随文件大小
线性缩放，预算保持完整值），`LUBAN_PERF_BUDGET_FACTOR` 整体缩放预算，
`LUBAN_PERF_REPORT` 指定报告路径。基准每次运行都会增量构建 release 应用，
避免测量过期二进制。

## 配置覆盖

`LUBAN_NOTEPAD_CONFIG_DIR` 可重定向 `settings.json` 和 `session.json`。
它适用于：

- 集成测试，
- 运行多个相互独立的配置档案，
- 由外部启动器指定配置目录。

Windows 便携版通常不需要设置该环境变量：只要可执行文件旁边存在
`luban-notepad.portable` 标记文件，应用就会把配置写入同目录的 `data/`。
安装版不包含该标记文件，因此使用常规的 `%APPDATA%\luban-notepad`。

## 字体

编辑器使用 GPUI 的系统等宽字体及其回退链。对于中日韩文档，请安装 CJK 字体，
例如 Noto Sans CJK、Source Han Sans 或文泉驿。在 Linux 上，`fonts-noto-cjk`
是一个不错的选择。`docs/screenshots/` 中经过验证的截图包含通过系统字体回退
渲染的中文、日文、韩文和 emoji 文本。

可以在首选项中设置自定义等宽字体族；留空则使用主题的默认字体。

## 故障排查

| 现象 | 解决方法 |
| --- | --- |
| 构建错误：找不到 `xkbcommon.pc` | 安装 `libxkbcommon-dev` 和 `libxkbcommon-x11-dev`。 |
| 构建错误：找不到 `wayland-client.pc` | 安装 `libwayland-dev`。 |
| 构建错误：找不到 `freetype2.pc`/`fontconfig.pc` | 安装 `libfreetype-dev` 和 `libfontconfig1-dev`。 |
| 窗口创建失败 / 没有 Vulkan 设备 | 安装 `mesa-vulkan-drivers`，或尝试 `WGPU_BACKEND=gl`。 |
| macOS 上没有文字字形 | 确保已安装 Xcode Command Line Tools；GPUI 的文本后端使用 CoreText/Metal。 |
| 中日韩文字显示为方框 | 安装 CJK 系统字体；重启应用。 |
| 其他桌面应用的文件对话框不出现 | Luban Notepad 使用自己的对话框，因此不应影响打开/保存。 |

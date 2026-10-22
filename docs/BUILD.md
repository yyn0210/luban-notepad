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

## 打包（Linux）

`scripts/package-linux.sh` 会构建 release 二进制，并在 `dist/` 下生成可移植
压缩包和 Debian 安装包（若系统提供 `dpkg-deb`）：

```bash
scripts/package-linux.sh
```

产物：

- `dist/luban-notepad-<版本>-linux-<架构>.tar.gz`：二进制、README、许可证、
  更新日志以及 `.desktop`/图标文件。
- `dist/luban-notepad_<版本>_<架构>.deb`：安装到 `/usr/bin`、
  `/usr/share/applications` 和 `/usr/share/icons/hicolor/scalable/apps`。

已经构建过 release 二进制时，可加 `--skip-build` 复用现有产物。打包脚本使用
`CARGO_TARGET_DIR`（若已设置）定位二进制。

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

```bash
cargo build --release -p luban-app
open target/release/luban-notepad
```

二进制文件未进行代码签名。Gatekeeper 可能要求首次运行时右键 → 打开，或进行
临时签名（`codesign --force --deep -s -`）。

## Windows

- 使用 `x86_64-pc-windows-msvc` 工具链安装 Rust。
- 安装 Visual Studio Build Tools，选择“使用 C++ 的桌面开发”工作负载，
  并安装 Windows SDK。
- 不需要额外的系统库；GPUI 使用 Win32、DirectWrite 和 WGPU（DirectX 12/Vulkan）。

```powershell
cargo build --release -p luban-app
.\target\release\luban-notepad.exe
```

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

## 配置覆盖

`LUBAN_NOTEPAD_CONFIG_DIR` 可重定向 `settings.json` 和 `session.json`。
它适用于：

- 便携安装，
- 集成测试，
- 运行多个相互独立的配置档案。

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

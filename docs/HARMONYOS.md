# OpenHarmony / HarmonyOS 支持

状态：Phase 0（集成验证）进行中。桌面平台不受影响。

## 结论

社区后端 [`ohos-rs/gpui-ohos`](https://github.com/ohos-rs/gpui-ohos) 覆盖了
GPUI 在 OpenHarmony 上需要的平台层（窗口、触摸/鼠标/键盘、IME、剪贴板、
系统菜单、文件选择器、持久授权、无障碍、自定义 GLES/Vulkan 渲染器），
因此不需要自研后端。Luban 的接入方式是：

1. 把 GPUI 依赖对齐到 `gpui-ohos` 使用的 `gpui-pre 0.3.11` 组合
   （`gpui-kit 0.9.0` 精确固定）；
2. 把桌面程序拆成 `luban-app` 库 + 薄 `main.rs`；
3. 新增 `crates/luban-ohos` cdylib：ArkTS `NativeAbility` →
   `gpui_ohos::current_platform` → `luban_app::run_embedded`；
4. 用 `crates/luban-ohos/ohos` 的 Stage 模型工程打 HAP。

## 已落地

| 项 | 位置 |
| --- | --- |
| `gpui-kit 0.9.0` / `gpui-pre 0.3.11` 对齐，9 处 `secondary(cx)` 迁移 | `crates/luban-app/Cargo.toml`、`Cargo.lock` |
| `gpui-pre-linux` 重基到 0.3.11（13 处 X11 补丁） | `vendor/gpui-pre-linux` |
| `gpui-pre-platform` OHOS 编译分支（`cfg_select!` 不再无分支） | `vendor/gpui-pre-platform` |
| `gpui-ohos` vendor（rev `324abb33`） | `vendor/gpui-ohos` |
| 库入口、`EmbeddedStartup`、`run_embedded` | `crates/luban-app/src/lib.rs` |
| OHOS 入口与 HAP 工程 | `crates/luban-ohos` |
| CI 交叉编译 arm64 cdylib 并打包未签名 HAP（7.0 public SDK API 26 + hvigor 6） | `.github/workflows/ci.yml` |
| X11 依赖/代码在 OHOS 上关闭 | `luban-app/Cargo.toml`、`window_registry.rs`、`lib.rs` |

已执行的验证：

- `cargo check --workspace --all-targets`：通过
- `cargo test --workspace`：**646 passed / 0 failed**
- `cargo check -p luban-ohos --target aarch64-unknown-linux-ohos`：通过
- 桌面二进制 `--smoke-test --safe-mode`（Xvfb + lavapipe）：退出码 0，
  库拆分后的启动路径正常

`cargo check` 不链接；真正的 `ohrs build` 仍需真实 NDK 与签名，属于 G1 的
设备验收。

## CI（GitHub Actions）

`.github/workflows/ci.yml` 的 `ohos` job 走完整链路，不再只交叉编译 cdylib：

1. 下载并校验 OpenHarmony 7.0 public SDK（`26.0.0.38-Beta`，API 26），把
   `ets/js/native/toolchains/previewer` 解到 `$OHOS_BASE_SDK_HOME/26.0.0/`；
2. 从 DevEco command-line tools 5.1.0.840 中只提取 `ohpm`；hvigor 6.26.20 与
   `@ohos/hvigor-ohos-plugin` 从 `https://repo.harmonyos.com/npm/` 安装。
   CLI 自带的 hvigor 5.18.5 只认 `metaVersion 3.0.0`，读不了 API 26 SDK 的
   `3.0.2`，所以不能直接用 CLI 里的 hvigor；
3. `ohrs build --arch arm64 --release` 生成 `libluban_ohos.so`；
4. `ohpm install --all` + hvigor `assembleHap` 生成
   `entry-default-unsigned.hap`；
5. 校验 HAP 内的 `libs/arm64-v8a/libluban_ohos.so`、`libc++_shared.so` 和
   `module.json` 的权限声明，然后上传 `ohos-arm64` 工件（含 HAP 与 .so）。

CI 使用 `crates/luban-ohos/ohos/build-profile.openharmony.json5`：
`runtimeOS: OpenHarmony`、`compileSdkVersion/targetSdkVersion: "26.0.0"`、
`compatibleSdkVersion: 15`。API 26 起版本字段必须是点分字符串，低于 26 的
`compatibleSdkVersion` 仍是数字；本地 DevEco 模板保留 HarmonyOS 版本串
（如 `6.1.0(23)`）不变。

工件是**未签名** HAP，适合 OpenHarmony 开发镜像/模拟器；真机与 AppGallery
仍需 AGC 证书和 profile（见上文签名一节）。在 Linux 上可按 CI 的目录约定
复现：

```sh
export OHOS_BASE_SDK_HOME=/path/to/sdk-root   # 含 26.0.0/{ets,js,native,toolchains,previewer}
export DEVECO_SDK_HOME="$OHOS_BASE_SDK_HOME"
export OHOS_NDK_HOME="$OHOS_BASE_SDK_HOME/26.0.0"
export PATH="/path/to/command-line-tools/ohpm/bin:$PATH"

cd crates/luban-ohos
scripts/build-hap.sh --profile "$PWD/ohos/build-profile.openharmony.json5" --release
cd ohos
ohpm install --all
node /path/to/node_modules/@ohos/hvigor/bin/hvigor.js \
  --mode module -p product=default -p buildMode=release assembleHap --no-daemon
```

## 待办（Phase 0 剩余）

- **G1 真机**：`ohrs build` 链接、HAP 签名、安装启动、`hdc` 冒烟。
- **G3 Linux 专属逻辑**：`file_picker/places.rs` 的 `/proc/mounts` 侧栏、
  `app/mod.rs` 的客户端调整热区、`app/menus.rs` 的自绘窗口按钮，在 OHOS 上
  都应走系统装饰/系统 picker；目前靠 `manual_chrome = false` 不发生，但需要
  显式清理。
- **G4 文件模型**：系统 picker URI → path、授权恢复早于会话恢复、最近文件
  存 URI、撤销授权后的重新授权流程、Trash 语义。
- **G5 输入与渲染**：中文 IME、触摸滚动/选择、剪贴板权限（模拟器已验证）、
  GLES 与 PC wgpu 的视觉对比、`/system/fonts` 字体回退。
- **G6 发行**：bundle 名、签名/ACL、AppGallery 或开源鸿蒙分发、插件商店
  `INTERNET` 与 `aarch64-linux-ohos` target 映射。

## 需要尽早决定

1. **API 基线**：示例工程与 `gpui-ohos` 默认使用 API 26 工具链。若目标设备
   低于该基线，需要先验证低版本设备；否则首发只覆盖最新系统。
2. **首发形态**：当前 `module.json5` 声明 `tablet` / `2in1`，未声明 `phone`。
3. **发行渠道**：AppGallery、开源鸿蒙社区分发或企业内部分发，签名与 ACL
   要求不同。
4. **文件访问产品策略**：picker + 最近授权为主，还是需要整目录授权方案。

## 风险

- `gpui-ohos` 为 `0.0.1-beta.0`，单维护者、无正式 release；已按 commit
  vendor，需预留上游修复与安全审计成本。
- 版本被锁定在 `gpui-pre 0.3.11`，直到上游跟进。
- 自定义 GLES 渲染器与 `gpui-component` 的视觉/性能差异需要在 G5 对比。

## 虚机验证（2026-11-11）

本机 KaihongOS 5.0.2.25 QEMU 镜像的实测结论：

- 客户机是 **x86_64、API 14**，自带 `bm` / `aa` / `hdc` / `hilog`，
  `bm install -p <hap>` 可用，开发者模式已开启；镜像里没有 sshd/telnetd，
  eth0 默认无 IP。
- 用镜像 ESP 里的 `bzImage`/`ramdisk.img` 直接 `-kernel/-initrd` 启动一份
  副本（绕过 UEFI 的 PXE 先启动），可从串口拿到 root shell；
  `-display egl-headless,gl=on` 可保持 render_service 正常。
- 文件可通过附加一块 FAT 共享盘（`/dev/block/vdb`）传入客户机。
- OpenHarmony 5.0.2 (API 14) SDK 可用；`x86_64-unknown-linux-ohos`
  的 hello world 与 `libluban_ohos.so` 都能链接（NDK clang +
  `--sysroot`），并在客户机上运行成功。
- **阻塞**：`@ohos-rs/ability 1.0.0-beta.5` 的 ArkTS 部分使用了 API 15+
  的窗口接口（`windowWillClose`、`setWindowContainerColor`、
  `setWindowTitle`、`SubWindowOptions.focusOnShow/maximizeSupported`），
  用 API 14 SDK 做 ArkTS 编译会直接报错，客户机侧也检索不到这些实现。
  因此该镜像无法运行 gpui-ohos 的 UI；需要 API 18+（最好 20/26）的
  镜像或真机，或对 ArkTS 宿主做 API 14 移植。

## 在 KaihongOS 6.1 虚机上跑通（2026-11-11）

用另一份 KaihongOS 6.1 ISO 安装出 x86_64 虚机（API 23，内核 6.6.101），
经 `hdc tconn 127.0.0.1:10178` 连接，完成了 HAP 的编译、签名、安装和启动：

- 原生库：`OHOS_NDK_HOME=<sdk>/linux ohrs build --arch x86_64 --release`
  产出 `dist/x86_64/libluban_ohos.so` 与 `dist/index.d.ts`
  （注意：`ohrs` 要指向 SDK 根，hvigor 要指向带 API 版本目录的 SDK 根）。
- ArkTS：DevEco command-line tools 5.1.0（hvigor 5.18.5 / ohpm 5.1.3）+
  OpenHarmony 6.1 SDK（API 23）；`ohpm install` 拉取 `@ohos-rs/ability`
  及插件；build-profile 用 `compileSdkVersion/targetSdkVersion 23`、
  `compatibleSdkVersion 12`、`runtimeOS: OpenHarmony`、hvigor modelVersion
  `5.1.0`。
- 为 API 23 对 `@ohos-rs/ability` 做 3 处兼容修改（仅本地 `oh_modules`）：
  删除 `sdkApiVersion >= 26` 的 `zLevelAboveParentLoosened` 赋值；
  `WindowTitleBar` 的 `@Prop icon/label` 改为普通成员；`MenuBarSlot.build`
  改为空 `Column`（该 SDK 的 ArkTS 检查器不允许在 build 中调用普通函数）。
- 签名：SDK 自带 `hap-sign-tool.jar` + `OpenHarmony.p12`；profile 由
  `UnsgnedReleasedProfileTemplate.json` 生成（bundle-name、未来有效期、
  `allowed-acls` 加入 `ohos.permission.READ_PASTEBOARD`），签出
  `profile.p7b`；再用 profile 里的 CA 签发证书 + CA + Root 组成证书链签
  HAP。现在可用 `crates/luban-ohos/scripts/sign-hap.sh` 自动完成。
- 结果：`hdc install -r` 成功，`aa start -a EntryAbility -b
  com.janeluo.lubannotepad` 启动；完整编辑器 UI（菜单/工具栏/标签页/状态栏）
  渲染正常，键盘输入、修改标记正常。
- 配置目录：OHOS 进程的 `HOME=/storage/Users/currentUser` 不可写，会话保存
  报 `Operation not permitted`。`crates/luban-ohos/src/ohos_entry.rs` 现在
  在启动前把 `LUBAN_NOTEPAD_CONFIG_DIR` 指到
  `/data/storage/el2/base/files/luban-notepad`，已验证 settings/session/
  plugins 正常落盘。

尚未在 6.1 上验证：系统文件选择器、中文 IME、触摸/鼠标手势、GLES 与桌面的
视觉对比、aarch64 真机。剪贴板权限已在 DevEco HarmonyOS 6.1 模拟器验证，
见下一节。

补充（重装验证）：首次开机的 OOBE 必须走完登录密码步骤（至少 6 位、
数字/字母）。跳过该步骤会在最后一页出现 “PIN码设置失败（12300001）”，
且“完成/上一步”失去响应；重装并在“创建登录密码”页设置密码后即可正常
进入桌面。虚机口令属于本地测试环境配置，不写入仓库。

## 2026-11-13 剪贴板 ACL 与运行时授权（DevEco HarmonyOS 6.1 模拟器）

上一轮无签名 HAP 上 Ctrl+V 返回 `201`。根因是 `READ_PASTEBOARD` 为
`system_basic + user_grant`，需要同时满足两层：

- **签名 profile ACL**：未签名 HAP 没有 profile，`acls.allowed-acls` 为空。
  用 `scripts/sign-hap.sh` 从 SDK 的 `UnsgnedReleasedProfileTemplate.json`
  生成 profile（bundle-name、未来有效期、`allowed-acls` 加入
  `ohos.permission.READ_PASTEBOARD`），签出 `profile.p7b` 后再签 HAP。
  安装后 `bm dump` 的 `appProvisionType` 为 `release`，说明 profile 已生效。
- **运行时授权**：ACL 只负责放行，应用仍需调用
  `requestPermissionsFromUser`。`EntryAbility.onForeground` 现在请求一次，
  授权后 `reqPermissionStates` 为 `0`，Ctrl+V 正常。

注意：

- 覆盖安装旧的未签名包会报 `9568332 install sign info inconsistent`，必须先
  卸载；`sign-hap.sh --install` 已处理（会丢失应用数据）。
- `Edit → Paste` 走 ArkUI 原生粘贴控件，没有运行时授权也能工作；Ctrl+V 走
  原生剪贴板读取，必须 ACL + 运行时授权同时满足。
- OpenHarmony 测试根只适用于模拟器/开发机。真机/AppGallery 需要在 AGC 为
  应用申请 `ohos.permission.READ_PASTEBOARD` 的 ACL 权限，审核通过后用 AGC
  生成的 profile 签名；调试 profile 的 ACL 支持以 AGC 审核结果为准。

用法（路径、口令、设备序列号都不入库）：

```sh
export OHOS_SDK_LIB="<DevEco SDK>/default/openharmony/toolchains/lib"
export OHOS_SIGN_STORE_PASSWORD="<SDK 测试密钥库口令>"
crates/luban-ohos/scripts/sign-hap.sh --install --device <serial>
```

## 2026-11-11 e2e 回归（KaihongOS 6.1 虚机）

本轮在 `KaihongOS61-install-0` 上通过 `hdc` + QEMU SDL 窗口做全功能
冒烟，修复了 3 个阻断性问题：

1. **菜单点击后立即关闭**：KaihongOS 2-in-1 把同一次鼠标操作同时上报为
   XComponent 鼠标事件和 tool type 为 `Unknown` 的触摸点。触摸点在抬起时
   会合成一次“兼容鼠标点击”，其 mouse-down 落在刚展开的菜单外，菜单随即
   关闭。`vendor/gpui-ohos` 现在按“触摸接触期间是否出现过鼠标事件 / 200ms
   窗口”识别重复触摸：丢弃重复 move，并跳过该接触的兼容 tap。菜单可正常
   单击展开并保持。
2. **未知键码 SIGABRT**：`ohos-xcomponent-binding` 的 `KeyCode::from` 与
   `openharmony-ability` ArkUI 键盘路径的 `KeyCode::from(key_code_raw)`
   遇到枚举外键码会 `unreachable!` 并 abort 进程（OpenHarmony 的 `uinput`
   测试工具注入原始 evdev 键码即可复现）。两个 crate 已 vendored 并改为
   `try_from_raw`：XComponent 路径回退为 `KeyCode::Unknown`，ArkUI 路径
   直接忽略未知键。`uinput` 注入 Ctrl+N 后进程保持存活。
3. **文件打开/另存**：OHOS 上 `file.open` 与首次保存/Save As 改用系统
   picker（`cx.prompt_for_paths` / `prompt_for_new_path`），从而获得用户
   文档的持久授权。KaihongOS 6.1 的 `FilePickerUIExtAbility` 在本虚机上
   启动即 SIGABRT（`libwm`/`UIContentImpl::CommonInitialize`），因此
   picker 在 2 秒内返回取消/报错时回退到应用内 picker，并把默认目录设为
   应用沙箱的 `luban-notepad/Documents`（HOME 不可写）。

已验证：启动、会话恢复、从磁盘打开 UTF-8/中文文件、编辑、撤销/重做
（桌面用例）、主题切换、菜单单击、`uinput` 注入键不崩溃。

本轮未解决/待跟进：

- **中文 IME 提交**：拼音候选窗正常出现，选择候选后编辑器不插入文本。
  `ohos-ime-binding` 的 NDK TextEditorProxy 路径在该镜像上没有把
  `insert_text` 送到 GPUI 输入处理器，需要在真机/更高 API 上复测或修
  `gpui-ohos`/`ohos-ime-binding`。2026-11-12 的定位进展见下一节。
- **系统 picker**：上述 KaihongOS 6.1 FilePicker 崩溃是镜像问题；真机需
  重新验证。回退路径可用，但用户文档仍需系统 picker 授权。
- **菜单后的工具栏命中有残留**：2026-11-12 定位为快速单击被吞（见下节），
  已修复。
- **暗色主题状态栏**：2026-11-12 复测为可读（#a3a3a3 on #262626），
  “不可见”来自窗口底部被任务栏遮挡与截图缩放，不是主题 token 问题。
- 应用内 picker 的空目录/选择错误文案出现“请选择主题文件”，是 i18n key
  或过滤器选择错误；已修复。
- 触摸滚动、剪贴板权限弹窗、GLES/桌面视觉对比、aarch64 真机仍未验证。

## 2026-11-12 快速单击、保存与 IME 定位

继续在 `KaihongOS61-install-0` 上做交互回归，修复并复测了以下问题：

1. **快速单击被吞（工具栏按钮、菜单项需要“慢按”）**：根因不在命中区，
   而是 `gpui-ohos` 的按钮补偿逻辑。OpenHarmony 的 ArkUI Pointer Move
   在按住鼠标期间仍会报 `mouse_buttons_pressed = false`；旧代码据此调用
   `cancel_mouse_input()`，在真正的 XComponent Release 之前合成了一个
   `(-1, -1)` 的 MouseUp。GPUI 收到该 MouseUp 时命中测试为空，清掉了
   `pending_mouse_down`，随后真正的 MouseUp 不再产生 ClickEvent。
   修复：只有指针不在窗口内（`!hovered`）时才做“离开窗口后释放丢失”的
   补偿；窗口内按住的 Move 不再取消按压。验证：菜单项（新建/保存）与
   工具栏（打开/保存）快速单击均立即生效，不再需要慢按。
2. **鼠标/触摸重复上报**：重复识别从“最近 200ms 内出现过任意鼠标事件”
   改为“鼠标按键事件落在本次接触起点 ±200ms 内”，悬停 Move 不再误伤
   真实触摸；并记录已合成的兼容 tap，若权威鼠标事件随后到达则丢弃该次
   重复点击，避免一次操作触发两次。
3. **保存失败**：日志与取色确认应用已正确报错
   `保存失败: 输入/输出错误: Permission denied (os error 13)`。真实原因是
   测试环境的 `Documents` 目录由 root 创建，原子保存所需的同目录临时文件
   无法写入；把目录 `chown` 给应用后，工具栏保存、文件菜单保存、`Ctrl+S`
   都能写盘（`stat` 时间与文件内容均已核对）。
4. **应用内 picker 目录**：启动时创建 `<config>/Documents`，并用临时文件
   探测目录可写性；`Documents` 不可写时回退到配置根目录，避免把用户送进
   一个“能看不能存”的目录。空选择错误文案改为“请选择要打开的文件”
   （新增 `dialog.select_open_file`）。
5. **中文 IME**：attach 链路成功（`request_keyboard` 各标志均为 true，
   `set_native_text_input_active(true)` 无报错），但输入拼音时只有
   `ImeStatusEvent` 到达，既没有 `PreviewTextEvent` 也没有
   `TextInputEvent`；系统候选窗照常显示。判断为 KaihongOS 6.1 的 IME
   没有对该 `OH_InputMethodController_Attach` 编辑器调用 TextEditorProxy
   回调，属于镜像/平台侧限制，需真机或更高 API 复测。另修复了
   `FocusChanged(false)` 的过度 detach：候选窗/软键盘取得 ArkUI 节点焦点
   时不再断开 IME（窗口本身仍 active），避免本来就少的回调被进一步丢弃。
6. **暗色状态栏与对话框底部**：原生分辨率取色为文字 #a3a3a3、底色
   #262626（对比度约 4.6:1），可读；先前截图发黑是缩放/JPEG 与窗口底部
   延伸到任务栏下方共同造成。浮动窗口未最大化时，状态栏与文件选择器
   页脚（取消/打开）会落到 KaihongOS 任务栏后面，最大化后完整可见。这是
   窗口管理器工作区约束问题，本机 `getAvailableArea` 还返回
   “Capability not supported”，代码侧暂不处理。

复测通过项：快速单击（菜单/工具栏）、文件菜单保存、工具栏保存、`Ctrl+S`
保存、会话恢复、UTF-8/中文文件打开与编辑、应用内 picker 回退与错误文案、
暗色主题状态栏读数。

## 2026-11-12 文件选择器：系统优先，失败时回退

OpenHarmony 的权限模型是应用只能按路径访问自己的沙箱，沙箱外用户文件必须
经系统 picker 选择并授权。因此系统 picker 是主路径：打开文件、打开/添加
文件夹、打开工作区、查找文件夹、导入主题/设置、导出、另存为，全部通过
`prompt_for_paths` / `prompt_for_new_path`。

但本虚机的系统 `FilePickerUIExtAbility` 会崩溃，无法呈现选择器。faultlog
显示是系统组件自身 abort：

- `cpcrash-com.kaihong.filemanager-*`：SIGABRT，
  `libwm.z.so` → `libace_compatible.z.so`
  `OHOS::Ace::UIContentImpl::CommonInitialize`。
- 崩溃前 hilog 有 `OpenGLWrapper: EGLDislay is invalid`、
  `libiGraphicsCore.z.so: No such file or directory`、
  `window_session_impl.cpp SetWindowType: Session is invalid`。
- `com.kaihong.filemanager:sysPicker/filePicker` 进程启动约 100ms 后死亡，
  `DocumentViewPicker` 以“取消”返回。

因此最终行为是“系统优先 + 失败回退”，不是无条件使用自研 picker：

- 系统 picker 正常返回时，一律使用其结果（真机/正常镜像的主路径）。
- 系统 picker 在 2 秒内返回“取消”、返回错误或通道关闭（本虚机的崩溃属于
  这一类），回退到应用内 picker，并在状态栏提示
  “系统文件选择器不可用，已切换到应用内选择器。”
  （`status.picker_fallback`）。退出/关闭流程中的保存会继续在应用内 picker
  完成，不再中止。
- 超过 2 秒的“取消”视为用户主动取消：保存关闭流程仍会
  `abort_pending_saves`。
- 应用内 picker 只在 OHOS 回退时使用；其侧栏不再有 `/` 和挂载点，最近/
  工作区目录经 `read_dir` 可读性过滤，位置为“应用文档”/“应用文件”，
  沙箱外目录显示权限提示而不是 `os error 13`。
- `apply_open_path` / `apply_save_path` / `apply_folder_path` 由系统 picker
  与应用内 picker 共用，避免两套确认逻辑漂移。

验证：工具栏打开与另存为都会先尝试系统 picker（`sysPicker` 进程启动后
SIGABRT），随后应用内 picker 自动出现，状态栏显示回退提示；打开/另存为
对话框均可正常操作。OHOS 与桌面 `cargo check` 均通过。真机需确认系统
picker 正常时不会触发回退。




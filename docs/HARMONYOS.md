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
3. **发行渠道**：已定 AppGallery（2026-11-14 起）与深开鸿 KaihongOS 应用
   市场（2026-11-15 起）双渠道，签名与 ACL 各走各的（见对应日期章节）；
   企业内部分发暂未涉及。
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

## 2026-11-14 文件选择器授权持久化对齐官方文档

按华为《用户文件授权持久化》文档补齐三处缺口，使 Picker 选择的文件在应用
重启后免重新授权：

- **签名 ACL**：`ohos.permission.FILE_ACCESS_PERSIST` 是 `system_basic` 级，
  module.json5 声明之外还必须在签名 profile 的 `acls.allowed-acls` 里才会在
  安装时授予。此前 `sign-hap.sh` 默认 ACL 只有 `READ_PASTEBOARD`，运行时
  `fileShare.persistPermission` 必报 `201`。现在默认 ACL 为
  `READ_PASTEBOARD,FILE_ACCESS_PERSIST`。
- **卸载重装保留**：module.json5 增加
  `metadata: ohos.fileshare.supportPreservePersistentPermission`（文档
  "持久化权限保留配置"，API 24 起生效；旧 API 忽略，无副作用）。API 23
  模拟器上不可验证，仅记档。
- **按需撤销（LRU 淘汰）**：文档要求用 `fileShare.revokePermission` 按需
  取消持久化授权。gpui-ohos 的授权历史此前满 256 条后直接拒绝新授权；现在
  历史文件升级为有序 `[uri, mode]` 数组（兼容读取旧 map 格式），新授权放不
  下时用 `revokePermission` 撤销最旧的授权腾位。
- **能力/权限降级**：顶层返回 `PermissionError`（缺 ACL）或
  `DeviceNotSupported`（无 FolderAuthorization 能力，等价 ArkTS
  `canIUse` 检查不过）时，记录一次可操作日志并停用本次会话内的授权持久化；
  Picker 的临时授权流程不受影响。

验证（DevEco HarmonyOS 6.1 x86_64 模拟器，全部通过）：

- `cargo check -p luban-ohos --target x86_64-unknown-linux-ohos` 通过；签名
  安装后 `bm dump` 显示 `FILE_ACCESS_PERSIST` 授予（`reqPermissionStates`
  为 0）、module metadata 含
  `ohos.fileshare.supportPreservePersistentPermission`。
- Picker 打开 `Documents.luban-workspace` 后强停并重启：全新进程无临时
  授权，工作区仍自动恢复（内容完整渲染），证明 persist→历史文件→
  check→activate 全链路生效；状态栏显示重启后 Ctrl+S 保存成功（写路径
  同样可用）。
- 淘汰路径：临时把 `MAX_GRANTS` 降到 3 重编译，连续打开 4 个不同文件，
  `luban.log`（帮助 → 打开日志目录可读）出现
  `File grant history is full; evicted 1 oldest grant(s), revoked 1`，
  与第 4 个文件的打开时间吻合，随后恢复 256。淘汰走的是
  `fileShare.revokePermission`。
- 观测提示：Rust 日志在应用沙箱 `logs/luban.log`（UTC 时间戳），shell 无
  root 读不了，用 帮助 → 打开日志目录 在应用内打开即可。
- API 23 模拟器无法验证卸载重装保留（API 24 起）。真机/AppGallery 需在
  AGC 为 `FILE_ACCESS_PERSIST` 申请 ACL。

## 2026-11-14 AGC 发布 profile 到位，发布签名接入 sign-hap.sh

AGC 审核通过并签发了 `prdRelease` profile（`app-distribution-type:
app_gallery`，type `release`，有效期 2026-11-14 → 2029-11-14），与本地
发布证书链（Root → Developer Relations CA G2 → `杨依宁(...),Release`，
ECC P-256）核对一致：

- bundle 名 `com.janeluo.lubannotepad` 与 `AppScope/app.json5` 相同；
- **两条 ACL 都批了**：`ohos.permission.READ_PASTEBOARD` 和
  `ohos.permission.FILE_ACCESS_PERSIST` 已出现在 profile 的
  `acls.allowed-acls`，意味着剪贴板与 picker 授权持久化在零售设备上
  不再需要额外申请；
- profile 内嵌分发证书与 `.cer` 链里的叶子证书逐字节相同。

`scripts/sign-hap.sh` 新增 AGC 发布模式（`--profile` + `--app-cert` +
`--keystore` + `--app-key-alias`）：不再从 SDK 模板铸造 profile，AGC 的
p7b 原样进 `sign-app`；签名前校验 profile 可验签、bundle 名一致、profile
内嵌证书属于 `--app-cert`。用法与守卫说明见
`crates/luban-ohos/README.md` 的 "AGC 发布签名" 一节。

**已完成（2026-11-14）**：版本号对齐 profile（`versionCode 2 /
versionName 2.0.0`，`AppScope/app.json5` 与 `app.template.json5` 同步），
arm64+x86_64 release HAP 用真实 AGC 材料签出
`entry-release-signed.hap`，`verify-app` 通过（signing block v2、
SHA-256 digest 全量一致、权限签名通过），签名内嵌 profile 与 AGC 原件
逐字节相同，证书链为 Root → Developer Relations CA G2 → 发布叶子证书。
工具怪癖记录：`verify-app` 的 `-outCertChain` 只认 `.cer` 后缀，
`-outProfile` 认 `.p7b`。

AppGallery 上架的 APP 包：`hvigorw assembleApp` 是**工程级**任务（带
`--mode module` 会报 task not found）。关键结论：**AGC 校验的是 APP
容器自身的签名块**——手工把签名 HAP 重打进容器会被 991（非法软件包）
拒绝，而官方流程里内嵌 HAP 反而保持未签名，`SignApp` 给 APP 容器打
v2 签名块。最终上传包
`ohos/build/outputs/default/ohos-default-signed.app` 由带 signingConfig
的 `assembleApp` 直接产出，`verify-app` 对容器校验通过（digest 全量
一致，内嵌 profile 与 AGC 原件逐字节相同）。

hvigor 6（modelVersion 6.0.0）签名配置写在 gitignore 的本机
`build-profile.json5`：字段嵌在 `material` 对象里（certpath/keyAlias/
keyPassword/profile/signAlg/storeFile/storePassword），且口令必须是
"签名材料库"加密后的 hex（≥32 字符），不接受明文。材料库路径由
`storeFile` 的上级目录推导（`<p12 所在目录>/material/`，含
`fd/`（3 份密钥份额）、`ac/`（salt）、`ce/`（加密的工作密钥）），
生成 key/CSR 时 DevEco 留下的材料库正好就在证书目录里。

后续真机/AppGallery 步骤：

1. `hdc install` 发布签名包到真机，验证安装、ACL 生效（`bm dump`）、
   Ctrl+V 与 picker 授权持久化；
2. 上架材料、AppGallery 上传校验（含版本号与 profile 的
   `version-code 2` 对齐策略）；
3. 覆盖安装旧签名包必须先卸载（数据会丢）。

## 2026-11-14 云测启动崩溃（SIGABRT）：显示 rotation 越界

AGC 上架测试在 HUAWEI MateBook Pro（HarmonyOS 6.1.0.135，arm64，2in1）
上报"应用启动失败"，cppcrash 日志为 SIGABRT@`libluban_ohos.so`，进程
存活 18s，栈在 `JsUIAbility::OnStart → @ohos-rs/ability initializeSession
→ ArkNativeFunctionCallBack` 之下全部落在我们的 .so 里（无符号）。

符号化过程（值得留档）：faultlogger 打印的 pc 是"运行时地址 - 映射基址"
= ELF vaddr（用 Maps 段 `5bcb400000` 基址核对过）；打包的 .so 被 strip，
只有 dynsym。用 `CARGO_PROFILE_RELEASE_STRIP=none ohrs build` 重出一
份带 symtab 的同源 .so（注意：同时改 `DEBUG` 会使段布局漂移 ~0x100，
只关 strip 则与线上版本仅差一个常量；以 `napi_register_module_v1` 的
dynsym/symtab 地址差 0x470 为锚做平移），`llvm-addr2line` 逐帧符号化。

结论：panic 链是 `openharmony_ability_mod::init`（OnceLock）→
`gpui_ohos::OhosPlatform::new → set_app` →
`ohos_display_binding::DisplayInfo::from_raw` →
`core::panicking::panic_fmt`，反汇编看到守卫是 `cmp w8, #4; b.lo`——
结构体偏移 88 的 `rotation` 字段（`NativeDisplayManager_Rotation`，
4 个变体）取值 ≥ 4。与 SDK 头文件 `oh_display_info.h` 逐字段核对，
布局一致，排除 ABI 错位；就是设备/云测环境返回了枚举范围外的值
（模拟器与 KaihongOS 虚机均为 0–3，从未触发）。panic 消息字符串
`unreachable!("Invalid attribute value")` 来自 `ohos-enum-derive`
生成的 `From<raw> for 枚举`。

修复：vendored `ohos-enum-derive`（见 `vendor/README.md`），raw→枚举
的 `_` 臂改为回退到第一个非 cfg 变体（Rotation→Rotation0 等），对全部
使用 `EnumFrom` 的 binding crate 生效——这是同一崩溃类第三次出现
（XComponent 键码、ArkUI 键码之后），从宏层面根治。验证：新 .so 中
`Invalid attribute value` 出现次数 5 → 0；重新 assembleApp 后容器
`verify-app` 通过、内嵌 profile 与 AGC 原件一致。真机显示 rotation
异常时按未旋转（ROTATION_0）处理，不再崩溃。

注意：符号化用的临时 .so（`CARGO_PROFILE_RELEASE_*` 环境变量构建）
不要用于发布；发布包一律用不带这些变量的标准 `ohrs build` 流程。

## 2026-11-15 AGC 拒审：检测到 Beta 版 API

上架检测报"您的应用使用了HarmonyOS beta版本的API"。这不是代码里调了
什么 beta 接口——AGC 读的是包体元数据：上一包 `pack.info` 里
`apiVersion.releaseType = "Beta1"`。来源是工具链：当时的
DevEco Studio 26.0.1.290（Beta 线）自带 **HarmonyOS 26.0.1 Beta1**
SDK（`sdk/default/sdk-pkg.json` 的 `stage`），hvigor 打包时原样写进
包体。26 线当时没有 Release（版本清单页 2026-06 快照里 26.0.0 只有
Beta1），Release 线最新是 6.1.1(24)；到 2026-11 重装时
**HarmonyOS 26.0.0 已转 Release**（sdk-pkg.json `stage: Release`，
version 26.0.0.105），于是直接用它。

修复（无任何代码改动，.so 与 ArkTS 原样）：

1. 重装 DevEco Studio **26.0.0.865**（Release 线，与旧版并排装在
   `D:\Program Files\Huawei\DevEco Studio 26.0.0\`）；旧安装目录随即
   失效，`D:\ohos-sdk-parent` / `D:\ohos-sdk` 两个 junction 重新指到
   新装的 `sdk\default` 与 `sdk\default\openharmony`（空间路径规避，
  junction 创建用 PowerShell `New-Item -ItemType Junction`）。
2. 本机 `build-profile.json5`（gitignored）products 增加
   `compileSdkVersion: "26.0.0"`、`targetSdkVersion` 由 `26.0.1` 改为
   `"26.0.0"`；`compatibleSdkVersion` 保持 `"6.1.0(23)"`（HarmonyOS
   版本串，= API 23，实测底线不变）。模板同步更新——26 起版本字段用
   点分串，低于 26 的 compatible 在 HarmonyOS runtimeOS 下仍是
   `x.y.z(api)` 版本串（OpenHarmony runtimeOS 下则是数字，见 CI
   profile）。
3. x86_64 `.so` 顺带重编：包内 x86_64 那份还是 11-12 的未打补丁构建
   （同款 rotation panic），云测跑 arm64 没暴露而已。
   `OHOS_NDK_HOME='D:\ohos-sdk-parent\openharmony' ohrs build --arch
   x86_64 --release` 后把 `dist/x86_64/{libluban_ohos.so,
   libc++_shared.so}` 拷进 `ohos/entry/libs/x86_64/`。
4. 构建（hvigorw 在 DevEco 内嵌 tools 下，node 用 PATH 上的即可）：

   ```sh
   cd ohos
   DEVECO_SDK_HOME='D:\Program Files\Huawei\DevEco Studio 26.0.0\sdk' \
     node "D:/Program Files/Huawei/DevEco Studio 26.0.0/tools/hvigor/bin/hvigorw.js" \
     -p product=default -p buildMode=release assembleApp --no-daemon
   ```

   口令在材料库里，signingConfig 原样可用，无需重输。
5. 校验：`pack.info` → `{compatible: 23, releaseType: "Release",
   target: 26}`；容器 `verify-app` 通过；内嵌 profile 与 AGC 原件
   逐字节相同；包内两个 ABI 的 .so 均无 `Invalid attribute value`。

工具怪癖补充：`verify-app` 的输入参数名是 **`-inFile`**（不是
`-appFile`，传错只会报语焉不详的 `11011005 Param is not trusted`）；
`-outCertChain` 只认 `.cer`、`-outProfile` 认 `.p7b`。

## 2026-11-15 深开鸿（KaihongOS）市场发布版本

增加第二个发行渠道：深开鸿 KaihongOS 应用市场。KaihongOS 是 OpenHarmony
发行版，与 AppGallery（HarmonyOS + AGC）是两套包体，档位、签名、产物全部
并行，AppGallery 链路零改动。

**签名材料现状（2026-11-15）**：开鸿开发者服务平台已签发发布证书
`深开鸿-证书/luban-padnote.cer`（issuer `CN=Kaihong Application Sub CA,
O=ShenKaiHong`，ECC P-256，Code Signing EKU，有效期 2026-11-15 →
2027-11-15，文件内含 Root → Sub CA → 叶子 3 级链）。关键发现：**这份证书
与 AGC 发布证书是同一把私钥**——`luban-notepad.csr` 同时上传给了 AGC 与
开鸿，公钥 SHA256 `04104337…b3f9` 在四处一致（CSR、`证书/luban-notepad.p12`
别名 `luban-notepad`、AGC 发布证书叶子与 `prdRelease.p7b` 内嵌分发证书、
开鸿证书叶子）。

**profile 直接用鸿蒙的**（用户查证 + 实测签名通过）：AGC 的
`prdRelease.p7b` 可以直接签深开鸿包——bundle 名一致、内嵌分发证书与 p12
同钥、两条 ACL 已在 profile 里，`sign-hap.sh` 三道守卫加 key↔cert 匹配全
过，产出
`dist-release/kaihongos/luban-notepad-2.0.0-kaihongos-agcprofile-signed.hap`。
设备与开鸿后台是否认华为信任链待验证（虚机安装/后台上传）；若后台要求
开鸿自签 profile，把命令里的 `--profile` 换成开鸿签发的 p7b 即可，其余参
数不动。签名统一走 `sign-hap.sh` 发布材料模式（措辞已放宽为"市场发布材
料"，守卫不变）。注意 p12 里没有证书链，hap-sign-tool 的 `-appCertFile`
用 `.cer` 提供。

**档位**：`ohos/build-profile.kaihongos.json5` —— `runtimeOS: OpenHarmony`、
`compileSdkVersion/targetSdkVersion: "26.0.0"`（本机 DevEco 26.0.0.865
自带的 OpenHarmony 26.0.0 Release SDK，点分串；Release stage 写进
pack.info，AGC 拒审 Beta 的教训对所有检查包体的市场适用）、
`compatibleSdkVersion: 15`（数值，`@ohos-rs/ability` 窗口接口的 API
下限；KaihongOS 6.1 = API 23 可安装）。API 26+ 的符号（如
`zLevelAboveParentLoosened`）带运行时 `sdkApiVersion` 守卫，不会在
API 23 上执行。没有走 11-11 的 API 23 组合（API 23 SDK + hvigor 5.18.5 +
modelVersion 5.1.0 + 3 处 `@ohos-rs/ability` 本地补丁）是因为那需要整套
独立工具链；若 6.1 虚机拒装 target 26 的包，后备方案就是切回该组合，
只需改这一个档位文件。

**构建**：`scripts/build-kaihongos-hap.sh` 一次完成 ohrs 双 ABI（arm64 +
x86_64，`build-hap.sh` 新增 `--abi arm64|x86_64|both`，默认 arm64 与 CI
一致）、ohpm、hvigor `assembleHap`（`--mode module`）、pack.info 断言
（Release / compatible 15 / target 26 / 双 ABI .so），产物落到
`dist-release/kaihongos/`（与 AppGallery 的 `ohos/build/outputs` 分离）。
本机用法：

```sh
export OHOS_NDK_HOME='D:\ohos-sdk-parent\openharmony'
export OHOS_BASE_SDK_HOME='D:\ohos-ohsdk-26'   # junction 树，见下方 SDK 解析坑
export HVIGORW='D:/Program Files/Huawei/DevEco Studio 26.0.0/tools/hvigor/bin/hvigorw.js'

crates/luban-ohos/scripts/build-kaihongos-hap.sh            # 未签名 HAP
crates/luban-ohos/scripts/build-kaihongos-hap.sh --sign     # + SDK 测试材料（KaihongOS 6.1 虚机验收）
crates/luban-ohos/scripts/build-kaihongos-hap.sh --sign -- \
  --profile 'D:\工作文件\00. 简洛\luban-notepad\证书\luban-notepad-prdRelease.p7b' \
  --app-cert 'D:\工作文件\00. 简洛\luban-notepad\证书\luban-notepad - 发布证书.cer' \
  --keystore 'D:\工作文件\00. 简洛\luban-notepad\证书\luban-notepad.p12' \
  --app-key-alias luban-notepad
```

**SDK 解析坑（00308018，2026-11-15 实测）**：hvigor 6 对 OpenHarmony
runtimeOS 产品的 SDK 只认 `local.properties` 的 `sdk.dir` 或
`OHOS_BASE_SDK_HOME`（`DEVECO_SDK_HOME` 只服务 HarmonyOS 侧产品）；把这两者
指到 DevEco 托管的 `sdk/default` 根会报 "The SDK management mode has
changed"——托管根里的统一布局组件（`oh-uni-package.json`）对独立 hvigor 是
旧模式，`local.properties` 也一样。解决办法是 CI 同款外部布局：用 junction
把 DevEco 的 OpenHarmony 26.0.0 Release 组件（`releaseType: Release`，正是
市场要的 stage）拼成 `D:\ohos-ohsdk-26\26.0.0\{ets,js,native,previewer,
toolchains}`（PowerShell `New-Item -ItemType Junction`），`OHOS_BASE_SDK_HOME`
指向 `D:\ohos-ohsdk-26`，不要写 `local.properties`。

已完成的链路验证：构建 → `pack.info` 断言（Release/15/26/双 ABI）→ SDK
测试材料签名成功（`luban-notepad-2.0.0-kaihongos-signed.hap`，bundle 与两条
ACL 正确）；`ohos/build-profile.json5` 在成功与失败路径上都逐字节还原。

**防误伤 AppGallery**：本机 `ohos/build-profile.json5` 是 DevEco 手工维护
的 AGC 签名配置，`build-hap.sh --profile` 会覆盖它。现在 `build-hap.sh`
覆盖前先备份到 `build-profile.json5.bak`；`build-kaihongos-hap.sh` 另有
快照 + trap 还原 + 哈希一致性校验，构建成功、失败或中断后 AGC 配置都必然
复原。`AppScope/app.json5`（bundle 名、versionCode 2 / versionName 2.0.0）
与 `build-profile.template.json5` 两个市场共用、均未改动——versionCode
命名空间按市场独立，共用数字没有影响。

**上传格式（2026-11-15 实测确定）**：开鸿开发者平台"发布版本软件包"仅收
**zip（≤2G）**，且解包后在 zip 根目录找配置文件——直接把签名 HAP 塞进
zip 会报"软件包未找到配置文件"。正确形态是 **APP 容器**：`assembleApp`
（工程级，`build-kaihongos-hap.sh --app` 已支持）出
`ohos-default-unsigned.app` → `sign-hap.sh` 用 AGC 材料签容器（内嵌 HAP
保持未签名，与 AGC 流程一致）→ `verify-app` 通过、内嵌 profile 与 AGC
原件逐字节相同 → 签名后的 `.app` 本身就是 zip（根目录 `pac.json` +
`pack.info` + `entry-default.hap`），改名 `.zip` 直接上传。产物
`dist-release/kaihongos/luban-notepad-2.0.0-kaihongos.zip`。

**待办**：

1. ~~KaihongOS 6.1 虚机验证~~ **已完成（2026-11-15，见下一节）**：AGC
   profile 签名的 HAP 在 KaihongOS 6.1 上安装、启动、交互全部通过；
2. 开鸿后台上传校验；若后台要求开鸿自签 profile，等平台签发 p7b 后把签名
   命令里的 `--profile` 换掉重签（keystore/别名/证书已齐）；
3. KaihongOS 5.0.2（API 14）仍不支持——`@ohos-rs/ability` 的 ArkTS 需要
   API 15+，`compatibleSdkVersion 15` 已是下限。

## 2026-11-15 KaihongOS 6.1 QEMU 虚机：AGC 签名包安装/运行验证通过

**结论（本节最重要的一句）：KaihongOS 6.1 的包管理器接受华为 AGC 的发布
证书链 + profile**。`bm install` AGC-profile 签名的
`luban-notepad-2.0.0-kaihongos-agcprofile-signed.hap` 直接成功，`bm dump`
显示 `appProvisionType: release`，`aa start` 启动、GPUI/virgl 渲染、菜单
交互全部正常。"鸿蒙 profile 直接用于开鸿市场"从查证升级为设备实证。

### QEMU 环境（`D:\dev\KaihongOS6.1\qemu\launch_qemu.bat`，已改好）

在官方包的 `launch_qemu.bat` 基础上改了四处，其余照官方 `QEMU_USAGE.md`：

1. **路径**：`DISK_IMG`/`ISO_IMG`/`QEMU` 指向本机位置；磁盘
   `qemu-img create -f raw disk.img 64G`。
2. **固件必须走 pflash，不能用 `-bios`**：本机的 QEMU 11.1 构建
   （`v11.1.0-12130`）对任何显式 `-bios` 路径都报 "could not load PC
   BIOS"（默认 SeaBIOS 与块层文件打开都正常，唯独 `-bios` 的 access()
   解析坏了）。解法：
   `-drive if=pflash,format=raw,readonly=on,file=OVMF.fd` +
   `-drive if=pflash,format=raw,file=OVMF_VARS.fd`（两个 .fd 从
   `<qemu>\share\edk2-x86_64-code.fd` / `edk2-i386-vars.fd` 拷贝）。
3. **自动化通道**：`-monitor tcp:127.0.0.1:4444,server,nowait` +
   `-qmp tcp:127.0.0.1:4445,server,nowait`（QMP `input-send-event` 注入
   绝对坐标点击/键盘，`screendump` 在 UEFI 阶段可用）；hdc 端口转发
   `-netdev user,id=n0,hostfwd=tcp:127.0.0.1:10178-:10178 -device
   virtio-net-pci,netdev=n0`。
4. **显示**：官方 `sdl,gl=on` 可用（guest 渲染正常），但 QMP `screendump`
   在 guest 进 GL 后报 "no surface"；`egl-headless,gl=on` 下 guest 进 GL
   后同样没有 surface；`gl=readback` 该构建不认；`-vnc` 与 GL 上下文互斥。
   拿画面的可靠办法是 **Windows GDI 窗口截屏**（PowerShell
   GetWindowRect + CopyFromScreen，脚本见
   `~/.zcode/tmp/kaihong-vm/capture.ps1`）——bash 会话就是控制台会话，
   窗口在用户桌面上可见，人可以直接操作，机器靠 QMP 注入。

### 安装与引导

- 首次开机 OVMF 落到 UEFI Shell（不自动引导 ISO）：`map -r` 看 FS 别名，
  `fs0:\efi\boot\bootx64.efi`（全小写，UEFI Shell 大小写不敏感）进 grub。
  QMP 注入键盘时**必须放慢节奏**（每键 ≥60ms），否则 guest 键盘缓冲丢键。
- 安装器（欢迎→分区→摘要→安装→结束）和首次 OOBE 是图形交互，人工点完
  最快；OOBE 的"登录密码"必须设置（≥6 位），跳过会卡死。
- 安装后 guest 关机，QEMU 窗口标题变 `[Stopped]`——正常，重跑
  launch_qemu.bat 即进系统（grub 会弹菜单选 QEMU 项）。
- 装好的系统 hdcd 自启，`hdc tconn 127.0.0.1:10178` 直连（hostfwd 生效）；
  内核日志走串口（任务输出文件可看）。

### 验证过程与结果

1. `hdc file send <hap> /data/local/tmp/…`（注意 Git Bash 要
   `MSYS_NO_PATHCONV=1`，否则远端路径被转成 `C:/Program Files/Git/...`；
   本地路径用纯反斜杠）→ 84MB 约 1.6s。
2. `bm install -p /data/local/tmp/luban-kaihongos-agcprofile.hap` →
   **install bundle successfully**（华为 Root CA G2 → Developer Relations
   CA G2 → 杨依宁 Release 的链 + AGC prdRelease profile 被 KaihongOS
   接受）。
3. `aa start -a EntryAbility -b com.janeluo.lubannotepad` → 启动成功，
   编辑器完整渲染（菜单/工具栏/标签页/行号），人工交互（偏好设置、切
   简体中文）正常；`appProvisionType: release`。
4. guest 分辨率 1920x1080（fb0），网络 eth0(virtio_net) 10.0.2.15，网关
   与公网（baidu/223.5.5.5）ping 全通。

**OOBE"网络连接失败"排查记录**：弹窗报错时 guest 网络实际完全正常
（DNS 解析 + 公网 ICMP 都通），判定为 Kaihong 云端验证服务侧/其网络状态
检查的问题，与 QEMU 网络配置无关；手动跳过后不影响后续使用。

**未验项**：KaihongOS 应用中心（市场后端）的上传校验仍待真流程；中文
IME 提交与系统 picker 在 KaihongOS 上的表现沿用 11-11/11-12 的已知限制
记录。

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




# luban-ohos

OpenHarmony（含 HarmonyOS）入口 crate。桌面的可执行文件仍在 `luban-app`；
这里只生成供 ArkTS `NativeAbility` 加载的 `cdylib`。

```
crates/luban-ohos
├── src/ohos_entry.rs      # #[ability] 入口：注入 gpui-ohos 平台后调用 luban_app::run_embedded
├── ohos/                  # Stage 模型 HAP 工程（hvigor）
├── scripts/build-hap.sh   # ohrs 交叉编译 + 拷贝 .so / 类型声明
└── scripts/sign-hap.sh    # 生成带 ACL 的 profile 并签名 HAP
```

## 组成

- **平台后端**：`gpui-ohos`（vendored 在 `vendor/gpui-ohos`，固定到上游
  `324abb33`）。窗口、触摸/鼠标/键盘、IME、剪贴板、系统菜单、文件选择器与
  持久授权都由它提供。
- **编辑器**：与桌面完全相同的 `luban-app` 库。`run_embedded` 复用同一套
  启动逻辑，只是不解析命令行、不启动单实例 IPC。
- **ArkTS 宿主**：`ohos/entry` 里的 `EntryAbility` 注册 gpui-ohos 需要的
  plugin（AppControl/Clipboard/Files/Menu/Notification/Permission/
  SystemState/Url/Window），`Index.ets` 用 `DefaultXComponent` 承载 GPUI 画面。

## 构建

依赖：

- OpenHarmony SDK / command-line tools（API 26）
- `OHOS_NDK_HOME` 指向 NDK
- `rustup target add aarch64-unknown-linux-ohos`
- [`ohrs`](https://github.com/ohos-rs/ohrs) 在 `PATH` 中

```sh
crates/luban-ohos/scripts/build-hap.sh --release
cd crates/luban-ohos/ohos && hvigorw assembleHap
```

`build-hap.sh` 会把 `libluban_ohos.so`、`libc++_shared.so` 和自动生成的
`Index.d.ts` 放进 HAP 工程，并从模板生成 `build-profile.json5` 与
`AppScope/app.json5`（这两个文件带本机签名配置，已在 `.gitignore` 中）。

## 签名（模拟器 / 开发机）

`hvigorw assembleHap` 产出的是 `entry-default-unsigned.hap`，没有签名 profile，
因此拿不到 `READ_PASTEBOARD` 的 ACL 授权。模拟器/OpenHarmony 开发镜像可以用
SDK 自带的测试材料签名：

```sh
export OHOS_SDK_LIB="<DevEco SDK>/default/openharmony/toolchains/lib"
export OHOS_SIGN_STORE_PASSWORD="<SDK 测试密钥库口令>"
crates/luban-ohos/scripts/sign-hap.sh --install --device <serial>
```

脚本会从 `UnsgnedReleasedProfileTemplate.json` 生成
`acls.allowed-acls = ["ohos.permission.READ_PASTEBOARD"]` 的 profile，用
`hap-sign-tool.jar` 签出 `profile.p7b`，再用 SDK 的证书链签出
`entry-default-signed.hap`。`--install` 会先卸载同 bundle 的旧包再安装
（应用数据会丢失）；不加 `--install` 只生成签名包。路径、口令和设备序列号
只通过参数或环境变量传入，仓库里不保存任何密钥或本机路径。脚本需要
`python3`/`python` 生成 profile JSON；Java/keytool 默认使用 DevEco 自带的
JBR。

真机和 AppGallery 必须使用 AGC 签发的证书/profile；OpenHarmony 测试根
不适用于零售设备。ACL 申请和发布签名见 `docs/HARMONYOS.md`。

## 权限

`module.json5` 声明了：

| 权限 | 用途 |
| --- | --- |
| `ohos.permission.READ_PASTEBOARD` | 粘贴（user_grant，首次前台请求） |
| `ohos.permission.FILE_ACCESS_PERSIST` | 跨重启保留 picker / 拖放 / URI 授权 |
| `ohos.permission.INTERNET` | 插件商店与更新检查 |

`PlatformOptions::persist_file_access` 已开启；授权恢复在首帧之后异步开始，
因此会话恢复必须容忍“最近文件暂不可读”，在授权恢复完成后再重试。

`READ_PASTEBOARD` 是 `system_basic + user_grant`：签名 profile 的
`allowed-acls` 只负责放行，应用仍需在 `EntryAbility.onForeground` 里调用
`requestPermissionsFromUser`。两者缺一不可。`Edit → Paste` 走 ArkUI 原生
粘贴控件，没有该权限也能工作；Ctrl+V 走原生剪贴板读取，必须两者都满足。

## 当前状态

- `cargo check --target aarch64-unknown-linux-ohos -p luban-ohos` 通过
  （PC 侧 `cargo check/test --workspace` 也通过）。
- 模拟器已验证：签名 HAP 安装、profile ACL、`READ_PASTEBOARD` 运行时授权、
  Ctrl+C/Ctrl+V 粘贴。
- 尚未在真机验证：链接（需要真实 NDK）、IME/触摸、picker 授权恢复、
  GLES 渲染差异；真机签名/ACL 还需要 AGC 证书与 profile。
- `targetSdkVersion`/`compatibleSdkVersion` 暂时是 `26.0.0`（与 gpui-ohos
  示例一致）。是否下调研判中，见 `docs/HARMONYOS.md`。

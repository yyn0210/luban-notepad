# luban-ohos

OpenHarmony（含 HarmonyOS）入口 crate。桌面的可执行文件仍在 `luban-app`；
这里只生成供 ArkTS `NativeAbility` 加载的 `cdylib`。

```
crates/luban-ohos
├── src/ohos_entry.rs      # #[ability] 入口：注入 gpui-ohos 平台后调用 luban_app::run_embedded
├── ohos/                  # Stage 模型 HAP 工程（hvigor）
└── scripts/build-hap.sh   # ohrs 交叉编译 + 拷贝 .so / 类型声明
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

## 权限

`module.json5` 声明了：

| 权限 | 用途 |
| --- | --- |
| `ohos.permission.READ_PASTEBOARD` | 粘贴（user_grant，首次使用时弹窗） |
| `ohos.permission.FILE_ACCESS_PERSIST` | 跨重启保留 picker / 拖放 / URI 授权 |
| `ohos.permission.INTERNET` | 插件商店与更新检查 |

`PlatformOptions::persist_file_access` 已开启；授权恢复在首帧之后异步开始，
因此会话恢复必须容忍“最近文件暂不可读”，在授权恢复完成后再重试。

## 当前状态

- `cargo check --target aarch64-unknown-linux-ohos -p luban-ohos` 通过
  （PC 侧 `cargo check/test --workspace` 也通过）。
- 尚未在真机验证：链接（需要真实 NDK）、HAP 签名、IME/触摸/剪贴板、
  picker 授权恢复、GLES 渲染差异。这些是 Phase 0 的 G1/G4/G5 验收项。
- `targetSdkVersion`/`compatibleSdkVersion` 暂时是 `26.0.0`（与 gpui-ohos
  示例一致）。是否下调研判中，见 `docs/HARMONYOS.md`。

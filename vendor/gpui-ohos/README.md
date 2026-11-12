# gpui-ohos

OpenHarmony platform backend for GPUI, with native windows, GPU rendering,
touch and mouse input, keyboard and IME support, and system integrations.

## Usage

Create the GPUI application with the OpenHarmony platform:

```rust
let application = gpui::Application::with_platform(
    gpui_ohos::current_platform(openharmony_app, false),
);
```

Pass the Ability's `OpenHarmonyApp` as `openharmony_app`. To configure platform
options (such as persistent file access or explicit headless mode) or access
shared platform services (such as `FontCatalog`), use `try_current_platform` or
`PlatformServices::try_new`:

```rust
let platform = gpui_ohos::try_current_platform(
    openharmony_app,
    gpui_ohos::PlatformOptions {
        persist_file_access: true,
        ..Default::default()
    },
)?;
```

Configure the Ability and its ArkTS plugins as shown in the
[example entry Ability](./example/ohos/entry/src/main/ets/entryability/EntryAbility.ets).
To load bundled HAP `resources/rawfile` assets or fonts without intermediate FFI
buffer copies, register `ResourcePlugin` on the ArkTS Ability and attach
`RawFileAssets`:

```rust
let assets = gpui_ohos::RawFileAssets::from_app(openharmony_app.clone());
let application = gpui::Application::with_platform(
    gpui_ohos::current_platform(openharmony_app, false),
)
.with_assets(assets);
```

## Features

| Cargo feature | Purpose |
| --- | --- |
| `gles` | OpenGL ES rendering (enabled by default). |
| `vulkan` | Vulkan rendering. |
| `fast` | Official `gpui-fast` core with scroll tiles and analytic rounded clipping; see [Fast rendering](docs/fast.md). |
| `native-vsync` | Use NativeVSync frame callbacks instead of XComponent callbacks. |
| `audio` | Opt-in owned-file audio playback (`NativeAudioService` / `AudioBridgePlugin`). |
| `grid` | Experimental fixed-cell terminal shaping (`FontCatalog::layout_grid`). |
| `native-input` | Opt-in native pointer coordinates (`NativePointerPosition`) and wheel metadata (`NativeScrollDelta`). |
| `api-19` | Enable API-19 CapsLock / NumLock / ScrollLock state snapshots on keyboard events. |
| `api-26` | Enable `api-19` and configure native hardware-key consumption before IME attachment (API 26+). |

### Optional NativeVSync delivery

Touch scrolling uses native gesture recognition and supports momentum scrolling.
Frames are requested on demand through XComponent callbacks by default. Enable
`native-vsync` to select NativeVSync for all windows at build time:

```sh
cargo build --target aarch64-unknown-linux-ohos --features native-vsync
```

NativeVSync uses the display refresh rate reported by the system. Both modes
register callbacks on demand and release them when windows become hidden or idle.

## Example

The [example app](./example/README.md) demonstrates windows, text input,
scrolling, menus, file pickers, clipboard, credentials, screen capture,
notifications, and application lifecycle.

```bash
cd example
sh scripts/build-native.sh --release
```

For desktop or 2in1 targets, prefix the build command with
`OHOS_DEVICE_TYPE=desktop`. See the [example README](./example/README.md) for
signing and building the HAP.

## Platform notes

- Declare app identity and URL schemes in `app.json5` and `module.json5`.
  `register_url_scheme()` validates the installed declaration.
- Clipboard reads require `ohos.permission.READ_PASTEBOARD`, an ACL-enabled
  signing profile, and runtime permission. Persistent file access requires
  `ohos.permission.FILE_ACCESS_PERSIST`; transparent main windows may require
  `ohos.permission.SET_WINDOW_TRANSPARENT`.
- Hiding other applications, dock menus, and recent-document lists are
  unavailable through the application SDK.
- Native IME configuration does not expose autocorrection, capitalization,
  or suggestion controls. Missing hardware key mappings use a US layout.

For implementation details, see [touch input and frame scheduling](./docs/touch-input.md)
and [OHOS adaptation performance](./docs/ohos-performance.md).

## License

[Apache-2.0](./LICENSE-APACHE) or [MIT](./LICENSE-MIT)

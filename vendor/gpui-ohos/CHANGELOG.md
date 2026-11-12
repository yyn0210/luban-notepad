# Changelog

## Unreleased

### Breaking

- Audio playback is now behind the nondefault `audio` Cargo feature.
  `AudioBridgePlugin`, `NativeAudioService`, `NativeAudioPlayer`,
  `NativeAudioPlayerOptions` and `NativeAudioError` are unavailable without it.
  Existing audio consumers must enable `audio` and continue registering the Rust
  facade and matching ArkTS `AudioPlugin` explicitly. Owned-file acknowledgement
  and command ordering are unchanged.
- `NativePointerPosition` and `NativeScrollDelta` now require the nondefault
  `native-input` Cargo feature. Their modules and thread-local recording are
  excluded from default builds. Ordinary GPUI input is unchanged by this gate.
- Unknown or nonpositive wheel `scroll_step` no longer fabricates unscaled
  detents: `NativeScrollDelta::get` returns `None`. Generic GPUI line deltas
  remain available.

- `PremultipliedImage::new`, `RasterImage::straight`, and
  `RasterImage::premultiplied` now return `Result<Self, ImageRegistrationError>`.
  Handle the result (for example, with `?`) instead of assuming registration
  succeeds. Conflicts identify the image and both `ImagePolicy` values; the
  existing registration remains unchanged.

- Fixed-cell terminal shaping now requires the nondefault `grid` Cargo feature.
  This includes `FontCatalog::layout_grid`, `GridLayoutRequest`,
  `GridLayoutError`, and `GridTextCell`. Enable `gpui-ohos/grid` in terminal
  consumers; ordinary GPUI text layout is unchanged.
- `FontCatalog`, `PlatformServices`, `FontFaceData`, `FontDesignCoordinate`, and
  `SyntheticFontStyle` are explicitly experimental APIs whose signatures may
  change between releases. Runtime face identities and registered bytes remain
  immutable.
- Align with the native-only clipboard facade: remove HAR constructor options
  and implicit authorization reads; applications must arrange paste access or
  explicitly request `READ_PASTEBOARD` before native reads. Configure the existing
  GPUI metadata MIME in Rust. Clipboard failures are typed, native data IPC runs
  on the background executor, and there is no ArkTS data/observer transport.
- Plain text/HTML cannot contain embedded NUL; empty opaque values, including
  empty custom metadata, are unsupported. Do not replace them with absent values
  or private encodings. API-18 clipboard revisions are pull queries; zero/errors
  invalidate the cache and service restart can decrease the counter.
- Consume Ability's single `ArkUiInputEvent::Key` stream and `ime_consumed`
  disposition. Remove pre/post-IME receipts, timestamp deduplication, Escape
  replay, the Ctrl-Space interception policy and input-callback foreground-task
  pumping. IME attachment transfer now uses Ability's UI-thread owner APIs.
- Add opt-in `api-19` and `api-26` features; the example enables `api-26`.
  Native key consumption is configured explicitly before attachment. Linked
  API-19/API-26 symbols may raise the application's minimum runtime requirement.
- File grant persistence is now disabled by default, including through
  `current_platform`. Applications that need cross-restart file access must use
  `try_current_platform` with `PlatformOptions::persist_file_access = true` and
  declare `ohos.permission.FILE_ACCESS_PERSIST`. Struct literals for
  `PlatformOptions` need the new field or `..Default::default()`.
- Restoration starts after the first successfully presented GPUI frame, without
  delaying GPU initialization or the launch callback. Restored workspace I/O must
  tolerate startup before grants are activated.

### Fixed

- Restore the pre-pointer-scroll native wheel and touchpad signs. Discrete
  wheels continue to use GPUI line units with the system scroll-step coefficient;
  touchpads retain device-pixel scaling. Zero-delta axis updates are ignored
  except for gesture start and end.
- Scroll direction has not been verified on hardware. Mouse-wheel and touchpad
  acceptance, including natural scrolling on and off, remains required.
- Empty image-policy atlas metadata skips GPU sprite copies and per-sprite hash
  lookups. Uploaded tiles retain their policy until removal even after their
  registration's final owner is dropped.
- Matching registrations and clones share lifetime-bound cleanup. Registry locks
  are released before upgraded owners can be dropped.
- Retain clipboard serialization until native completion even if a caller drops
  its write/read task. Invalidate cache after writes instead of attributing a
  potentially interleaved external write's counter to our old item. Preserve
  encoded image bytes and identify their format instead of labeling every image
  PNG; the existing demo now checks exact entries and stops on permission denial.
- Keep native menubar accelerator labels display-only so focused GPUI keymaps
  retain precedence. Existing focus-restoring window-command popups carry no
  accelerator. The IME demo counts an explicit Ctrl-Shift-M context override of
  the same shortcut displayed on the existing Count menu action.
- Bound file-access history and early selections to 256 grants, history reads to
  2 MiB, URI length to 4096 bytes and permission IPC to batches of 64. Report
  malformed, oversized and full histories without discarding selected paths.
- Request read-only grants for open pickers, incoming file URIs and accepted drops;
  only save pickers request read/write. Preserve successful persistence and report
  partial provider, activation and storage failures.
- Remove Rust dependencies into `example/` and align Rust/HAR source to the
  explicitly authorized combined Ability #98 revision
  `0e41a3141d1053e9a5172738538d725b050e2d36` (with `ohos-native-bindings` unified
  at `43aec6c125503a47f7e875c793aaf2462e91ad19`). Ability #93-#98 and GPUI core #3
  remain unmerged release blockers.

The private GPU padding-field encoding remains a bounded workaround pending an
upstream `RenderImage` alpha/sampling API; alpha math and default linear filtering
are unchanged.

//! The `NativeAbility` callback that ArkTS registers in `EntryAbility.ets`.
//!
//! The Ability passes its `OpenHarmonyApp` handle here; GPUI platform services
//! (window, input, IME, clipboard, pickers, persistence of file grants) come
//! from `gpui-ohos`. The editor itself is the same `luban-app` library the
//! desktop binary runs.

use luban_app::{run_embedded, EmbeddedStartup};
use openharmony_ability::OpenHarmonyApp;

#[openharmony_ability_derive::ability]
fn openharmony_app(app: OpenHarmonyApp) {
    // The OHOS process starts with HOME=/storage/Users/currentUser, which the
    // app sandbox cannot write. Point the editor's settings, session, local
    // history and plugin store at the writable app files directory before
    // `run_embedded` loads them.
    if std::env::var_os("LUBAN_NOTEPAD_CONFIG_DIR").is_none() {
        let config_dir = std::path::PathBuf::from("/data/storage/el2/base/files/luban-notepad");
        let _ = std::fs::create_dir_all(&config_dir);
        std::env::set_var("LUBAN_NOTEPAD_CONFIG_DIR", &config_dir);
    }

    // Persist picker/drag/URI grants across restarts. The OHOS module manifest
    // must declare `ohos.permission.FILE_ACCESS_PERSIST`; grant restoration
    // starts in the background after the first presented frame.
    let platform = gpui_ohos::try_current_platform(
        app,
        gpui_ohos::PlatformOptions {
            persist_file_access: true,
            ..Default::default()
        },
    )
    .expect("failed to initialize the OpenHarmony platform");

    let application = gpui::Application::with_platform(platform);
    run_embedded(application, EmbeddedStartup::default());
}

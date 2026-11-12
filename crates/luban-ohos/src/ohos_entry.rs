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

    // The system picker grants URI access, not directory access, so saves to
    // user storage go through the ArkTS `fs` plugin instead of POSIX writes.
    crate::file_io::register(&app).expect("failed to register the OHOS file I/O plugin");
    let writer_app = app.clone();
    let _ = luban_app::ohos_file_save::set_writer(Box::new(move |path, bytes| {
        crate::file_io::write_uri(writer_app.clone(), path, bytes)
    }));

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

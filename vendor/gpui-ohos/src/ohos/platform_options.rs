#[derive(Clone, Copy)]
pub struct PlatformOptions {
    /// Disable native menus when the application renders and dispatches its own menus.
    pub native_menus: bool,
    /// Retain picker, open-URI and accepted drop grants across application restarts.
    ///
    /// Disabled by default. Requires `ohos.permission.FILE_ACCESS_PERSIST`.
    /// Restoration starts in the background after the first presented GPUI frame.
    pub persist_file_access: bool,
}

impl Default for PlatformOptions {
    fn default() -> Self {
        Self {
            native_menus: true,
            persist_file_access: false,
        }
    }
}

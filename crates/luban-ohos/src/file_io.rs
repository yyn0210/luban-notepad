//! ArkTS file I/O bridge for system-picker URIs.
//!
//! The OpenHarmony document picker grants access to a chosen file through its
//! `file://docs/...` URI. The application sandbox may not open that path with
//! POSIX `open(2)`, so the save flow hands the encoded document to the ArkTS
//! `luban.file-io` plugin, which opens the URI with `@ohos.file.fs` and writes
//! through the returned descriptor.

use std::{future::Future, io, path::PathBuf, pin::Pin};

use napi_derive_ohos::napi;
use napi_ohos::Result as NapiResult;
use openharmony_ability::{
    impl_bridge_napi_type, AsyncBridge, BridgeCallOptions, BridgeContextRequirement, BridgePlugin,
    OpenHarmonyApp,
};

/// ArkTS plugin that owns URI writes for this application.
pub struct FileIoBridgePlugin;

impl BridgePlugin for FileIoBridgePlugin {
    type Mode = AsyncBridge;
    const ID: &'static str = "luban.file-io";
    const REQUIRED_CONTEXTS: &'static [BridgeContextRequirement] =
        &[BridgeContextRequirement::Ability];
}

/// Request for [`write_uri`]: a document-picker URI plus the encoded bytes.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct WriteUriRequest {
    pub uri: String,
    pub bytes: Vec<u8>,
}

impl_bridge_napi_type!(WriteUriRequest, "luban.file_io.WriteUriRequest");

/// Number of bytes the ArkTS writer accepted.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct WriteUriResponse {
    pub bytes_written: u32,
}

impl_bridge_napi_type!(WriteUriResponse, "luban.file_io.WriteUriResponse");

/// Register the Rust half of the plugin during ability initialization.
pub fn register(app: &OpenHarmonyApp) -> NapiResult<()> {
    app.register_plugin(FileIoBridgePlugin)
}

/// Write `bytes` to `path` through its picker URI.
///
/// This future is safe to await on the GPUI foreground executor: the ArkTS
/// callback runs on the same event loop, and `fs.open`/`fs.write` are awaited
/// there, so the UI is not blocked.
pub fn write_uri(
    app: OpenHarmonyApp,
    path: PathBuf,
    bytes: Vec<u8>,
) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send>> {
    Box::pin(async move {
        let path = path.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "OpenHarmony file path is not UTF-8",
            )
        })?;
        let uri = ohos_fileuri_binding::get_uri_from_path(path).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("convert path to file URI: {error:?}"),
            )
        })?;
        let bridge = app
            .bridge()
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))?;
        let response = bridge
            .call_async::<FileIoBridgePlugin, WriteUriRequest, WriteUriResponse>(
                "write-uri",
                WriteUriRequest { uri, bytes },
                BridgeCallOptions::for_user_interaction(),
            )
            .await
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))?;
        if response.bytes_written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "ArkTS file writer wrote no bytes",
            ));
        }
        Ok(())
    })
}

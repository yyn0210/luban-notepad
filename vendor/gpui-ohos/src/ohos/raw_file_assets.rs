//! Zero-intermediate-allocation HAP `rawfile` reader and GPUI [`AssetSource`].
//!
//! Uses `ohos_resource_manager_binding::RawFile::read_into` so asset and font
//! payloads stream directly into caller-owned buffers or a single pre-reserved
//! `Vec<u8>` without allocating temporary per-chunk `Vec`s in the FFI layer.
//! All native `NativeResourceManager` calls are serialized behind a mutex
//! because the underlying OpenHarmony C NDK handle is not thread-safe.

use std::{borrow::Cow, sync::Arc};

use anyhow::{bail, ensure, Context as _, Result};
use ohos_resource_manager_binding::{
    RawFile, RawFileError, ResourceManager as NativeResourceManager,
};
use openharmony_ability::OpenHarmonyApp;
use openharmony_ability_plugin_resource::ResourceExt as _;
use parking_lot::Mutex;

use crate::{AssetSource, PlatformTextSystem, SharedString};

const DEFAULT_MAX_ASSET_BYTES: usize = 128 * 1024 * 1024;
static APP_RESOURCE_IO_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone)]
enum ManagerBackend {
    App(OpenHarmonyApp),
    Native(Arc<Mutex<NativeResourceManager>>),
}

/// Thread-safe HAP `resources/rawfile` reader implementing [`gpui::AssetSource`].
#[derive(Clone)]
pub struct RawFileAssets {
    backend: ManagerBackend,
    prefix: String,
    max_asset_bytes: usize,
}

impl RawFileAssets {
    /// Creates an asset source backed by the `ResourceBridgePlugin` registered on `app`.
    ///
    /// Register [`openharmony_ability_plugin_resource::ResourceBridgePlugin`] on the Rust
    /// side and `ResourcePlugin` in the ArkTS `EntryAbility.bridgePlugins` list so the
    /// native resource manager is pushed during Ability installation.
    pub fn from_app(app: OpenHarmonyApp) -> Self {
        if matches!(
            app.registered_plugin::<openharmony_ability_plugin_resource::ResourceBridgePlugin>(),
            Ok(None)
        ) {
            let _ = app
                .register_plugin(openharmony_ability_plugin_resource::ResourceBridgePlugin::new());
        }
        Self {
            backend: ManagerBackend::App(app),
            prefix: String::new(),
            max_asset_bytes: DEFAULT_MAX_ASSET_BYTES,
        }
    }

    /// Creates an asset source from an owned [`ohos_resource_manager_binding::ResourceManager`].
    pub fn from_resource_manager(manager: NativeResourceManager) -> Self {
        Self {
            backend: ManagerBackend::Native(Arc::new(Mutex::new(manager))),
            prefix: String::new(),
            max_asset_bytes: DEFAULT_MAX_ASSET_BYTES,
        }
    }

    /// Scopes all lookups under a subdirectory inside `resources/rawfile` (for example `"assets"`).
    pub fn with_prefix(mut self, prefix: impl AsRef<str>) -> Result<Self> {
        self.prefix = normalize_raw_path(prefix.as_ref(), true)?;
        Ok(self)
    }

    /// Overrides the maximum byte size permitted for a single loaded asset (default: 128 MiB).
    pub fn with_max_asset_bytes(mut self, max_asset_bytes: usize) -> Self {
        self.max_asset_bytes = max_asset_bytes;
        self
    }

    /// Reads up to `buffer.len()` bytes of `path` directly into `buffer` without
    /// intermediate heap allocations, returning `Ok(None)` when the rawfile does not exist.
    pub fn read_into(&self, path: &str, buffer: &mut [u8]) -> Result<Option<usize>> {
        let resolved = self.resolve_path(path, false)?;
        self.with_manager(|manager| {
            if manager.is_raw_dir(&resolved) {
                return Ok(None);
            }
            let Some(raw_file) = open_optional_raw_file(manager, &resolved)? else {
                return Ok(None);
            };
            let file_len = usize::try_from(raw_file.file_size())
                .ok()
                .context("Invalid negative rawfile size")?;
            let target_len = buffer.len().min(file_len);
            fill_slice_from_raw_file(&raw_file, &mut buffer[..target_len])?;
            Ok(Some(target_len))
        })
    }

    /// Streams the entire rawfile at `path` through a caller-supplied reusable `buffer`,
    /// invoking `on_chunk` for each filled slice without allocating intermediate buffers.
    ///
    /// Returns `Ok(None)` when the rawfile does not exist, or `Ok(Some(total_bytes))` on completion.
    pub fn stream_into(
        &self,
        path: &str,
        buffer: &mut [u8],
        mut on_chunk: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<Option<u64>> {
        ensure!(
            !buffer.is_empty(),
            "OHOS rawfile streaming buffer must not be empty"
        );
        let resolved = self.resolve_path(path, false)?;
        self.with_manager(|manager| {
            if manager.is_raw_dir(&resolved) {
                return Ok(None);
            }
            let Some(raw_file) = open_optional_raw_file(manager, &resolved)? else {
                return Ok(None);
            };
            let file_len = usize::try_from(raw_file.file_size())
                .ok()
                .context("Invalid negative rawfile size")?;
            stream_chunks_with_buffer(
                file_len,
                buffer,
                |chunk| {
                    raw_file
                        .read_into(chunk)
                        .map_err(|error| anyhow::anyhow!("Read OHOS rawfile: {error}"))
                },
                &mut on_chunk,
            )?;
            Ok(Some(file_len as u64))
        })
    }

    /// Reads an entire rawfile into a single exact-sized `Vec<u8>` using `RawFile::read_into`,
    /// returning `Ok(None)` when the resource does not exist.
    pub fn read_bytes(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let resolved = self.resolve_path(path, false)?;
        let limit = self.max_asset_bytes;
        self.with_manager(|manager| {
            if manager.is_raw_dir(&resolved) {
                return Ok(None);
            }
            let Some(raw_file) = open_optional_raw_file(manager, &resolved)? else {
                return Ok(None);
            };
            let file_len = usize::try_from(raw_file.file_size())
                .ok()
                .context("Invalid negative rawfile size")?;
            ensure!(
                file_len <= limit,
                "OHOS rawfile '{resolved}' ({file_len} bytes) exceeds limit of {limit} bytes"
            );
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(file_len)
                .with_context(|| format!("Reserve {file_len} bytes for rawfile '{resolved}'"))?;
            bytes.resize(file_len, 0);
            fill_slice_from_raw_file(&raw_file, &mut bytes)?;
            Ok(Some(bytes))
        })
    }

    /// Loads one or more bundled font files from `resources/rawfile` directly into `text_system`
    /// without extra buffer copies.
    pub fn load_fonts(&self, text_system: &dyn PlatformTextSystem, paths: &[&str]) -> Result<()> {
        let mut fonts = Vec::with_capacity(paths.len());
        for path in paths {
            let bytes = self
                .read_bytes(path)?
                .with_context(|| format!("Bundled OHOS rawfile font '{path}' does not exist"))?;
            fonts.push(Cow::Owned(bytes));
        }
        if !fonts.is_empty() {
            text_system.add_fonts(fonts)?;
        }
        Ok(())
    }

    fn resolve_path(&self, path: &str, allow_empty: bool) -> Result<String> {
        let normalized = normalize_raw_path(path, allow_empty)?;
        if self.prefix.is_empty() {
            Ok(normalized)
        } else if normalized.is_empty() {
            Ok(self.prefix.clone())
        } else {
            Ok(format!("{}/{normalized}", self.prefix))
        }
    }

    fn with_manager<T>(&self, f: impl FnOnce(&NativeResourceManager) -> Result<T>) -> Result<T> {
        match &self.backend {
            ManagerBackend::App(app) => {
                let _guard = APP_RESOURCE_IO_LOCK.lock();
                let manager = app.resource_manager().context(
                    "OHOS native ResourceManager is unavailable; ensure ResourcePlugin is installed",
                )?;
                f(manager.inner())
            }
            ManagerBackend::Native(manager) => {
                let guard = manager.lock();
                f(&guard)
            }
        }
    }
}

impl AssetSource for RawFileAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(self.read_bytes(path)?.map(Cow::Owned))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let resolved = self.resolve_path(path, true)?;
        let strip_prefix = if self.prefix.is_empty() {
            None
        } else {
            Some(format!("{}/", self.prefix))
        };
        self.with_manager(|manager| {
            let dir = manager
                .open_dir(&resolved, false)
                .map_err(|error| anyhow::anyhow!("Open OHOS rawfile dir '{resolved}': {error}"))?;
            let mut entries = dir
                .files
                .into_keys()
                .map(|entry| {
                    let relative = match &strip_prefix {
                        Some(prefix) => entry.strip_prefix(prefix).unwrap_or(&entry).to_owned(),
                        None => entry,
                    };
                    SharedString::from(relative)
                })
                .collect::<Vec<_>>();
            entries.sort();
            Ok(entries)
        })
    }
}

fn open_optional_raw_file(manager: &NativeResourceManager, path: &str) -> Result<Option<RawFile>> {
    match manager.open_raw_file(path) {
        Ok(file) => Ok(Some(file)),
        Err(RawFileError::FfiInnerError(message)) if message.contains("does not exist") => Ok(None),
        Err(error) => Err(anyhow::anyhow!("Open OHOS rawfile '{path}': {error}")),
    }
}

fn fill_slice_from_raw_file(raw_file: &RawFile, target: &mut [u8]) -> Result<()> {
    read_chunks_into(target, |chunk| {
        raw_file
            .read_into(chunk)
            .map_err(|error| anyhow::anyhow!("Read OHOS rawfile: {error}"))
    })
}

fn read_chunks_into(
    target: &mut [u8],
    mut read_chunk: impl FnMut(&mut [u8]) -> Result<usize>,
) -> Result<()> {
    let mut offset = 0;
    while offset < target.len() {
        let remaining = &mut target[offset..];
        let count = read_chunk(remaining)?;
        if count == 0 || count > remaining.len() {
            bail!(
                "Unexpected EOF or invalid byte count ({count}) while reading OHOS rawfile at offset {offset}/{}",
                target.len()
            );
        }
        offset += count;
    }
    Ok(())
}

fn stream_chunks_with_buffer(
    total_len: usize,
    buffer: &mut [u8],
    mut read_chunk: impl FnMut(&mut [u8]) -> Result<usize>,
    on_chunk: &mut impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let mut remaining_total = total_len;
    while remaining_total > 0 {
        let window = remaining_total.min(buffer.len());
        let count = read_chunk(&mut buffer[..window])?;
        if count == 0 || count > window {
            bail!(
                "Unexpected EOF or invalid byte count ({count}) while streaming OHOS rawfile ({} bytes remaining)",
                remaining_total
            );
        }
        on_chunk(&buffer[..count])?;
        remaining_total -= count;
    }
    Ok(())
}

fn normalize_raw_path(path: &str, allow_empty: bool) -> Result<String> {
    ensure!(!path.contains('\0'), "OHOS rawfile path contains NUL byte");
    let mut segments = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => bail!("Parent directory traversal ('..') is not allowed in OHOS rawfile path"),
            other => segments.push(other),
        }
    }
    if segments.is_empty() && !allow_empty {
        bail!("OHOS rawfile path must not be empty");
    }
    Ok(segments.join("/"))
}

#[cfg(test)]
mod tests {
    use super::{normalize_raw_path, read_chunks_into, stream_chunks_with_buffer};

    #[test]
    fn normalizes_and_validates_rawfile_paths() {
        assert_eq!(
            normalize_raw_path("icons/check.svg", false).unwrap(),
            "icons/check.svg"
        );
        assert_eq!(
            normalize_raw_path("/./fonts//HarmonyOS_Sans.ttf/", false).unwrap(),
            "fonts/HarmonyOS_Sans.ttf"
        );
        assert_eq!(normalize_raw_path("", true).unwrap(), "");
        assert!(normalize_raw_path("", false).is_err());
        assert!(normalize_raw_path("icons/../secret.txt", false).is_err());
        assert!(normalize_raw_path("icons\0bad.svg", false).is_err());
    }

    #[test]
    fn fills_preallocated_buffer_across_partial_reads_without_extra_allocs() {
        let source = b"gpui-ohos-zero-copy-rawfile";
        let mut cursor = 0usize;
        let mut target = vec![0u8; source.len()];
        read_chunks_into(&mut target, |out| {
            let step = out.len().min(5);
            out[..step].copy_from_slice(&source[cursor..cursor + step]);
            cursor += step;
            Ok(step)
        })
        .unwrap();
        assert_eq!(target, source);
    }

    #[test]
    fn rejects_premature_eof_or_over_read() {
        let mut target = [0u8; 8];
        assert!(read_chunks_into(&mut target, |_| Ok(0)).is_err());
        assert!(read_chunks_into(&mut target, |out| Ok(out.len() + 1)).is_err());
    }

    #[test]
    fn streams_large_payload_through_reusable_fixed_buffer() {
        let source = b"0123456789abcdef0123456789ABCDEF!";
        let mut cursor = 0usize;
        let mut scratch = [0u8; 7];
        let mut reassembled = Vec::new();
        stream_chunks_with_buffer(
            source.len(),
            &mut scratch,
            |out| {
                let step = out.len().min(4);
                out[..step].copy_from_slice(&source[cursor..cursor + step]);
                cursor += step;
                Ok(step)
            },
            &mut |chunk| {
                reassembled.extend_from_slice(chunk);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(reassembled, source);
    }
}

//! Application asset source: the default GPUI Kit component icons plus the
//! extra Lucide icons the toolbar uses.

use std::borrow::Cow;

use gpui_kit::assets::{icon_assets, Assets};
use gpui_kit::{AssetSource, Result, SharedString};

icon_assets!(ExtraIcons, [FilePlus, FolderSearch, Save, TextWrap]);

/// Serves [`ExtraIcons`] first and falls back to the component bundle.
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

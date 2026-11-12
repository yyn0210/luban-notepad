//! OpenHarmony platform support for GPUI.
//!
//! The platform backend lives outside the Zed workspace and is injected with
//! [`gpui::Application::with_platform`], matching the architecture used by
//! `gpui-mobile` for Android and iOS.

#[cfg(feature = "fast")]
extern crate gpui_fast as gpui;

mod ohos;

pub use gpui::*;
pub use ohos::{current_platform, try_current_platform, PlatformOptions};

pub use ohos::{
    FontCatalog, FontDesignCoordinate, FontFaceData, PlatformServices, SyntheticFontStyle,
};
#[cfg(feature = "grid")]
pub use ohos::{GridLayoutError, GridLayoutRequest, GridTextCell};
pub use ohos::{ImagePolicy, ImageRegistrationError, PremultipliedImage, RasterImage};
pub use ohos::{RawFileAssets, ResourceBridgePlugin};

#[cfg(feature = "native-input")]
pub use ohos::{NativePointerPosition, NativeScrollDelta};

#[cfg(feature = "audio")]
pub use ohos::{
    AudioBridgePlugin, NativeAudioError, NativeAudioPlayer, NativeAudioPlayerOptions,
    NativeAudioService,
};

//! OpenHarmony entry point for Luban Notepad.
//!
//! The desktop binary lives in `luban-app`; this crate only builds the
//! `cdylib` that the ArkTS `NativeAbility` loads from the HAP. On non-OHOS
//! targets it compiles to an empty library so the desktop workspace stays
//! buildable.

#[cfg(target_env = "ohos")]
pub mod file_io;
#[cfg(target_env = "ohos")]
mod ohos_entry;

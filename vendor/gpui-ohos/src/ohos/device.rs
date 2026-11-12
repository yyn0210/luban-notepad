//! Synchronous OpenHarmony device metadata, SDK version, and SysCap queries.
//!
//! Uses direct C NDK bindings (`ohos-device-binding` and `ohos-init-binding`) so
//! version checks, capability probes, and desktop/mobile form-factor adaptation
//! work synchronously on any thread without waiting for ArkTS initialization.

/// Runtime device form factor reported by `OH_GetDeviceType`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeviceFormFactor {
    /// HarmonyOS PC / 2-in-1 desktop (`"2in1"` / `"pc"`).
    Desktop,
    /// Phone (`"phone"` or `"default"`).
    Phone,
    /// Tablet (`"tablet"`).
    Tablet,
    /// Watch / wearable (`"wearable"` or `"liteWearable"`).
    Wearable,
    /// TV, car, or unclassified device profile.
    Other,
}

impl DeviceFormFactor {
    pub(crate) fn detect() -> Self {
        Self::from_device_type(raw_device_type(), openharmony_ability::is_desktop_device())
    }

    pub(crate) fn from_device_type(raw: Option<&str>, compile_time_desktop: bool) -> Self {
        let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
            return if compile_time_desktop {
                Self::Desktop
            } else {
                Self::Other
            };
        };
        if raw.eq_ignore_ascii_case("2in1") || raw.eq_ignore_ascii_case("pc") {
            Self::Desktop
        } else if raw.eq_ignore_ascii_case("phone") || raw.eq_ignore_ascii_case("default") {
            Self::Phone
        } else if raw.eq_ignore_ascii_case("tablet") {
            Self::Tablet
        } else if raw.eq_ignore_ascii_case("wearable") || raw.eq_ignore_ascii_case("liteWearable") {
            Self::Wearable
        } else {
            Self::Other
        }
    }

    pub(crate) const fn is_desktop(self) -> bool {
        matches!(self, Self::Desktop)
    }

    pub(crate) const fn should_auto_hide_scrollbars(self) -> bool {
        matches!(self, Self::Phone | Self::Tablet | Self::Wearable)
    }
}

/// Maps `window.WindowStatusType` values to whether the window operates in
/// free-window management mode (`FLOATING = 4` or `MAXIMIZE = 2`).
///
/// Returns `None` for `MINIMIZE = 3` (which changes visibility without altering
/// the underlying windowing mode) and unknown status codes.
pub(crate) const fn is_free_window_status(status: i32) -> Option<bool> {
    match status {
        2 | 4 => Some(true),
        1 | 5 => Some(false),
        _ => None,
    }
}

/// Returns the OpenHarmony base SDK API version (`OH_GetSdkApiVersion`).
pub(crate) fn sdk_api_version() -> u32 {
    ohos_device_binding::sdk_api_version()
}

/// Returns the distribution OS API version (`OH_GetDistributionOSApiVersion`),
/// encoded as `M * 10000 + S * 100 + F` (for example, `60000` for HarmonyOS 6.0.0).
#[allow(dead_code)]
pub(crate) fn distribution_os_api_version() -> u32 {
    ohos_device_binding::distribution_os_api_version()
}

/// Synchronously queries whether a `SystemCapability.*` identifier is supported
/// by the current device (`canIUse` NDK symbol, API 8+).
pub(crate) fn can_i_use(syscap: &str) -> bool {
    if syscap.is_empty() || syscap.contains('\0') {
        return false;
    }
    ohos_init_binding::canIUse(syscap)
}

fn raw_device_type() -> Option<&'static str> {
    ohos_device_binding::device_type()
}

#[cfg(test)]
mod tests {
    use super::{can_i_use, is_free_window_status, DeviceFormFactor};

    #[test]
    fn classifies_ohos_device_types_for_desktop_and_scrollbars() {
        let desktop = DeviceFormFactor::from_device_type(Some("2in1"), false);
        assert_eq!(desktop, DeviceFormFactor::Desktop);
        assert!(desktop.is_desktop());
        assert!(!desktop.should_auto_hide_scrollbars());

        for phone_alias in ["phone", "default", "Phone"] {
            let phone = DeviceFormFactor::from_device_type(Some(phone_alias), true);
            assert_eq!(phone, DeviceFormFactor::Phone);
            assert!(!phone.is_desktop());
            assert!(phone.should_auto_hide_scrollbars());
        }

        let tablet = DeviceFormFactor::from_device_type(Some("tablet"), false);
        assert_eq!(tablet, DeviceFormFactor::Tablet);
        assert!(!tablet.is_desktop());
        assert!(tablet.should_auto_hide_scrollbars());

        let wearable = DeviceFormFactor::from_device_type(Some("liteWearable"), false);
        assert_eq!(wearable, DeviceFormFactor::Wearable);
        assert!(wearable.should_auto_hide_scrollbars());

        assert_eq!(
            DeviceFormFactor::from_device_type(None, true),
            DeviceFormFactor::Desktop
        );
        assert_eq!(
            DeviceFormFactor::from_device_type(None, false),
            DeviceFormFactor::Other
        );
    }

    #[test]
    fn maps_window_status_to_free_window_mode() {
        assert_eq!(is_free_window_status(1), Some(false)); // FULL_SCREEN
        assert_eq!(is_free_window_status(2), Some(true)); // MAXIMIZE
        assert_eq!(is_free_window_status(3), None); // MINIMIZE preserves prior mode
        assert_eq!(is_free_window_status(4), Some(true)); // FLOATING
        assert_eq!(is_free_window_status(5), Some(false)); // SPLIT_SCREEN
        assert_eq!(is_free_window_status(0), None);
    }

    #[test]
    fn rejects_empty_or_nul_syscap_without_panicking() {
        assert!(!can_i_use(""));
        assert!(!can_i_use("SystemCapability.Window\0.SessionManager"));
    }
}

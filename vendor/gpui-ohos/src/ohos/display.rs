use std::{
    fmt::Debug,
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

use super::platform::system_state_plugin::{AvailableArea, DisplaySnapshot};
use openharmony_ability::{display::DisplayInfo as NativeDisplayInfo, OpenHarmonyApp};

use crate::{point, px, size, Bounds, DisplayId, Pixels, PlatformDisplay, Result};

#[derive(Clone)]
pub(crate) struct OhosDisplay {
    app: OpenHarmonyApp,
    snapshot: Arc<Mutex<Option<DisplaySnapshot>>>,
    available_area: Arc<Mutex<Option<Bounds<Pixels>>>>,
}

impl OhosDisplay {
    pub(crate) fn new(app: OpenHarmonyApp) -> Self {
        Self {
            app,
            snapshot: Arc::new(Mutex::new(None)),
            available_area: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn from_snapshot(app: OpenHarmonyApp, snapshot: DisplaySnapshot) -> Self {
        let display = Self::new(app);
        display.update_snapshot(snapshot);
        display
    }

    pub(crate) fn id_raw(&self) -> i64 {
        self.snapshot
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, |snapshot| snapshot.id)
    }

    pub(crate) fn update_snapshot(&self, snapshot: DisplaySnapshot) {
        if snapshot.id < 0
            || snapshot.width <= 0
            || snapshot.height <= 0
            || !snapshot.density_pixels.is_finite()
            || snapshot.density_pixels <= 0.0
        {
            log::warn!("Ignoring invalid OHOS display snapshot: {snapshot:?}");
            return;
        }
        let area = snapshot.available_area.clone();
        *self.snapshot.lock().unwrap() = Some(snapshot);
        *self.available_area.lock().unwrap() = None;
        if let Some(area) = area {
            self.set_available_area(area.left, area.top, area.width, area.height);
        }
    }

    pub(crate) fn dimensions_and_scale(&self) -> (i32, i32, f32) {
        if let Some(snapshot) = self.snapshot.lock().unwrap().as_ref() {
            return (
                snapshot.width,
                snapshot.height,
                snapshot.density_pixels as f32,
            );
        }
        let (width, height) = self.app.display_size();
        (
            i32::try_from(width).unwrap_or(i32::MAX),
            i32::try_from(height).unwrap_or(i32::MAX),
            self.app.scale().max(f32::EPSILON),
        )
    }

    pub(crate) fn scale_factor(&self) -> f32 {
        self.dimensions_and_scale().2
    }

    pub(crate) fn set_available_area(&self, left: i32, top: i32, width: i32, height: i32) {
        let (display_width, display_height, scale) = self.dimensions_and_scale();
        if left < 0
            || top < 0
            || width <= 0
            || height <= 0
            || left as i64 + width as i64 > display_width as i64
            || top as i64 + height as i64 > display_height as i64
        {
            log::warn!("Ignoring invalid OHOS available area: {left},{top} {width}x{height}");
            return;
        }
        let bounds = Bounds::new(
            point(px(left as f32 / scale), px(top as f32 / scale)),
            size(px(width as f32 / scale), px(height as f32 / scale)),
        );
        *self.available_area.lock().unwrap() = Some(bounds);
        log::info!("OHOS available display area: {bounds:?}");
    }
}

impl Debug for OhosDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OhosDisplay")
            .field("id", &self.id_raw())
            .finish()
    }
}

impl PlatformDisplay for OhosDisplay {
    fn id(&self) -> DisplayId {
        DisplayId::new(self.id_raw() as u64)
    }

    fn uuid(&self) -> Result<Uuid> {
        Ok(Uuid::from_u128(self.id_raw() as u128 + 1))
    }

    fn bounds(&self) -> Bounds<Pixels> {
        let (width, height, scale) = self.dimensions_and_scale();
        if width > 0 && height > 0 {
            Bounds::new(
                point(px(0.0), px(0.0)),
                size(px(width as f32 / scale), px(height as f32 / scale)),
            )
        } else {
            let content_rect = self.app.content_rect();
            Bounds::new(
                point(px(0.0), px(0.0)),
                size(
                    px(content_rect.width.max(1) as f32 / scale),
                    px(content_rect.height.max(1) as f32 / scale),
                ),
            )
        }
    }

    fn refresh_interval(&self) -> Option<Duration> {
        let snapshot = match self.snapshot.lock() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                log::error!("Failed to read OHOS display refresh rate: {error}");
                return None;
            }
        };
        let hertz = match snapshot.as_ref() {
            Some(snapshot) => snapshot.refresh_rate?,
            None => f64::from(self.app.refresh_rate()),
        };
        crate::refresh_interval_from_hz(hertz)
    }

    fn visible_bounds(&self) -> Bounds<Pixels> {
        self.available_area
            .lock()
            .unwrap()
            .unwrap_or_else(|| self.bounds())
    }
}

pub(crate) fn seed_native_displays(app: &OpenHarmonyApp) -> (OhosDisplay, Vec<OhosDisplay>) {
    if super::device::sdk_api_version() < 14 {
        return (OhosDisplay::new(app.clone()), Vec::new());
    }
    let primary_info = openharmony_ability::display::primary_display().ok();
    let all_infos = openharmony_ability::display::all_displays().ok();
    let primary_id = primary_info.as_ref().map(|info| info.id).or_else(|| {
        all_infos
            .as_ref()
            .and_then(|list| list.first().map(|info| info.id))
    });

    let primary_snapshot = primary_info
        .as_ref()
        .and_then(|info| snapshot_from_native(info, true))
        .or_else(|| {
            let target_id = primary_id?;
            all_infos
                .as_ref()?
                .iter()
                .find(|info| info.id == target_id)
                .and_then(|info| snapshot_from_native(info, true))
        });

    let primary = match primary_snapshot {
        Some(snapshot) => OhosDisplay::from_snapshot(app.clone(), snapshot),
        None => OhosDisplay::new(app.clone()),
    };

    let others = all_infos
        .unwrap_or_default()
        .into_iter()
        .filter(|info| info.is_alive && Some(info.id) != primary_id)
        .filter_map(|info| snapshot_from_native(&info, false))
        .map(|snapshot| OhosDisplay::from_snapshot(app.clone(), snapshot))
        .collect();

    (primary, others)
}

fn snapshot_from_native(info: &NativeDisplayInfo, is_default: bool) -> Option<DisplaySnapshot> {
    if info.width <= 0
        || info.height <= 0
        || !info.density_pixels.is_finite()
        || info.density_pixels <= 0.0
    {
        return None;
    }
    Some(DisplaySnapshot {
        id: i64::from(info.id),
        width: info.width,
        height: info.height,
        density_pixels: f64::from(info.density_pixels),
        refresh_rate: (info.refresh_rate > 0).then(|| f64::from(info.refresh_rate)),
        available_area: query_native_available_area(info.id),
        is_default,
    })
}

fn query_native_available_area(display_id: u32) -> Option<AvailableArea> {
    #[cfg(feature = "api-26")]
    {
        if super::device::sdk_api_version() >= 20 {
            if let Ok(rect) =
                openharmony_ability::display::display_available_area(u64::from(display_id))
            {
                return Some(AvailableArea {
                    left: rect.left,
                    top: rect.top,
                    width: i32::try_from(rect.width).ok()?,
                    height: i32::try_from(rect.height).ok()?,
                });
            }
        }
    }
    let _ = display_id;
    None
}

#[cfg(test)]
mod tests {
    use super::snapshot_from_native;
    use openharmony_ability::display::{DisplayInfo, DisplayState, Orientation, Rotation};

    fn sample_display_info() -> DisplayInfo {
        DisplayInfo {
            id: 2,
            name: "External".into(),
            is_alive: true,
            width: 2560,
            height: 1600,
            physical_width: 2560,
            physical_height: 1600,
            refresh_rate: 120,
            available_width: 2560,
            available_height: 1520,
            density_dpi: 320.0,
            density_pixels: 2.0,
            scaled_density: 2.0,
            x_dpi: 254.0,
            y_dpi: 254.0,
            rotation: Rotation::Rotation0,
            state: DisplayState::On,
            orientation: Orientation::Landscape,
            hdr_formats: Vec::new(),
            color_spaces: Vec::new(),
        }
    }

    #[test]
    fn converts_valid_native_display_info_and_rejects_invalid_metrics() {
        let info = sample_display_info();
        let snapshot = snapshot_from_native(&info, false).expect("valid snapshot");
        assert_eq!(snapshot.id, 2);
        assert_eq!((snapshot.width, snapshot.height), (2560, 1600));
        assert_eq!(snapshot.density_pixels, 2.0);
        assert_eq!(snapshot.refresh_rate, Some(120.0));
        assert!(!snapshot.is_default);

        let mut zero_refresh = sample_display_info();
        zero_refresh.refresh_rate = 0;
        assert_eq!(
            snapshot_from_native(&zero_refresh, true)
                .expect("zero refresh rate should map to None")
                .refresh_rate,
            None
        );

        let mut bad_density = sample_display_info();
        bad_density.density_pixels = 0.0;
        assert!(snapshot_from_native(&bad_density, true).is_none());
    }
}

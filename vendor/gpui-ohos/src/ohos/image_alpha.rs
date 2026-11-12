//! Explicit alpha encoding for externally rasterized images. The image and its
//! registration share a lifetime; ordinary GPUI images retain straight alpha.
use gpui::{AtlasTextureId, ImageId, PolychromeSprite, RenderImage};
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use std::{
    fmt,
    sync::{Arc, OnceLock, Weak},
};

type Registrations = FxHashMap<ImageId, Weak<Registration>>;

fn registrations() -> &'static Mutex<Registrations> {
    static IMAGES: OnceLock<Mutex<Registrations>> = OnceLock::new();
    IMAGES.get_or_init(Default::default)
}

/// Keeps an image marked as premultiplied while it is cached or being painted.
/// Register before the first paint uploads the image into an atlas.
#[derive(Clone)]
pub struct PremultipliedImage(Arc<Registration>);

struct Registration {
    image: Arc<RenderImage>,
    policy: ImagePolicy,
}

/// The explicit alpha encoding and sampling policy of a registered image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImagePolicy {
    /// Premultiplied pixels with ordinary linear UI sampling.
    PremultipliedLinear,
    /// Straight-alpha pixels with nearest-neighbor raster sampling.
    StraightNearest,
    /// Premultiplied pixels with nearest-neighbor raster sampling.
    PremultipliedNearest,
}

impl ImagePolicy {
    fn flags(self) -> u32 {
        match self {
            Self::PremultipliedLinear => 1,
            Self::StraightNearest => 2,
            Self::PremultipliedNearest => 3,
        }
    }
}

/// An image already has a live owner with a different alpha or sampling policy.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ImageRegistrationError {
    /// The image whose existing registration was preserved.
    pub image_id: ImageId,
    /// The policy retained by the existing owner.
    pub existing: ImagePolicy,
    /// The incompatible policy requested by the caller.
    pub requested: ImagePolicy,
}

impl fmt::Display for ImageRegistrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "image {:?} is already registered as {:?}; cannot register as {:?}",
            self.image_id, self.existing, self.requested
        )
    }
}

impl std::error::Error for ImageRegistrationError {}

/// Native font rasters use the original terminal's nearest atlas sampling.
/// Register before the first upload; ordinary UI images keep linear filtering.
#[derive(Clone)]
pub struct RasterImage(Arc<Registration>);

impl Registration {
    fn new(
        image: Arc<RenderImage>,
        policy: ImagePolicy,
    ) -> Result<Arc<Self>, ImageRegistrationError> {
        let mut images = registrations().lock();
        if let Some(existing) = images.get(&image.id).and_then(Weak::upgrade) {
            drop(images);
            if existing.policy != policy {
                return Err(ImageRegistrationError {
                    image_id: image.id,
                    existing: existing.policy,
                    requested: policy,
                });
            }
            return Ok(existing);
        }
        let owner = Arc::new(Registration { image, policy });
        images.insert(owner.image.id, Arc::downgrade(&owner));
        Ok(owner)
    }
}

impl RasterImage {
    /// Registers straight-alpha pixels for nearest-neighbor sampling.
    ///
    /// # Errors
    /// Returns an error if this image has a live owner with a different policy.
    pub fn straight(image: Arc<RenderImage>) -> Result<Self, ImageRegistrationError> {
        Registration::new(image, ImagePolicy::StraightNearest).map(Self)
    }

    /// Registers premultiplied pixels for nearest-neighbor sampling.
    ///
    /// # Errors
    /// Returns an error if this image has a live owner with a different policy.
    pub fn premultiplied(image: Arc<RenderImage>) -> Result<Self, ImageRegistrationError> {
        Registration::new(image, ImagePolicy::PremultipliedNearest).map(Self)
    }

    pub fn image(&self) -> &Arc<RenderImage> {
        &self.0.image
    }
}

impl PremultipliedImage {
    /// Registers premultiplied pixels without changing linear UI sampling.
    ///
    /// # Errors
    /// Returns an error if this image has a live owner with a different policy.
    pub fn new(image: Arc<RenderImage>) -> Result<Self, ImageRegistrationError> {
        Registration::new(image, ImagePolicy::PremultipliedLinear).map(Self)
    }

    pub fn image(&self) -> &Arc<RenderImage> {
        &self.0.image
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut images = registrations().lock();
        // A concurrent constructor may have replaced an expired weak entry.
        if images
            .get(&self.image.id)
            .is_some_and(|entry| std::ptr::eq(entry.as_ptr(), self))
        {
            images.remove(&self.image.id);
        }
    }
}

pub(super) fn flags(id: ImageId) -> u32 {
    // Dropping the last upgraded owner calls Registration::drop, which locks
    // this registry too. Release the guard before inspecting/releasing it.
    let owner = {
        let images = registrations().lock();
        images.get(&id).and_then(Weak::upgrade)
    };
    owner.map_or(0, |owner| owner.policy.flags())
}

pub(super) fn encode_sprite_flags<'a>(
    image_flags: &FxHashMap<(AtlasTextureId, u32), u32>,
    sprites: &'a [PolychromeSprite],
    scratch: &'a mut Vec<PolychromeSprite>,
) -> &'a [PolychromeSprite] {
    if image_flags.is_empty() || sprites.is_empty() {
        return sprites;
    }
    scratch.clear();
    scratch.extend_from_slice(sprites);
    for sprite in scratch.iter_mut() {
        sprite.pad = image_flags
            .get(&(sprite.tile.texture_id, sprite.tile.tile_id.0))
            .copied()
            .unwrap_or(0);
    }
    scratch
}

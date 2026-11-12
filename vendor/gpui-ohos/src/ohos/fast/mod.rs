//! GPU support for the unmodified gpui-fast scene protocol.
//!
//! Scroll tiles are cached per renderer; rounded groups are flattened into
//! ordered draw steps with analytic clip stacks, including their paths.

mod bind_groups;
pub(super) mod frame;
mod layers;
mod pass_state;
pub(super) mod rounded_clip;

// Copyright 2022-2025 Zed Industries, Inc.; Copyright 2026 Longbridge.
// SPDX-License-Identifier: Apache-2.0
// Adapted from gpui-fast-wgpu 0.1.13 for the OHOS vertex-buffer renderer.
//! Scroll layer tiles on wgpu: the textures a layer's content is rasterized
//! into, and the passes that draw them where the scene composites them.

pub(crate) mod composite;
pub(crate) mod raster;
pub(crate) mod tile_cache;

pub(crate) use tile_cache::TileCache;

#[cfg(test)]
mod tests;

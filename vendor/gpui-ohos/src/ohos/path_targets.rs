/// Window-sized path attachments, owned only after the first path frame.
/// Views and bind groups retain their textures until in-flight GPU work completes.
pub(super) struct PathTargets {
    intermediate_view: wgpu::TextureView,
    msaa_view: Option<wgpu::TextureView>,
    #[cfg(not(feature = "fast"))]
    pub(super) bind_group: wgpu::BindGroup,
}

impl PathTargets {
    pub(super) fn new(
        device: &wgpu::Device,
        config: &wgpu::SurfaceConfiguration,
        sample_count: u32,
        _layout: &wgpu::BindGroupLayout,
        _sampler: &wgpu::Sampler,
    ) -> Self {
        let (_, intermediate_view) =
            Self::create_path_intermediate(device, config.format, config.width, config.height);
        let msaa_view = Self::create_msaa_if_needed(
            device,
            config.format,
            config.width,
            config.height,
            sample_count,
        )
        .map(|(_, view)| view);
        #[cfg(not(feature = "fast"))]
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("path_texture_bind_group"),
            layout: _layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&intermediate_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(_sampler),
                },
            ],
        });
        Self {
            intermediate_view,
            msaa_view,
            #[cfg(not(feature = "fast"))]
            bind_group,
        }
    }

    pub(super) fn attachments(&self) -> (&wgpu::TextureView, Option<&wgpu::TextureView>) {
        match &self.msaa_view {
            Some(msaa) => (msaa, Some(&self.intermediate_view)),
            None => (&self.intermediate_view, None),
        }
    }

    pub(super) fn create_path_intermediate(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> (wgpu::Texture, wgpu::TextureView) {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("path_intermediate"),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        (texture, view)
    }

    pub(super) fn create_msaa_if_needed(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        sample_count: u32,
    ) -> Option<(wgpu::Texture, wgpu::TextureView)> {
        if sample_count <= 1 {
            return None;
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("path_msaa"),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Some((texture, view))
    }
}

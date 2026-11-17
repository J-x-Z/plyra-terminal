// SPDX-License-Identifier: GPL-3.0-only
use wgpu::util::DeviceExt;

/// Clips the complete terminal and controls after they have been composed.
pub(crate) struct RoundedWindow {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    globals: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    size: [u32; 2],
    format: wgpu::TextureFormat,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a native graphics adapter; one bounded offscreen composition"]
    fn window_mask_clips_the_complete_composed_texture() {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .expect("native graphics adapter");
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))
                .unwrap();
        device.push_error_scope(wgpu::ErrorFilter::Validation);
        let mut mask = RoundedWindow::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb, [32, 32]);
        mask.resize(&device, [40, 32]);
        mask.resize(&device, [32, 32]);
        let output = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("window mask test output"),
            size: wgpu::Extent3d {
                width: 32,
                height: 32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = output.create_view(&wgpu::TextureViewDescriptor::default());
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("window mask test readback"),
            size: 32 * 256,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("stand-in for complete terminal and controls"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: mask.view(),
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.2,
                            g: 0.6,
                            b: 0.3,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
        }
        mask.draw(&queue, &mut encoder, &view, 12.0, None);
        encoder.copy_texture_to_buffer(
            output.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(32),
                },
            },
            output.size(),
        );
        queue.submit(Some(encoder.finish()));
        let _ = device.poll(wgpu::Maintain::Wait);
        assert!(pollster::block_on(device.pop_error_scope()).is_none());
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                sender.send(result).unwrap();
            });
        let _ = device.poll(wgpu::Maintain::Wait);
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .unwrap();
        let pixels = readback.slice(..).get_mapped_range();
        assert_eq!(&pixels[..4], &[0, 0, 0, 0]);
        let center = &pixels[16 * 256 + 16 * 4..16 * 256 + 16 * 4 + 4];
        assert_eq!(center[3], 255);
        assert!(center[1] > center[2] && center[2] > center[0]);
        drop(pixels);
        readback.unmap();
    }
}

impl RoundedWindow {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat, size: [u32; 2]) -> Self {
        let globals = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("window corner geometry"),
            contents: bytemuck::cast_slice(&[size[0] as f32, size[1] as f32, 0.0, 0.0]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rounded window source"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rounded window shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("rounded_window.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rounded window composition"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rounded window composition"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vertex_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fragment_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });
        let (texture, view, bind_group) = Self::target(device, &layout, &globals, format, size);
        Self {
            pipeline,
            layout,
            globals,
            bind_group,
            _texture: texture,
            view,
            size,
            format,
        }
    }

    fn target(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        globals: &wgpu::Buffer,
        format: wgpu::TextureFormat,
        size: [u32; 2],
    ) -> (wgpu::Texture, wgpu::TextureView, wgpu::BindGroup) {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("cached window composition"),
            size: wgpu::Extent3d {
                width: size[0],
                height: size[1],
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
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rounded window source"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: globals.as_entire_binding(),
                },
            ],
        });
        (texture, view, bind_group)
    }

    pub fn resize(&mut self, device: &wgpu::Device, size: [u32; 2]) {
        if self.size == size {
            return;
        }
        let (texture, view, bind_group) =
            Self::target(device, &self.layout, &self.globals, self.format, size);
        self._texture = texture;
        self.view = view;
        self.bind_group = bind_group;
        self.size = size;
    }

    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    pub fn draw(
        &self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        output: &wgpu::TextureView,
        radius: f32,
        timestamp_writes: Option<wgpu::RenderPassTimestampWrites<'_>>,
    ) {
        queue.write_buffer(
            &self.globals,
            0,
            bytemuck::cast_slice(&[self.size[0] as f32, self.size[1] as f32, radius, 0.0]),
        );
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("complete window corner mask"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: output,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes,
            occlusion_query_set: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

// FVPROV1 10886 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMDM4YjkyMDdkNDhjMzE3MmQzZWQ4MWJiZjgxMzZjNDVjNjU4ZWY3ZjhlOTgyYTJmZjY4YzNjNjFmODZhNWI4YSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiOTg4ZWQ1MTc2OWMwNjBhZGJmOTFjY2Y3YTQwM2JmZmQzZDAxMmNjY2U1MDNlNDI4YjBlNDkwNzU2MjJjNmNmMjc4MTVhZThhMmMxYTZiM2Y3NmRkMDI3ZTI2MDdlMzg2ZTA5YjRlNjMyYzhkZDhmN2FiMWU4NWEzZTU4OGI4MGIifV19XQ== FVPROV1-END

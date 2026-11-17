// SPDX-License-Identifier: GPL-3.0-only
use crate::{GlassMaterial, MaterialParameters};
use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MaterialProfile {
    tint_lift: f32,
    tint_strength: f32,
    grain: f32,
    sheen: f32,
    rim: f32,
    native_material: bool,
}

pub(crate) fn material_profile(
    material: GlassMaterial,
    parameters: MaterialParameters,
    native_material: bool,
) -> MaterialProfile {
    // Native macOS provides the material's optical response underneath this layer.
    let [tint_lift, grain, sheen, rim] = if native_material {
        [0.0, 0.003, 0.004, 0.025]
    } else {
        match material {
            GlassMaterial::LiquidClear => [0.008, 0.002, 0.045, 0.12],
            GlassMaterial::LiquidRegular => [0.035, 0.009, 0.018, 0.065],
            GlassMaterial::Hud => [0.012, 0.003, 0.012, 0.04],
            GlassMaterial::Sidebar => [0.024, 0.006, 0.01, 0.055],
            GlassMaterial::UnderWindowBackground => [0.042, 0.005, 0.006, 0.025],
        }
    };
    MaterialProfile {
        tint_lift: tint_lift * parameters.tint_strength,
        tint_strength: parameters.tint_strength,
        grain: grain * parameters.grain_strength,
        sheen: sheen * parameters.sheen_strength,
        rim: rim * parameters.rim_strength,
        native_material,
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GlassGlobals {
    resolution: [f32; 2],
    interaction: [f32; 2],
    tint: [f32; 4],
    profile: [f32; 4],
    options: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct GlassInstance {
    pub rect: [f32; 4],
    pub state: [f32; 4],
}

pub(crate) struct GlassPipeline {
    pipeline: wgpu::RenderPipeline,
    globals: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

impl GlassPipeline {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let globals = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("glass viewport"),
            contents: bytemuck::bytes_of(&GlassGlobals {
                resolution: [1.0; 2],
                interaction: [0.0; 2],
                tint: [0.0, 0.0, 0.0, 1.0],
                profile: [0.0; 4],
                options: [1.0, 1.0, 0.0, 0.0],
            }),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("glass viewport"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("glass viewport"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("glass materials"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("glass material shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("glass.wgsl").into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("glass materials"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vertex_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<GlassInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4],
                }],
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
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });
        Self {
            pipeline,
            globals,
            bind_group,
        }
    }

    pub fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        size: [u32; 2],
        interaction: [f32; 2],
        tint: [f32; 4],
        profile: MaterialProfile,
        instances: &[GlassInstance],
    ) -> Option<wgpu::Buffer> {
        queue.write_buffer(
            &self.globals,
            0,
            bytemuck::bytes_of(&GlassGlobals {
                resolution: [size[0] as f32, size[1] as f32],
                interaction,
                tint,
                profile: [profile.tint_lift, profile.grain, profile.sheen, profile.rim],
                options: [
                    f32::from(profile.native_material),
                    profile.tint_strength,
                    0.0,
                    0.0,
                ],
            }),
        );
        if instances.is_empty() {
            return None;
        }
        Some(
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("glass pane instances"),
                contents: bytemuck::cast_slice(instances),
                usage: wgpu::BufferUsages::VERTEX,
            }),
        )
    }

    pub fn draw<'a>(
        &'a self,
        pass: &mut wgpu::RenderPass<'a>,
        buffer: &'a wgpu::Buffer,
        count: u32,
    ) {
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, buffer.slice(..));
        pass.draw(0..6, 0..count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native_device() -> (wgpu::AdapterInfo, wgpu::Device, wgpu::Queue) {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .expect("a native graphics adapter is available");
        let info = adapter.get_info();
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))
                .expect("create native graphics device");
        (info, device, queue)
    }

    fn render_material(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        glass: &GlassPipeline,
        tint: [f32; 4],
        mode: f32,
        profile: MaterialProfile,
    ) -> Vec<u8> {
        device.push_error_scope(wgpu::ErrorFilter::Validation);
        let instances = glass
            .prepare(
                device,
                queue,
                [64, 64],
                [-1.0, -1.0],
                tint,
                profile,
                &[GlassInstance {
                    rect: [0.0, 0.0, 64.0, 64.0],
                    state: [mode, 1.0, 1.0, 1.0],
                }],
            )
            .unwrap();
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glass shader regression output"),
            size: wgpu::Extent3d {
                width: 64,
                height: 64,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("glass shader regression"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            glass.draw(&mut pass, &instances, 1);
        }
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("glass shader regression readback"),
            size: 64 * 256,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(64),
                },
            },
            wgpu::Extent3d {
                width: 64,
                height: 64,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(Some(encoder.finish()));
        let _ = device.poll(wgpu::Maintain::Wait);
        assert!(
            pollster::block_on(device.pop_error_scope()).is_none(),
            "glass shader validation failed"
        );
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                sender.send(result).unwrap();
            });
        let _ = device.poll(wgpu::Maintain::Wait);
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .unwrap();
        let pixels = buffer.slice(..).get_mapped_range().to_vec();
        buffer.unmap();
        pixels
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn metal_glass_shader_preserves_neutral_tint_and_reference_transparency() {
        let (info, device, queue) = native_device();
        assert_eq!(info.backend, wgpu::Backend::Metal);
        let glass = GlassPipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        for (tint, mode, expected_alpha) in [
            ([0.0, 0.0, 0.0, 0.503074], 1.0, 128_u8),
            ([0.25, 0.25, 0.25, 0.25], 1.0, 64_u8),
            ([0.25, 0.25, 0.25, 0.25], 0.0, 255_u8),
            ([0.0, 0.0, 0.0, 0.503074], 2.0, 128_u8),
        ] {
            let pixels = render_material(
                &device,
                &queue,
                &glass,
                tint,
                mode,
                material_profile(
                    GlassMaterial::LiquidClear,
                    MaterialParameters::default(),
                    true,
                ),
            );
            let center = &pixels[32 * 256 + 32 * 4..32 * 256 + 32 * 4 + 4];
            assert!(
                center[3].abs_diff(expected_alpha) <= 1,
                "unexpected material alpha {center:?}"
            );
            assert_eq!(center[0], center[1], "material must be neutral {center:?}");
            assert_eq!(center[1], center[2], "material must be neutral {center:?}");
            if tint[0] > 0.0 {
                assert!(
                    center[0] > 20,
                    "configured tint must reach the shader {center:?}"
                );
            }
            assert_eq!(
                pixels[3], center[3],
                "the window material must cover the terminal without an inset frame"
            );
        }
    }

    #[test]
    #[ignore = "requires a native graphics adapter; bounded offscreen material readback"]
    fn liquid_materials_differ_without_changing_configured_alpha() {
        let (_, device, queue) = native_device();
        let glass = GlassPipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        let clear = material_profile(
            GlassMaterial::LiquidClear,
            MaterialParameters::default(),
            false,
        );
        let regular = material_profile(
            GlassMaterial::LiquidRegular,
            MaterialParameters::default(),
            false,
        );
        let tint = [0.25, 0.25, 0.25, 0.4];
        for mode in [1.0, 2.0] {
            let first = render_material(&device, &queue, &glass, tint, mode, clear);
            let second = render_material(&device, &queue, &glass, tint, mode, regular);
            for pixels in [&first, &second] {
                let center = &pixels[32 * 256 + 32 * 4..32 * 256 + 32 * 4 + 4];
                assert!(
                    center[3].abs_diff(102) <= 1,
                    "configured alpha must be retained"
                );
                assert_eq!(center[0], center[1]);
                assert_eq!(center[1], center[2]);
            }
            assert_ne!(
                first, second,
                "Clear and Regular must produce distinct material pixels"
            );
        }
        let clear_off = render_material(&device, &queue, &glass, tint, 0.0, clear);
        let regular_off = render_material(&device, &queue, &glass, tint, 0.0, regular);
        assert_eq!(clear_off, regular_off, "Off disables material shading");
        assert!(clear_off.chunks_exact(4).all(|pixel| pixel[3] == 255));
        let transparent = render_material(
            &device,
            &queue,
            &glass,
            [0.25, 0.25, 0.25, 0.0],
            2.0,
            regular,
        );
        assert!(transparent
            .chunks_exact(4)
            .all(|pixel| pixel == [0, 0, 0, 0]));
    }
}

#[cfg(test)]
mod profile_tests {
    use super::*;

    #[test]
    fn material_presets_are_distinct_and_strengths_are_independent() {
        let materials = [
            GlassMaterial::LiquidClear,
            GlassMaterial::LiquidRegular,
            GlassMaterial::Hud,
            GlassMaterial::Sidebar,
            GlassMaterial::UnderWindowBackground,
        ];
        let profiles = materials
            .map(|material| material_profile(material, MaterialParameters::default(), false));
        for (index, profile) in profiles.iter().enumerate() {
            assert!(profiles[index + 1..].iter().all(|other| profile != other));
        }
        let reduced = material_profile(
            GlassMaterial::LiquidRegular,
            MaterialParameters {
                grain_strength: 0.0,
                ..MaterialParameters::default()
            },
            false,
        );
        assert_eq!(reduced.grain, 0.0);
        assert_eq!(reduced.tint_lift, profiles[1].tint_lift);
        assert_eq!(reduced.sheen, profiles[1].sheen);
        assert_eq!(reduced.rim, profiles[1].rim);
        let native = material_profile(
            GlassMaterial::LiquidRegular,
            MaterialParameters::default(),
            true,
        );
        assert_eq!(native.tint_lift, 0.0);
        assert_eq!(native.grain, 0.003);
        assert_eq!(native.sheen, 0.004);
        assert_eq!(native.rim, 0.025);
    }
}

// FVPROV1 16210 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiOGEyYTM1MzZlYTgzZmEyYjIyMTJkOGQzZTdlYTVmOTU2YjQyN2M4OTE4NGVhNDM5YTUwZjQ3ZDJiZWEyNzNhZSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZjhmZDNjODcyMzZjYmM4MDVjNTE5YjQzNjQ3YmRjNmE4OGQyOGE2N2M3NjkyOTU4NjE0ODM4OTgwYzg4Mjg4MzkxZDNkNjc3NmJhZDJjOGRiYWNiZjI2ODdiY2I2NWQ1YTZhNDFjMzEzOGM4ODJlMmEwYjYzYWUyOGY2MmZkMDgifV19XQ== FVPROV1-END

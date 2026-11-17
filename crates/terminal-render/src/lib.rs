// SPDX-License-Identifier: GPL-3.0-only
//! Event-driven GPU projection of the terminal model. This crate owns no PTY.

mod glass;
mod native_glass;
#[cfg(not(target_os = "macos"))]
mod rounded_window;
mod shaped_runs;

#[cfg(target_os = "android")]
pub mod android_glass;
#[cfg(target_os = "linux")]
mod linux_glass;
#[cfg(target_os = "linux")]
pub use linux_glass::prefer_server_decorations;
#[cfg(windows)]
mod windows_glass;
#[cfg(windows)]
mod windows_surface;

use glass::{GlassInstance, GlassPipeline};
pub use native_glass::{
    accessibility_preferences, configure_native_glass, AccessibilityPreferences,
};
#[cfg(not(target_os = "macos"))]
use rounded_window::RoundedWindow;
use shaped_runs::ShapeCache;

use std::collections::BTreeSet;
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{anyhow, ensure, Context, Result};
use bytemuck::{Pod, Zeroable};
use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache,
    TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use terminal_core::{Run, Screen, TerminalPalette};
use wgpu::util::DeviceExt;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::window::Window;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GlassEffect {
    Off,
    #[default]
    Balanced,
    Full,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GlassMaterial {
    #[default]
    LiquidClear,
    LiquidRegular,
    Hud,
    Sidebar,
    UnderWindowBackground,
}

/// Adapter selection is a user preference, independent of terminal semantics.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AdapterPreference {
    Auto,
    #[default]
    LowPower,
    HighPerformance,
}

impl AdapterPreference {
    fn power_preference(self) -> wgpu::PowerPreference {
        match self {
            Self::Auto => wgpu::PowerPreference::None,
            Self::LowPower => wgpu::PowerPreference::LowPower,
            Self::HighPerformance => wgpu::PowerPreference::HighPerformance,
        }
    }
}

/// Material strengths are multipliers. Backdrop blur uses physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MaterialParameters {
    pub tint_strength: f32,
    pub grain_strength: f32,
    pub sheen_strength: f32,
    pub rim_strength: f32,
    pub backdrop_blur_radius_px: f32,
}

impl Default for MaterialParameters {
    fn default() -> Self {
        Self {
            tint_strength: 1.0,
            grain_strength: 1.0,
            sheen_strength: 1.0,
            rim_strength: 1.0,
            backdrop_blur_radius_px: if cfg!(windows) { 0.0 } else { 24.0 },
        }
    }
}

impl MaterialParameters {
    fn sanitized(self, previous: Self) -> Self {
        fn bounded(value: f32, previous: f32, maximum: f32) -> f32 {
            if value.is_finite() {
                value.clamp(0.0, maximum)
            } else {
                previous
            }
        }
        Self {
            tint_strength: bounded(self.tint_strength, previous.tint_strength, 2.0),
            grain_strength: bounded(self.grain_strength, previous.grain_strength, 2.0),
            sheen_strength: bounded(self.sheen_strength, previous.sheen_strength, 2.0),
            rim_strength: bounded(self.rim_strength, previous.rim_strength, 2.0),
            backdrop_blur_radius_px: bounded(
                self.backdrop_blur_radius_px,
                previous.backdrop_blur_radius_px,
                100.0,
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Appearance {
    pub foreground: [u8; 3],
    pub background: [u8; 3],
    pub cursor: [u8; 3],
    pub selection_background: [u8; 3],
    pub selection_foreground: [u8; 3],
    pub ansi: [[u8; 3]; 16],
    pub opacity: f32,
    pub window_corner_radius: f32,
    pub material: GlassMaterial,
    pub material_parameters: MaterialParameters,
}

impl Default for Appearance {
    fn default() -> Self {
        let palette = TerminalPalette::default();
        Self {
            foreground: palette.foreground,
            background: palette.background,
            cursor: palette.foreground,
            selection_background: [181, 213, 255],
            selection_foreground: [0, 0, 0],
            ansi: palette.ansi,
            opacity: 0.503074,
            window_corner_radius: 0.0,
            material: GlassMaterial::default(),
            material_parameters: MaterialParameters::default(),
        }
    }
}

pub struct PaneRender<'a> {
    pub id: u64,
    /// Physical-pixel terminal bounds, including the terminal's internal padding.
    pub rect: [f32; 4],
    pub screen: &'a Screen,
    pub selection: Option<Selection>,
    pub focused: bool,
    pub preedit: Option<&'a str>,
}

const FONT: &[u8] = include_bytes!("../../../assets/terminal-fonts/DejaVuSansMono.ttf");

/// Display-grid endpoints in (row, column) order. The end is exclusive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    pub start: (u16, u16),
    pub end: (u16, u16),
}

impl Selection {
    fn normalized(self, screen: &Screen) -> Self {
        let clamp = |point: (u16, u16)| {
            (
                point.0.min(screen.rows.saturating_sub(1)),
                point.1.min(screen.cols),
            )
        };
        let first = clamp(self.start);
        let last = clamp(self.end);
        Self {
            start: first.min(last),
            end: first.max(last),
        }
    }

    fn columns(self, row: u16, cols: u16) -> Option<[u16; 2]> {
        if row < self.start.0 || row > self.end.0 {
            return None;
        }
        let left = if row == self.start.0 { self.start.1 } else { 0 };
        let right = if row == self.end.0 { self.end.1 } else { cols };
        (right > left).then_some([left, right])
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    position: [f32; 2],
    color: [f32; 4],
}

struct PlacedText {
    buffer: Arc<Buffer>,
    left: f32,
    top: f32,
    scale: f32,
    bounds: TextBounds,
    color: Color,
    selection_color: Color,
    selection: Option<[f32; 2]>,
}

impl PlacedText {
    fn areas(&self) -> impl Iterator<Item = TextArea<'_>> {
        let [selected_left, selected_right] = self
            .selection
            .unwrap_or([self.bounds.right as f32, self.bounds.right as f32]);
        let left = (selected_left.floor() as i32).clamp(self.bounds.left, self.bounds.right);
        let right = (selected_right.ceil() as i32).clamp(left, self.bounds.right);
        [
            (self.bounds.left, left, self.color),
            (left, right, self.selection_color),
            (right, self.bounds.right, self.color),
        ]
        .into_iter()
        .filter(|(left, right, _)| right > left)
        .map(move |(left, right, color)| TextArea {
            buffer: &self.buffer,
            left: self.left,
            top: self.top,
            scale: self.scale,
            bounds: TextBounds {
                left,
                right,
                ..self.bounds
            },
            default_color: color,
            custom_glyphs: &[],
        })
    }
}

struct Geometry {
    glass: Option<wgpu::Buffer>,
    glass_count: u32,
    backgrounds: Option<wgpu::Buffer>,
    background_count: u32,
    overlays: Option<wgpu::Buffer>,
    overlay_count: u32,
}

const GPU_TIMING_MAX_FRAMES: u32 = 64;

#[derive(Clone, Debug)]
pub struct GpuPassTiming {
    pub renderer_frame: u64,
    pub requested_effect: GlassEffect,
    pub pass: &'static str,
    pub duration_ns: f64,
}

#[derive(Clone, Debug)]
pub struct GpuTimingReport {
    pub requested: bool,
    pub supported: bool,
    pub enabled: bool,
    pub status: &'static str,
    pub error: Option<String>,
    pub timestamp_period_ns: Option<f32>,
    pub max_frames: u32,
    pub recorded_frames: u32,
    pub invalid_samples: usize,
    pub samples: Vec<GpuPassTiming>,
}

struct GpuQueryRecord {
    frame: u64,
    effect: GlassEffect,
    pass: &'static str,
    index: u32,
}
struct GpuFrameQueries {
    frame: u64,
    effect: GlassEffect,
    terminal: u32,
    controls: Option<u32>,
    window_mask: Option<u32>,
}
struct GpuTimer {
    queries: wgpu::QuerySet,
    period_ns: f32,
    frames: u32,
    records: Vec<GpuQueryRecord>,
}
impl GpuTimer {
    fn new(device: &wgpu::Device, period_ns: f32) -> Self {
        Self {
            queries: device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("explicit native-probe GPU timestamps"),
                ty: wgpu::QueryType::Timestamp,
                count: GPU_TIMING_MAX_FRAMES * 6,
            }),
            period_ns,
            frames: 0,
            records: Vec::with_capacity(GPU_TIMING_MAX_FRAMES as usize * 3),
        }
    }
    fn next_frame(
        &self,
        frame: u64,
        effect: GlassEffect,
        controls: bool,
        window_mask: bool,
    ) -> Option<GpuFrameQueries> {
        (self.frames < GPU_TIMING_MAX_FRAMES).then(|| {
            let index = self.records.len() as u32 * 2;
            GpuFrameQueries {
                frame,
                effect,
                terminal: index,
                controls: controls.then_some(index + 2),
                window_mask: window_mask.then_some(index + 2 + u32::from(controls) * 2),
            }
        })
    }
    fn writes(&self, index: u32) -> wgpu::RenderPassTimestampWrites<'_> {
        wgpu::RenderPassTimestampWrites {
            query_set: &self.queries,
            beginning_of_pass_write_index: Some(index),
            end_of_pass_write_index: Some(index + 1),
        }
    }
    fn commit(&mut self, frame: GpuFrameQueries) {
        self.frames += 1;
        self.records.push(GpuQueryRecord {
            frame: frame.frame,
            effect: frame.effect,
            pass: "terminal",
            index: frame.terminal,
        });
        if let Some(index) = frame.controls {
            self.records.push(GpuQueryRecord {
                frame: frame.frame,
                effect: frame.effect,
                pass: "egui",
                index,
            });
        }
        if let Some(index) = frame.window_mask {
            self.records.push(GpuQueryRecord {
                frame: frame.frame,
                effect: frame.effect,
                pass: "window_mask",
                index,
            });
        }
    }
    fn collect(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<(Vec<GpuPassTiming>, usize)> {
        ensure!(
            self.period_ns.is_finite() && self.period_ns > 0.0,
            "GPU timestamp period is unavailable"
        );
        if self.records.is_empty() {
            return Ok((Vec::new(), 0));
        }
        let count = self.records.len() as u32 * 2;
        let bytes = u64::from(count) * u64::from(wgpu::QUERY_SIZE);
        let resolve = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("native-probe timestamp resolve"),
            size: bytes,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("native-probe timestamp readback"),
            size: bytes,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("explicit final native-probe timestamp collection"),
        });
        encoder.resolve_query_set(&self.queries, 0..count, &resolve, 0);
        encoder.copy_buffer_to_buffer(&resolve, 0, &readback, 0, bytes);
        queue.submit([encoder.finish()]);
        let (sender, receiver) = mpsc::sync_channel(1);
        readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = sender.send(result);
            });
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let _ = device.poll(wgpu::Maintain::Poll);
            match receiver.try_recv() {
                Ok(result) => {
                    result.context("GPU timestamp mapping failed")?;
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    anyhow::bail!("GPU timestamp callback disconnected")
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            if Instant::now() >= deadline {
                readback.unmap();
                anyhow::bail!("GPU timestamp readback exceeded two seconds");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let mapped = readback.slice(..).get_mapped_range();
        let timestamps: Vec<_> = mapped
            .chunks_exact(8)
            .map(|value| u64::from_ne_bytes(value.try_into().unwrap()))
            .collect();
        let mut samples = Vec::with_capacity(self.records.len());
        let mut invalid = 0;
        for record in &self.records {
            match gpu_duration_ns(
                timestamps[record.index as usize],
                timestamps[record.index as usize + 1],
                self.period_ns,
            ) {
                Some(duration_ns) => samples.push(GpuPassTiming {
                    renderer_frame: record.frame,
                    requested_effect: record.effect,
                    pass: record.pass,
                    duration_ns,
                }),
                None => invalid += 1,
            }
        }
        drop(mapped);
        readback.unmap();
        Ok((samples, invalid))
    }
}

fn gpu_duration_ns(begin: u64, end: u64, period_ns: f32) -> Option<f64> {
    (begin != 0 && end >= begin && period_ns.is_finite() && period_ns > 0.0)
        .then(|| (end - begin) as f64 * f64::from(period_ns))
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    #[cfg(windows)]
    windows_composition: Option<windows_surface::CompositionSurface>,
    #[cfg(windows)]
    windows_instance: wgpu::Instance,
    #[cfg(windows)]
    windows_adapter: wgpu::Adapter,
    #[cfg(windows)]
    windows_surface_status: String,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    adapter_info: wgpu::AdapterInfo,
    alpha_modes: Vec<wgpu::CompositeAlphaMode>,
    font_system: FontSystem,
    shaped_runs: ShapeCache,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    preedit_renderer: TextRenderer,
    rectangles: wgpu::RenderPipeline,
    glass: GlassPipeline,
    #[cfg(not(target_os = "macos"))]
    rounded_window: Option<RoundedWindow>,
    egui: egui_wgpu::Renderer,
    size: PhysicalSize<u32>,
    scale_factor: f32,
    cell_width: f32,
    cell_height: f32,
    font_pixels: f32,
    font_points: f32,
    font_family: String,
    padding: f32,
    frame_count: u64,
    last_surface_error: Option<wgpu::SurfaceError>,
    selection: Option<Selection>,
    appearance: Appearance,
    native_glass: bool,
    configured_effect: Option<GlassEffect>,
    interaction: [f32; 2],
    reduce_motion: bool,
    gpu_timer: Option<GpuTimer>,
    gpu_timing_requested: bool,
    gpu_timing_supported: bool,
    gpu_timing_error: Option<String>,
    gpu_timing_report: Option<GpuTimingReport>,
    // Retain the native window until after the surface is dropped.
    _window: Arc<Window>,
}

impl Renderer {
    /// Called once after the probe's static-observation phases. No collection runs from normal rendering.
    pub fn collect_gpu_timings(&mut self) -> GpuTimingReport {
        if let Some(report) = &self.gpu_timing_report {
            return report.clone();
        }
        let mut report = GpuTimingReport {
            requested: self.gpu_timing_requested,
            supported: self.gpu_timing_supported,
            enabled: self.gpu_timer.is_some(),
            status: "not_requested",
            error: self.gpu_timing_error.clone(),
            timestamp_period_ns: None,
            max_frames: GPU_TIMING_MAX_FRAMES,
            recorded_frames: 0,
            invalid_samples: 0,
            samples: Vec::new(),
        };
        if let Some(timer) = self.gpu_timer.take() {
            report.timestamp_period_ns = Some(timer.period_ns);
            report.recorded_frames = timer.frames;
            match timer.collect(&self.device, &self.queue) {
                Ok((samples, invalid)) => {
                    report.invalid_samples = invalid;
                    report.status = if samples.is_empty() {
                        "no_valid_samples"
                    } else {
                        "sampled"
                    };
                    report.samples = samples;
                }
                Err(error) => {
                    report.status = "collection_failed";
                    report.error = Some(format!("{error:#}"));
                }
            }
        } else if report.error.is_some() {
            report.status = "feature_request_failed";
        } else if report.requested {
            report.status = "unsupported";
        }
        self.gpu_timing_report = Some(report.clone());
        report
    }

    pub fn new(window: Arc<Window>) -> Result<Self> {
        pollster::block_on(Self::initialize(
            window,
            false,
            AdapterPreference::default(),
        ))
    }

    /// Explicit native-probe constructor. Ordinary windows request no timestamp feature.
    pub fn new_with_gpu_timing(window: Arc<Window>) -> Result<Self> {
        pollster::block_on(Self::initialize(window, true, AdapterPreference::default()))
    }

    pub fn new_with_adapter_preference(
        window: Arc<Window>,
        gpu_timing_requested: bool,
        preference: AdapterPreference,
    ) -> Result<Self> {
        pollster::block_on(Self::initialize(window, gpu_timing_requested, preference))
    }

    async fn initialize(
        window: Arc<Window>,
        gpu_timing_requested: bool,
        adapter_preference: AdapterPreference,
    ) -> Result<Self> {
        let trace_started =
            (std::env::var("GLASS_TRACE_STARTUP").as_deref() == Ok("1")).then(Instant::now);
        let trace = |stage: &str| {
            if let Some(started) = trace_started {
                eprintln!(
                    "[glass-startup] renderer elapsed_ms={} stage={stage}",
                    started.elapsed().as_millis()
                );
            }
        };
        trace("initialize.begin");
        let size = window.inner_size();
        trace("instance.begin");
        let mut instance_descriptor = wgpu::InstanceDescriptor::default();
        #[cfg(windows)]
        {
            // A composition swap chain is DX12. Loading unrelated GL/Vulkan
            // drivers here creates workers even when those backends are unused.
            // The plain-HWND retry keeps both modern Windows fallback backends.
            instance_descriptor.backends = if windows_surface::has_no_redirection_bitmap(&window)? {
                wgpu::Backends::DX12
            } else {
                wgpu::Backends::DX12 | wgpu::Backends::VULKAN
            };
            // Backend debug layers are an explicit diagnostic mode, including
            // in an unoptimized developer package. WGPU API validation remains.
            instance_descriptor.flags =
                if std::env::var("GLASS_GPU_VALIDATION").as_deref() == Ok("1") {
                    wgpu::InstanceFlags::debugging()
                } else {
                    wgpu::InstanceFlags::empty()
                };
            if std::env::var("GLASS_GPU_ALL_BACKENDS").as_deref() == Ok("1") {
                instance_descriptor.backends = wgpu::Backends::all();
            }
            if trace_started.is_some() {
                eprintln!(
                    "[glass-startup] windows backends={:?} backend_flags={:?}",
                    instance_descriptor.backends, instance_descriptor.flags
                );
            }
        }
        let instance = wgpu::Instance::new(&instance_descriptor);
        trace("instance.end");
        #[cfg(windows)]
        trace("direct_composition.begin");
        #[cfg(windows)]
        let (mut windows_composition, mut windows_surface_status) =
            if !windows_surface::has_no_redirection_bitmap(&window)? {
                trace("direct_composition.skipped.redirected_hwnd");
                (
                    None,
                    "opaque_hwnd_fallback: window created with a redirection bitmap".to_owned(),
                )
            } else {
                match if gpu_timing_requested
                    && std::env::var("GLASS_PROBE_FORCE_HWND").as_deref() == Ok("1")
                {
                    Err(anyhow::anyhow!("forced HWND fallback in native probe"))
                } else {
                    windows_surface::CompositionSurface::new(window.clone())
                } {
                    Ok(owner) => (Some(owner), "dx12_direct_composition".to_owned()),
                    Err(error) => {
                        trace("direct_composition.failed.restore_redirection.begin");
                        windows_surface::restore_redirection(&window).with_context(|| {
                            format!("DirectComposition failed ({error:#}); restoring HWND bitmap")
                        })?;
                        trace("direct_composition.failed.restore_redirection.end");
                        (None, format!("opaque_hwnd_fallback: {error:#}"))
                    }
                }
            };
        #[cfg(windows)]
        trace("direct_composition.end");
        trace("surface.begin");
        #[cfg(windows)]
        let mut surface = if let Some(owner) = &windows_composition {
            match owner.create_surface(&instance) {
                Ok(surface) => surface,
                Err(error) => {
                    trace("composition_surface.failed.restore_hwnd.begin");
                    windows_composition = None;
                    windows_surface::restore_redirection(&window).with_context(|| {
                        format!(
                            "DirectComposition surface failed ({error:#}); restoring HWND bitmap"
                        )
                    })?;
                    windows_surface_status = format!("opaque_hwnd_fallback: {error:#}");
                    let surface = instance.create_surface(window.clone())?;
                    trace("composition_surface.failed.restore_hwnd.end");
                    surface
                }
            }
        } else {
            instance.create_surface(window.clone())?
        };
        #[cfg(not(windows))]
        let surface = instance.create_surface(window.clone())?;
        trace("surface.end");
        trace("request_adapter.begin");
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(&surface),
                power_preference: adapter_preference.power_preference(),
                force_fallback_adapter: false,
            })
            .await;
        trace(if adapter.is_some() {
            "request_adapter.end.found"
        } else {
            "request_adapter.end.none"
        });
        #[cfg(windows)]
        let adapter = if adapter.is_none() && windows_composition.is_some() {
            trace("adapter_fallback.restore_hwnd.begin");
            // WGPU retains a COM reference to the old visual until this surface drops.
            windows_composition = None;
            windows_surface::restore_redirection(&window).context(
                "no compatible DX12 composition adapter; restoring the window redirection bitmap",
            )?;
            surface = instance.create_surface(window.clone())?;
            windows_surface_status =
                "opaque_hwnd_fallback: no compatible DX12 composition adapter".to_owned();
            trace("adapter_fallback.restore_hwnd.end");
            trace("adapter_fallback.request_adapter.begin");
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    compatible_surface: Some(&surface),
                    power_preference: adapter_preference.power_preference(),
                    force_fallback_adapter: false,
                })
                .await;
            trace("adapter_fallback.request_adapter.end");
            adapter
        } else {
            adapter
        };
        let mut adapter = adapter.context("No GPU adapter can present to this native window")?;
        let gpu_timing_supported = adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY);
        let mut descriptor = wgpu::DeviceDescriptor {
            label: Some("terminal graphics device"),
            required_limits: graphics_device_limits(adapter.limits()),
            // DX12's performance allocator reserves large blocks even for this
            // small UI. Prefer compact reusable blocks on Windows laptops.
            memory_hints: if cfg!(windows) {
                wgpu::MemoryHints::MemoryUsage
            } else {
                wgpu::MemoryHints::Performance
            },
            required_features: if gpu_timing_requested && gpu_timing_supported {
                wgpu::Features::TIMESTAMP_QUERY
            } else {
                wgpu::Features::empty()
            },
            ..wgpu::DeviceDescriptor::default()
        };
        let mut gpu_timing_error = None;
        trace("request_device.begin");
        let initialization = adapter.request_device(&descriptor, None).await;
        trace(if initialization.is_ok() {
            "request_device.end.ok"
        } else {
            "request_device.end.error"
        });
        let (device, queue) = match initialization {
            Ok(device) => device,
            Err(error) if gpu_timing_requested && gpu_timing_supported => {
                gpu_timing_error = Some(format!("timestamp feature request failed: {error}"));
                trace("timestamp_fallback.request_adapter.begin");
                adapter = instance
                    .request_adapter(&wgpu::RequestAdapterOptions {
                        compatible_surface: Some(&surface),
                        power_preference: adapter_preference.power_preference(),
                        force_fallback_adapter: false,
                    })
                    .await
                    .context("GPU fallback adapter initialization failed")?;
                trace("timestamp_fallback.request_adapter.end");
                descriptor.required_features = wgpu::Features::empty();
                descriptor.required_limits = graphics_device_limits(adapter.limits());
                trace("timestamp_fallback.request_device.begin");
                let device = adapter
                    .request_device(&descriptor, None)
                    .await
                    .context("GPU device initialization failed")?;
                trace("timestamp_fallback.request_device.end");
                device
            }
            Err(error) => return Err(error).context("GPU device initialization failed"),
        };
        let adapter_info = adapter.get_info();
        if trace_started.is_some() {
            eprintln!(
                "[glass-startup] adapter preference={adapter_preference:?} name={:?} backend={:?}",
                adapter_info.name, adapter_info.backend
            );
        }
        let gpu_timing_supported = adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY);
        let gpu_timer =
            (gpu_timing_requested && gpu_timing_supported && gpu_timing_error.is_none())
                .then(|| GpuTimer::new(&device, queue.get_timestamp_period()));
        let capabilities = surface.get_capabilities(&adapter);
        let format = [
            wgpu::TextureFormat::Bgra8UnormSrgb,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        ]
        .into_iter()
        .find(|format| capabilities.formats.contains(format))
        .context("The window surface has no supported 8-bit sRGB format")?;
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .context("The GPU cannot configure this window surface")?;
        config.format = format;
        config.present_mode = wgpu::PresentMode::Fifo;
        #[cfg(windows)]
        let opaque_fallback = windows_composition.is_none();
        #[cfg(not(windows))]
        let opaque_fallback = false;
        config.alpha_mode = surface_alpha_mode(
            &capabilities.alpha_modes,
            config.alpha_mode,
            opaque_fallback,
        );
        trace("fonts.begin");
        let mut font_system =
            FontSystem::new_with_fonts([glyphon::fontdb::Source::Binary(Arc::new(FONT.to_vec()))]);
        // fontdb 0.16's automatic system-font discovery excludes Android.
        #[cfg(target_os = "android")]
        {
            let (locale, mut database) = font_system.into_locale_and_db();
            database.load_fonts_dir("/system/fonts");
            font_system = FontSystem::new_with_locale_and_db(locale, database);
        }
        font_system
            .db_mut()
            .set_monospace_family("DejaVu Sans Mono");
        trace("fonts.end");
        trace("pipelines.begin");
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let preedit_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("terminal cell rectangles"),
            source: wgpu::ShaderSource::Wgsl(include_str!("rectangles.wgsl").into()),
        });
        let rectangles = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("terminal cell rectangles"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4],
                }],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });
        let glass = GlassPipeline::new(&device, format);
        let egui = egui_wgpu::Renderer::new(&device, format, None, 1, false);
        trace("pipelines.end");
        let mut renderer = Self {
            surface,
            #[cfg(windows)]
            windows_composition,
            #[cfg(windows)]
            windows_instance: instance,
            #[cfg(windows)]
            windows_adapter: adapter,
            #[cfg(windows)]
            windows_surface_status,
            device,
            queue,
            config,
            adapter_info,
            alpha_modes: capabilities.alpha_modes,
            font_system,
            shaped_runs: ShapeCache::default(),
            swash_cache: SwashCache::new(),
            viewport,
            atlas,
            text_renderer,
            preedit_renderer,
            rectangles,
            glass,
            #[cfg(not(target_os = "macos"))]
            rounded_window: None,
            egui,
            size,
            scale_factor: 1.0,
            cell_width: 10.0,
            cell_height: 24.0,
            font_pixels: 16.0,
            font_points: 16.0,
            font_family: "DejaVu Sans Mono".to_owned(),
            padding: 12.0,
            frame_count: 0,
            last_surface_error: None,
            selection: None,
            appearance: Appearance::default(),
            native_glass: false,
            configured_effect: None,
            interaction: [0.5, 0.5],
            reduce_motion: false,
            gpu_timer,
            gpu_timing_requested,
            gpu_timing_supported,
            gpu_timing_error,
            gpu_timing_report: None,
            _window: window.clone(),
        };
        trace("font_measure.begin");
        renderer.measure(window.scale_factor());
        trace("font_measure.end");
        if size.width > 0 && size.height > 0 {
            trace("initial_configure.begin");
            renderer.configure_surface()?;
            trace("initial_configure.end");
        }
        trace("initialize.end");
        Ok(renderer)
    }

    fn configure_surface(&mut self) -> Result<()> {
        #[cfg(windows)]
        {
            // Native HWND transparency changes can alter Vulkan's surface
            // capabilities. Recheck only when configuring, never on idle frames.
            self.alpha_modes = self
                .surface
                .get_capabilities(&self.windows_adapter)
                .alpha_modes;
            self.config.alpha_mode = compatible_surface_alpha_mode(
                &self.alpha_modes,
                self.config.alpha_mode,
                self.requires_opaque_surface(),
            );
            ensure!(
                self.alpha_modes.contains(&self.config.alpha_mode),
                "the window surface has no compatible alpha mode"
            );
        }
        self.surface.configure(&self.device, &self.config);
        #[cfg(windows)]
        if let Some(owner) = &self.windows_composition {
            if let Err(error) = owner.commit() {
                self.windows_surface_status = format!("opaque_hwnd_fallback: {error:#}");
                // The surface owns its COM visual reference while the owner detaches its tree.
                self.windows_composition = None;
                windows_surface::restore_redirection(&self._window).with_context(|| {
                    format!("DirectComposition commit failed ({error:#}); restoring HWND bitmap")
                })?;
                self.surface = self.windows_instance.create_surface(self._window.clone())?;
                let capabilities = self.surface.get_capabilities(&self.windows_adapter);
                ensure!(
                    capabilities.formats.contains(&self.config.format),
                    "the HWND fallback cannot retain the configured render format"
                );
                self.alpha_modes = capabilities.alpha_modes;
                self.config.alpha_mode =
                    surface_alpha_mode(&self.alpha_modes, self.config.alpha_mode, true);
                self.native_glass = false;
                self.configured_effect = None;
                self.rounded_window = None;
                self.surface.configure(&self.device, &self.config);
            }
        }
        Ok(())
    }

    fn measure(&mut self, scale: f64) {
        self.shaped_runs.clear();
        self.scale_factor = if scale.is_finite() && scale > 0.0 {
            scale.clamp(0.5, 4.0) as f32
        } else {
            1.0
        };
        self.font_pixels = self.font_points * self.scale_factor;
        self.cell_height = (self.font_points * 1.5).ceil() * self.scale_factor;
        self.padding = 12.0 * self.scale_factor;
        let mut sample = Buffer::new(
            &mut self.font_system,
            Metrics::new(self.font_pixels, self.cell_height),
        );
        sample.set_text(
            &mut self.font_system,
            "0",
            Attrs::new().family(Family::Monospace),
            Shaping::Advanced,
        );
        sample.shape_until_scroll(&mut self.font_system, false);
        self.cell_width = sample
            .layout_runs()
            .next()
            .map(|run| run.line_w)
            .filter(|width| width.is_finite() && *width > 0.0)
            .unwrap_or(self.font_pixels * 0.61);
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>, scale: f64) -> Result<()> {
        self.size = size;
        if (f64::from(self.scale_factor) - scale).abs() > 0.001 {
            self.measure(scale);
        }
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.configure_surface()
    }

    pub fn dimensions(&self) -> (u16, u16) {
        self.dimensions_in(self.window_rect())
    }

    pub fn dimensions_in(&self, rect: [f32; 4]) -> (u16, u16) {
        let columns = ((rect[2] - self.padding * 2.0) / self.cell_width)
            .floor()
            .clamp(10.0, 240.0) as u16;
        let rows = ((rect[3] - self.padding * 2.0) / self.cell_height)
            .floor()
            .clamp(4.0, 100.0) as u16;
        (columns, rows)
    }

    pub fn set_font_size(&mut self, points: f32) {
        if points.is_finite() {
            self.font_points = points.clamp(10.0, 28.0);
            self.measure(f64::from(self.scale_factor));
        }
    }

    pub fn font_size(&self) -> f32 {
        self.font_points
    }
    pub fn font_family(&self) -> &str {
        &self.font_family
    }
    pub fn cell_metrics(&self) -> [f32; 2] {
        [self.cell_width, self.cell_height]
    }
    pub fn padding(&self) -> f32 {
        self.padding
    }
    pub fn native_glass_enabled(&self) -> bool {
        self.native_glass
    }

    fn translucent_background(&self) -> bool {
        self.native_glass
            || (cfg!(target_os = "linux")
                && matches!(
                    self.configured_effect,
                    Some(GlassEffect::Balanced | GlassEffect::Full)
                ))
    }

    fn rounded_window_enabled(&self) -> bool {
        !cfg!(target_os = "macos")
            && !self.requires_opaque_surface()
            && self.appearance.window_corner_radius > 0.0
            && self
                .alpha_modes
                .iter()
                .copied()
                .any(alpha_compositing_supported)
    }

    fn requires_opaque_surface(&self) -> bool {
        #[cfg(windows)]
        {
            // The redirected HWND fallback disables native transparency.
            // Vulkan's advertised alpha modes can change when that happens,
            // so a cached premultiplied mode must never be reselected here.
            self.windows_composition.is_none()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    pub fn set_appearance(&mut self, mut appearance: Appearance) {
        appearance.opacity = if appearance.opacity.is_finite() {
            appearance.opacity.clamp(0.0, 1.0)
        } else {
            self.appearance.opacity
        };
        appearance.window_corner_radius = if appearance.window_corner_radius.is_finite() {
            appearance.window_corner_radius.clamp(0.0, 48.0)
        } else {
            self.appearance.window_corner_radius
        };
        appearance.material_parameters = appearance
            .material_parameters
            .sanitized(self.appearance.material_parameters);
        if appearance.material != self.appearance.material
            || appearance.window_corner_radius != self.appearance.window_corner_radius
            || appearance.material_parameters != self.appearance.material_parameters
        {
            self.configured_effect = None;
        }
        self.appearance = appearance;
    }

    pub fn appearance(&self) -> Appearance {
        self.appearance
    }

    pub fn set_interaction(&mut self, position: PhysicalPosition<f64>, reduce_motion: bool) {
        self.reduce_motion = reduce_motion;
        if position.x.is_finite() && position.y.is_finite() {
            self.interaction = [
                (position.x / f64::from(self.size.width.max(1))).clamp(0.0, 1.0) as f32,
                (position.y / f64::from(self.size.height.max(1))).clamp(0.0, 1.0) as f32,
            ];
        }
    }

    pub fn font_families(&self) -> Vec<String> {
        self.font_system
            .db()
            .faces()
            .filter(|face| face.monospaced && !face.post_script_name.contains("Emoji"))
            .flat_map(|face| face.families.iter().map(|(name, _)| name.clone()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Load one installed CJK face into the UI font atlas without redistributing it.
    pub fn ui_font_fallback(&self) -> Option<(String, Vec<u8>, u32)> {
        let database = self.font_system.db();
        for family in [
            "PingFang SC",
            "Heiti SC",
            "Noto Sans CJK SC",
            "Source Han Sans SC",
        ] {
            let face = database
                .faces()
                .filter(|face| face.families.iter().any(|(name, _)| name == family))
                .min_by_key(|face| face.weight.0.abs_diff(400));
            if let Some(face) = face {
                if let Some(font) = database.with_face_data(face.id, |data, index| {
                    (family.to_owned(), data.to_vec(), index)
                }) {
                    return Some(font);
                }
            }
        }
        None
    }

    pub fn set_font_family(&mut self, family: &str) -> Result<()> {
        ensure!(
            self.font_families().iter().any(|name| name == family),
            "Monospace font is unavailable"
        );
        self.font_family = family.to_owned();
        self.font_system.db_mut().set_monospace_family(family);
        self.measure(f64::from(self.scale_factor));
        Ok(())
    }

    pub fn set_selection(&mut self, selection: Option<Selection>) {
        self.selection = selection;
    }

    pub fn cell_at(&self, position: PhysicalPosition<f64>) -> (u16, u16) {
        self.cell_at_in(position, self.window_rect())
    }

    pub fn cell_at_in(&self, position: PhysicalPosition<f64>, rect: [f32; 4]) -> (u16, u16) {
        let (columns, rows) = self.dimensions_in(rect);
        let column = ((position.x - f64::from(rect[0] + self.padding)) / f64::from(self.cell_width))
            .floor()
            .clamp(0.0, f64::from(columns - 1)) as u16;
        let row = ((position.y - f64::from(rect[1] + self.padding)) / f64::from(self.cell_height))
            .floor()
            .clamp(0.0, f64::from(rows - 1)) as u16;
        (row, column)
    }

    pub fn cursor_area(&self, screen: &Screen) -> (PhysicalPosition<i32>, PhysicalSize<u32>) {
        self.cursor_area_in(screen, self.window_rect())
    }

    pub fn cursor_area_in(
        &self,
        screen: &Screen,
        rect: [f32; 4],
    ) -> (PhysicalPosition<i32>, PhysicalSize<u32>) {
        (
            PhysicalPosition::new(
                (rect[0] + self.padding + f32::from(screen.cursor.col) * self.cell_width).round()
                    as i32,
                (rect[1] + self.padding + f32::from(screen.cursor.row) * self.cell_height).round()
                    as i32,
            ),
            PhysicalSize::new(
                self.cell_width.ceil() as u32,
                self.cell_height.ceil() as u32,
            ),
        )
    }

    fn window_rect(&self) -> [f32; 4] {
        [0.0, 0.0, self.size.width as f32, self.size.height as f32]
    }

    pub fn adapter_info(&self) -> &wgpu::AdapterInfo {
        &self.adapter_info
    }

    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }

    pub fn last_surface_error(&self) -> Option<&wgpu::SurfaceError> {
        self.last_surface_error.as_ref()
    }

    pub fn diagnostics(&self) -> String {
        #[cfg(windows)]
        let presentation = self.windows_surface_status.as_str();
        #[cfg(not(windows))]
        let presentation = "native_surface";
        format!("frames={} physical_size={:?} surface={}x{} surface_error={:?} alpha={:?} alpha_capabilities={:?} native_glass={} compositor_translucency={} effect={:?} opacity={} material={:?} material_parameters={:?} window_mask={} background={:?} foreground={:?} scale={} cell={:?} presentation={} {} {}",
            self.frame_count, self.size, self.config.width, self.config.height, self.last_surface_error,
            self.config.alpha_mode,
            self.alpha_modes,
            self.native_glass, self.translucent_background(), self.configured_effect, self.appearance.opacity, self.appearance.material,
            self.appearance.material_parameters, self.rounded_window_enabled(),
            self.appearance.background, self.appearance.foreground, self.scale_factor, self.cell_metrics(),
            presentation,
            self.shaped_runs.diagnostics(),
            native_glass::diagnostics(&self._window).unwrap_or_else(|error| format!("native_error={error}")))
    }

    pub fn render(&mut self, screen: &Screen) -> Result<()> {
        let pane = PaneRender {
            id: 0,
            rect: self.window_rect(),
            screen,
            selection: self.selection,
            focused: true,
            preedit: None,
        };
        self.render_workspace(
            &[pane],
            &[],
            &egui::TexturesDelta::default(),
            self.scale_factor,
            GlassEffect::Off,
        )
    }

    pub fn render_workspace(
        &mut self,
        panes: &[PaneRender<'_>],
        primitives: &[egui::ClippedPrimitive],
        textures: &egui::TexturesDelta,
        pixels_per_point: f32,
        effect: GlassEffect,
    ) -> Result<()> {
        self.last_surface_error = None;
        self.upload_ui_textures(textures);
        if self.size.width == 0 || self.size.height == 0 {
            self.free_ui_textures(textures);
            return Ok(());
        }
        let preferences = accessibility_preferences();
        let alpha_mode = surface_alpha_mode(
            &self.alpha_modes,
            self.config.alpha_mode,
            self.requires_opaque_surface()
                || ((effect == GlassEffect::Off || preferences.reduce_transparency)
                    && !self.rounded_window_enabled()),
        );
        if self.config.alpha_mode != alpha_mode {
            self.config.alpha_mode = alpha_mode;
            self.configure_surface()?;
        }
        let frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.configure_surface()?;
                match self.surface.get_current_texture() {
                    Ok(frame) => frame,
                    Err(
                        error @ (wgpu::SurfaceError::Timeout
                        | wgpu::SurfaceError::Lost
                        | wgpu::SurfaceError::Outdated),
                    ) => {
                        self.last_surface_error = Some(error);
                        self.free_ui_textures(textures);
                        return Ok(());
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error @ wgpu::SurfaceError::Timeout) => {
                self.last_surface_error = Some(error);
                self.free_ui_textures(textures);
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("glass workspace frame"),
            });
        let callbacks = self.encode_workspace(
            &mut encoder,
            &view,
            panes,
            primitives,
            pixels_per_point,
            effect,
            true,
        )?;
        self.queue.submit(
            callbacks
                .into_iter()
                .chain(std::iter::once(encoder.finish())),
        );
        frame.present();
        self.frame_count += 1;
        self.atlas.trim();
        self.free_ui_textures(textures);
        Ok(())
    }

    pub fn screenshot(&mut self, screen: &Screen, path: impl AsRef<Path>) -> Result<()> {
        let pane = PaneRender {
            id: 0,
            rect: self.window_rect(),
            screen,
            selection: self.selection,
            focused: true,
            preedit: None,
        };
        self.screenshot_workspace(
            &[pane],
            &[],
            &egui::TexturesDelta::default(),
            self.scale_factor,
            GlassEffect::Off,
            path,
        )
    }

    /// Capture this renderer's composed GPU frame without reading the desktop.
    #[allow(clippy::too_many_arguments)]
    pub fn screenshot_workspace(
        &mut self,
        panes: &[PaneRender<'_>],
        primitives: &[egui::ClippedPrimitive],
        textures: &egui::TexturesDelta,
        pixels_per_point: f32,
        effect: GlassEffect,
        path: impl AsRef<Path>,
    ) -> Result<()> {
        ensure!(
            self.size.width > 0 && self.size.height > 0,
            "Cannot capture a zero-size terminal surface"
        );
        self.upload_ui_textures(textures);
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glass workspace screenshot"),
            size: wgpu::Extent3d {
                width: self.size.width,
                height: self.size.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let unpadded_row = self.size.width * 4;
        let alignment = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_row = unpadded_row.div_ceil(alignment) * alignment;
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("glass workspace screenshot readback"),
            size: u64::from(padded_row) * u64::from(self.size.height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("glass workspace screenshot"),
            });
        let callbacks = self.encode_workspace(
            &mut encoder,
            &view,
            panes,
            primitives,
            pixels_per_point,
            effect,
            false,
        )?;
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row),
                    rows_per_image: Some(self.size.height),
                },
            },
            wgpu::Extent3d {
                width: self.size.width,
                height: self.size.height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(
            callbacks
                .into_iter()
                .chain(std::iter::once(encoder.finish())),
        );
        self.free_ui_textures(textures);
        let slice = readback.slice(..);
        let (sender, receiver) = mpsc::sync_channel(1);
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        let _ = self.device.poll(wgpu::Maintain::Wait);
        receiver
            .recv_timeout(Duration::from_secs(10))
            .context("GPU screenshot mapping timed out")??;
        let mapped = slice.get_mapped_range();
        let mut pixels = Vec::with_capacity(unpadded_row as usize * self.size.height as usize);
        for row in mapped.chunks_exact(padded_row as usize) {
            pixels.extend_from_slice(&row[..unpadded_row as usize]);
        }
        drop(mapped);
        readback.unmap();
        if self.config.format == wgpu::TextureFormat::Bgra8UnormSrgb {
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }
        // GPU targets store premultiplied linear color. PNG uses straight alpha.
        for pixel in pixels.chunks_exact_mut(4) {
            let alpha = f32::from(pixel[3]) / 255.0;
            if alpha == 0.0 {
                pixel[..3].fill(0);
            } else if alpha < 1.0 {
                for channel in &mut pixel[..3] {
                    let encoded = f32::from(*channel) / 255.0;
                    let linear = if encoded <= 0.04045 {
                        encoded / 12.92
                    } else {
                        ((encoded + 0.055) / 1.055).powf(2.4)
                    };
                    let straight = (linear / alpha).min(1.0);
                    let encoded = if straight <= 0.0031308 {
                        straight * 12.92
                    } else {
                        1.055 * straight.powf(1.0 / 2.4) - 0.055
                    };
                    *channel = (encoded * 255.0).round() as u8;
                }
            }
        }
        let file = File::create(path.as_ref()).context("Cannot create screenshot PNG")?;
        let mut encoder =
            png::Encoder::new(BufWriter::new(file), self.size.width, self.size.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&pixels)?;
        writer.finish()?;
        self.atlas.trim();
        Ok(())
    }

    fn upload_ui_textures(&mut self, textures: &egui::TexturesDelta) {
        for (id, delta) in &textures.set {
            self.egui
                .update_texture(&self.device, &self.queue, *id, delta);
        }
    }

    fn free_ui_textures(&mut self, textures: &egui::TexturesDelta) {
        for id in &textures.free {
            self.egui.free_texture(id);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_workspace(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        panes: &[PaneRender<'_>],
        primitives: &[egui::ClippedPrimitive],
        pixels_per_point: f32,
        effect: GlassEffect,
        configure_native: bool,
    ) -> Result<Vec<wgpu::CommandBuffer>> {
        let preferences = accessibility_preferences();
        let supports_transparency = alpha_compositing_supported(self.config.alpha_mode);
        #[cfg(windows)]
        let supports_backdrop = self.windows_composition.is_some();
        #[cfg(not(windows))]
        let supports_backdrop = true;
        let native_effect =
            if preferences.reduce_transparency || !supports_transparency || !supports_backdrop {
                GlassEffect::Off
            } else {
                effect
            };
        if configure_native
            && (self.configured_effect != Some(native_effect)
                || (cfg!(target_os = "macos")
                    && native_effect != GlassEffect::Off
                    && !self.native_glass))
        {
            self.native_glass = native_glass::configure_with_geometry(
                &self._window,
                native_effect,
                self.appearance.material,
                self.appearance.window_corner_radius,
                self.appearance.material_parameters,
            )
            .unwrap_or(false);
            self.configured_effect = Some(native_effect);
        }
        native_glass::poll(&self._window);
        #[cfg(target_os = "android")]
        {
            self.native_glass = android_glass::active();
        }
        let interaction = if preferences.reduce_motion || self.reduce_motion {
            [-1.0, -1.0]
        } else {
            self.interaction
        };
        let geometry = self.prepare_workspace(panes, effect, interaction)?;
        let use_window_mask = self.rounded_window_enabled();
        #[cfg(not(target_os = "macos"))]
        let mut rounded_window = self.rounded_window.take();
        #[cfg(not(target_os = "macos"))]
        let content_view = if use_window_mask {
            let size = [self.size.width, self.size.height];
            let mask = rounded_window
                .get_or_insert_with(|| RoundedWindow::new(&self.device, self.config.format, size));
            mask.resize(&self.device, size);
            mask.view()
        } else {
            rounded_window = None;
            view
        };
        #[cfg(target_os = "macos")]
        let content_view = view;
        let descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.size.width, self.size.height],
            pixels_per_point: if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
                pixels_per_point
            } else {
                1.0
            },
        };
        let callbacks =
            self.egui
                .update_buffers(&self.device, &self.queue, encoder, primitives, &descriptor);
        let timing = if configure_native {
            self.gpu_timer.as_ref().and_then(|timer| {
                timer.next_frame(
                    self.frame_count + 1,
                    effect,
                    !primitives.is_empty(),
                    use_window_mask,
                )
            })
        } else {
            None
        };
        let terminal_writes = timing.as_ref().and_then(|frame| {
            self.gpu_timer
                .as_ref()
                .map(|timer| timer.writes(frame.terminal))
        });
        self.encode(encoder, content_view, &geometry, terminal_writes)?;
        if !primitives.is_empty() {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("glass workspace controls"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: content_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: timing
                    .as_ref()
                    .and_then(|frame| frame.controls)
                    .and_then(|index| self.gpu_timer.as_ref().map(|timer| timer.writes(index))),
                occlusion_query_set: None,
            });
            self.egui
                .render(&mut pass.forget_lifetime(), primitives, &descriptor);
        }
        #[cfg(not(target_os = "macos"))]
        {
            if let Some(mask) = &rounded_window {
                let timestamp_writes = timing
                    .as_ref()
                    .and_then(|frame| frame.window_mask)
                    .and_then(|index| self.gpu_timer.as_ref().map(|timer| timer.writes(index)));
                mask.draw(
                    &self.queue,
                    encoder,
                    view,
                    self.appearance.window_corner_radius * self.scale_factor,
                    timestamp_writes,
                );
            }
            self.rounded_window = rounded_window;
        }
        if let (Some(timer), Some(frame)) = (&mut self.gpu_timer, timing) {
            timer.commit(frame);
        }
        Ok(callbacks)
    }

    fn prepare_workspace(
        &mut self,
        panes: &[PaneRender<'_>],
        effect: GlassEffect,
        interaction: [f32; 2],
    ) -> Result<Geometry> {
        self.shaped_runs.begin_frame();
        let mut backgrounds = Vec::new();
        let mut overlays = Vec::new();
        let mut text = Vec::new();
        let mut preedit_text = Vec::new();
        let mut materials = Vec::new();
        let mut uncovered = vec![self.window_rect()];
        for pane in panes {
            let Some(clip) = self.clip_rect(pane.rect) else {
                continue;
            };
            subtract_background_region(&mut uncovered, clip);
            let screen = pane.screen;
            let selection = pane.selection.map(|selection| selection.normalized(screen));
            let origin = [pane.rect[0] + self.padding, pane.rect[1] + self.padding];
            materials.push(GlassInstance {
                rect: pane.rect,
                state: [
                    match effect {
                        GlassEffect::Off => 0.0,
                        GlassEffect::Balanced => 1.0,
                        GlassEffect::Full => 2.0,
                    },
                    f32::from(pane.focused),
                    self.scale_factor,
                    f32::from(self.translucent_background()),
                ],
            });
            for (row, line) in screen.lines.iter().take(screen.rows as usize).enumerate() {
                let mut column = 0_u16;
                let top = origin[1] + row as f32 * self.cell_height;
                if top >= clip[1] + clip[3] {
                    break;
                }
                for run in &line.runs {
                    let width = run.width.min(screen.cols.saturating_sub(column));
                    if width == 0 {
                        break;
                    }
                    let left = origin[0] + f32::from(column) * self.cell_width;
                    if left >= clip[0] + clip[2] {
                        break;
                    }
                    let extent = f32::from(width) * self.cell_width;
                    let foreground = if run.fg_default {
                        self.appearance.foreground
                    } else {
                        run.fg_index
                            .and_then(|index| self.appearance.ansi.get(index as usize))
                            .copied()
                            .unwrap_or(run.fg)
                    };
                    let background = if run.bg_default {
                        self.appearance.background
                    } else {
                        run.bg_index
                            .and_then(|index| self.appearance.ansi.get(index as usize))
                            .copied()
                            .unwrap_or(run.bg)
                    };
                    let (foreground, background) = if run.inverse {
                        (background, foreground)
                    } else {
                        (foreground, background)
                    };
                    if !run.bg_default || run.inverse {
                        self.rectangle_clipped(
                            &mut backgrounds,
                            [left, top, extent, self.cell_height],
                            background,
                            clip,
                        );
                    }
                    if left + extent > clip[0]
                        && top + self.cell_height > clip[1]
                        && run.text.chars().any(|character| character != ' ')
                    {
                        let mut placed = self.shape_run(run, left, top, extent, foreground, clip);
                        placed.selection = selection
                            .and_then(|selection| selection.columns(row as u16, screen.cols))
                            .map(|[left, right]| {
                                [
                                    origin[0] + f32::from(left) * self.cell_width,
                                    origin[0] + f32::from(right) * self.cell_width,
                                ]
                            });
                        text.push(placed);
                    }
                    if run.underline {
                        self.rectangle_clipped(
                            &mut overlays,
                            [
                                left,
                                top + self.cell_height - 3.0 * self.scale_factor,
                                extent,
                                self.scale_factor,
                            ],
                            foreground,
                            clip,
                        );
                    }
                    if run.strike {
                        self.rectangle_clipped(
                            &mut overlays,
                            [
                                left,
                                top + self.cell_height * 0.55,
                                extent,
                                self.scale_factor,
                            ],
                            foreground,
                            clip,
                        );
                    }
                    column += width;
                }
            }
            if let Some(selection) = selection {
                for row in selection.start.0..=selection.end.0 {
                    if let Some([left, right]) = selection.columns(row, screen.cols) {
                        self.rectangle_clipped(
                            &mut backgrounds,
                            [
                                origin[0] + f32::from(left) * self.cell_width,
                                origin[1] + f32::from(row) * self.cell_height,
                                f32::from(right - left) * self.cell_width,
                                self.cell_height,
                            ],
                            self.appearance.selection_background,
                            clip,
                        );
                    }
                }
            }
            let composing = pane.focused && pane.preedit.is_some_and(|text| !text.is_empty());
            if pane.focused && screen.cursor.visible && !composing {
                self.rectangle_clipped(
                    &mut overlays,
                    [
                        origin[0] + f32::from(screen.cursor.col) * self.cell_width,
                        origin[1] + f32::from(screen.cursor.row) * self.cell_height,
                        2.0 * self.scale_factor,
                        self.cell_height,
                    ],
                    self.appearance.cursor,
                    clip,
                );
            }
            if composing {
                let composition: String = pane
                    .preedit
                    .unwrap_or_default()
                    .chars()
                    .filter(|character| !character.is_control())
                    .take(256)
                    .collect();
                let left = origin[0] + f32::from(screen.cursor.col) * self.cell_width;
                let top = origin[1] + f32::from(screen.cursor.row) * self.cell_height;
                let remaining = screen.cols.saturating_sub(screen.cursor.col).max(1);
                let extent = f32::from(remaining) * self.cell_width;
                let run = Run {
                    text: composition,
                    width: remaining,
                    ..Run::default()
                };
                let mut placed =
                    self.shape_run(&run, left, top, extent, self.appearance.foreground, clip);
                // Composition begins at the insertion point and clips at the row end.
                placed.left = left;
                placed.top = top;
                placed.scale = 1.0;
                let width = placed
                    .buffer
                    .layout_runs()
                    .map(|run| run.line_w)
                    .fold(0.0_f32, f32::max)
                    .max(self.cell_width)
                    .min(extent);
                self.rectangle_clipped(
                    &mut overlays,
                    [left, top, width, self.cell_height],
                    self.appearance.background,
                    clip,
                );
                self.rectangle_clipped(
                    &mut overlays,
                    [
                        left,
                        top + self.cell_height - 2.0 * self.scale_factor,
                        width,
                        self.scale_factor,
                    ],
                    self.appearance.cursor,
                    clip,
                );
                preedit_text.push(placed);
            }
        }
        // Dock margins, rounded panel corners and split gaps share the same
        // material substrate as terminal panes without compositing it twice.
        for rect in uncovered {
            materials.push(GlassInstance {
                rect,
                state: [
                    match effect {
                        GlassEffect::Off => 0.0,
                        GlassEffect::Balanced => 1.0,
                        GlassEffect::Full => 2.0,
                    },
                    0.0,
                    self.scale_factor,
                    f32::from(self.translucent_background()),
                ],
            });
        }
        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.size.width,
                height: self.size.height,
            },
        );
        self.text_renderer
            .prepare(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                text.iter().flat_map(PlacedText::areas),
                &mut self.swash_cache,
            )
            .map_err(|error| anyhow!("Cannot prepare terminal glyphs: {error}"))?;
        self.preedit_renderer
            .prepare(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                preedit_text.iter().map(|placed| TextArea {
                    buffer: &placed.buffer,
                    left: placed.left,
                    top: placed.top,
                    scale: placed.scale,
                    bounds: placed.bounds,
                    default_color: placed.color,
                    custom_glyphs: &[],
                }),
                &mut self.swash_cache,
            )
            .map_err(|error| anyhow!("Cannot prepare IME composition: {error}"))?;
        Ok(Geometry {
            glass: self.glass.prepare(
                &self.device,
                &self.queue,
                [self.size.width, self.size.height],
                interaction,
                [
                    f32::from(self.appearance.background[0]) / 255.0,
                    f32::from(self.appearance.background[1]) / 255.0,
                    f32::from(self.appearance.background[2]) / 255.0,
                    self.appearance.opacity,
                ],
                glass::material_profile(
                    self.appearance.material,
                    self.appearance.material_parameters,
                    cfg!(target_os = "macos"),
                ),
                &materials,
            ),
            glass_count: materials.len() as u32,
            backgrounds: self.vertex_buffer(&backgrounds),
            background_count: backgrounds.len() as u32,
            overlays: self.vertex_buffer(&overlays),
            overlay_count: overlays.len() as u32,
        })
    }

    fn clip_rect(&self, rect: [f32; 4]) -> Option<[f32; 4]> {
        if !rect.iter().all(|value| value.is_finite()) || rect[2] <= 0.0 || rect[3] <= 0.0 {
            return None;
        }
        let left = rect[0].max(0.0);
        let top = rect[1].max(0.0);
        let right = (rect[0] + rect[2]).min(self.size.width as f32);
        let bottom = (rect[1] + rect[3]).min(self.size.height as f32);
        (right > left && bottom > top).then_some([left, top, right - left, bottom - top])
    }

    fn shape_run(
        &mut self,
        run: &Run,
        left: f32,
        top: f32,
        extent: f32,
        foreground: [u8; 3],
        clip: [f32; 4],
    ) -> PlacedText {
        let (buffer, shaped_width) = self.shaped_runs.shape(
            &mut self.font_system,
            run,
            Metrics::new(self.font_pixels, self.cell_height),
        );
        let scale = if shaped_width > extent {
            extent / shaped_width
        } else {
            1.0
        };
        // libvterm owns columns. Font fallback cannot move the next run.
        let inset = if run.text.is_ascii() {
            0.0
        } else {
            (extent - shaped_width * scale).max(0.0) * 0.5
        };
        PlacedText {
            buffer,
            left: left + inset,
            top: top + self.cell_height * (1.0 - scale) * 0.5,
            scale,
            bounds: TextBounds {
                left: left.floor().max(clip[0]) as i32,
                top: top.floor().max(clip[1]) as i32,
                right: (left + extent).ceil().min(clip[0] + clip[2]) as i32,
                bottom: (top + self.cell_height).ceil().min(clip[1] + clip[3]) as i32,
            },
            color: Color::rgb(foreground[0], foreground[1], foreground[2]),
            selection_color: Color::rgb(
                self.appearance.selection_foreground[0],
                self.appearance.selection_foreground[1],
                self.appearance.selection_foreground[2],
            ),
            selection: None,
        }
    }

    fn rectangle_clipped(
        &self,
        output: &mut Vec<Vertex>,
        rect: [f32; 4],
        color: [u8; 3],
        clip: [f32; 4],
    ) {
        let left = rect[0].max(clip[0]);
        let top = rect[1].max(clip[1]);
        let right = (rect[0] + rect[2]).min(clip[0] + clip[2]);
        let bottom = (rect[1] + rect[3]).min(clip[1] + clip[3]);
        if right <= left || bottom <= top {
            return;
        }
        let x0 = left / self.size.width as f32 * 2.0 - 1.0;
        let x1 = right / self.size.width as f32 * 2.0 - 1.0;
        let y0 = 1.0 - top / self.size.height as f32 * 2.0;
        let y1 = 1.0 - bottom / self.size.height as f32 * 2.0;
        let color = linear_color(color);
        for position in [[x0, y0], [x0, y1], [x1, y1], [x0, y0], [x1, y1], [x1, y0]] {
            output.push(Vertex { position, color });
        }
    }

    fn vertex_buffer(&self, vertices: &[Vertex]) -> Option<wgpu::Buffer> {
        if vertices.is_empty() {
            return None;
        }
        Some(
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("terminal cell rectangles"),
                    contents: bytemuck::cast_slice(vertices),
                    usage: wgpu::BufferUsages::VERTEX,
                }),
        )
    }

    fn encode(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        geometry: &Geometry,
        timestamp_writes: Option<wgpu::RenderPassTimestampWrites<'_>>,
    ) -> Result<()> {
        let clear = if self.translucent_background() {
            [0.0; 4]
        } else {
            linear_color(self.appearance.background)
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("terminal text and cells"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: f64::from(clear[0]),
                        g: f64::from(clear[1]),
                        b: f64::from(clear[2]),
                        a: f64::from(clear[3]),
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes,
            occlusion_query_set: None,
        });
        if let Some(buffer) = &geometry.glass {
            self.glass.draw(&mut pass, buffer, geometry.glass_count);
        }
        if let Some(buffer) = &geometry.backgrounds {
            pass.set_pipeline(&self.rectangles);
            pass.set_vertex_buffer(0, buffer.slice(..));
            pass.draw(0..geometry.background_count, 0..1);
        }
        self.text_renderer
            .render(&self.atlas, &self.viewport, &mut pass)
            .map_err(|error| anyhow!("Cannot draw terminal glyphs: {error}"))?;
        if let Some(buffer) = &geometry.overlays {
            pass.set_pipeline(&self.rectangles);
            pass.set_vertex_buffer(0, buffer.slice(..));
            pass.draw(0..geometry.overlay_count, 0..1);
        }
        self.preedit_renderer
            .render(&self.atlas, &self.viewport, &mut pass)
            .map_err(|error| anyhow!("Cannot draw IME composition: {error}"))?;
        Ok(())
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        native_glass::release(&self._window);
    }
}

fn subtract_background_region(regions: &mut Vec<[f32; 4]>, cover: [f32; 4]) {
    for rect in std::mem::take(regions) {
        let right = rect[0] + rect[2];
        let bottom = rect[1] + rect[3];
        let left = rect[0].max(cover[0]);
        let top = rect[1].max(cover[1]);
        let cut_right = right.min(cover[0] + cover[2]);
        let cut_bottom = bottom.min(cover[1] + cover[3]);
        if left >= cut_right || top >= cut_bottom {
            regions.push(rect);
            continue;
        }
        for piece in [
            [rect[0], rect[1], left - rect[0], rect[3]],
            [cut_right, rect[1], right - cut_right, rect[3]],
            [left, rect[1], cut_right - left, top - rect[1]],
            [left, cut_bottom, cut_right - left, bottom - cut_bottom],
        ] {
            if piece[2] > 0.0 && piece[3] > 0.0 {
                regions.push(piece);
            }
        }
    }
}

fn linear_color(color: [u8; 3]) -> [f32; 4] {
    let mut result = [0.0, 0.0, 0.0, 1.0];
    for (output, channel) in result.iter_mut().zip(color) {
        let value = f32::from(channel) / 255.0;
        *output = if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        };
    }
    result
}

fn alpha_compositing_supported(mode: wgpu::CompositeAlphaMode) -> bool {
    matches!(
        mode,
        wgpu::CompositeAlphaMode::PreMultiplied | wgpu::CompositeAlphaMode::PostMultiplied
    ) || (cfg!(any(target_os = "linux", target_os = "android"))
        && mode == wgpu::CompositeAlphaMode::Inherit)
}

fn graphics_device_limits(adapter: wgpu::Limits) -> wgpu::Limits {
    wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter)
}

fn surface_alpha_mode(
    supported: &[wgpu::CompositeAlphaMode],
    fallback: wgpu::CompositeAlphaMode,
    opaque: bool,
) -> wgpu::CompositeAlphaMode {
    if opaque && supported.contains(&wgpu::CompositeAlphaMode::Opaque) {
        return wgpu::CompositeAlphaMode::Opaque;
    }
    // wgpu 24's Metal backend labels its non-opaque CAMetalLayer PostMultiplied.
    // Core Animation still consumes premultiplied pixels. Upstream corrected
    // this capability name in gfx-rs/wgpu#9922, so keep both names supported.
    [
        wgpu::CompositeAlphaMode::PreMultiplied,
        wgpu::CompositeAlphaMode::PostMultiplied,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        wgpu::CompositeAlphaMode::Inherit,
        wgpu::CompositeAlphaMode::Opaque,
    ]
    .into_iter()
    .find(|mode| supported.contains(mode))
    .unwrap_or(fallback)
}

#[cfg(any(windows, test))]
fn compatible_surface_alpha_mode(
    supported: &[wgpu::CompositeAlphaMode],
    current: wgpu::CompositeAlphaMode,
    opaque_required: bool,
) -> wgpu::CompositeAlphaMode {
    if !opaque_required && supported.contains(&current) {
        current
    } else {
        surface_alpha_mode(supported, current, opaque_required)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_material_covers_sidebar_and_split_gaps_without_double_alpha() {
        let mut regions = vec![[0.0, 0.0, 1280.0, 820.0]];
        super::subtract_background_region(&mut regions, [0.0, 0.0, 400.0, 820.0]);
        super::subtract_background_region(&mut regions, [408.0, 0.0, 452.0, 820.0]);
        assert_eq!(
            regions.iter().map(|rect| rect[2] * rect[3]).sum::<f32>(),
            428.0 * 820.0
        );
        for [x, y] in [
            [404.0, 20.0],
            [864.0, 4.0],
            [1276.0, 816.0],
            [1000.0, 400.0],
        ] {
            assert_eq!(
                regions
                    .iter()
                    .filter(|rect| x >= rect[0]
                        && x < rect[0] + rect[2]
                        && y >= rect[1]
                        && y < rect[1] + rect[3])
                    .count(),
                1
            );
        }
        assert!(regions
            .iter()
            .all(|rect| rect[0] >= 400.0 && rect[2] > 0.0 && rect[3] > 0.0));
        super::subtract_background_region(&mut regions, [0.0, 0.0, 1280.0, 820.0]);
        assert!(regions.is_empty());
    }

    #[test]
    fn material_parameters_bound_values_and_preserve_last_finite_values() {
        let parameters = super::MaterialParameters {
            tint_strength: f32::NAN,
            grain_strength: -1.0,
            sheen_strength: 3.0,
            rim_strength: f32::INFINITY,
            backdrop_blur_radius_px: 150.0,
        }
        .sanitized(super::MaterialParameters::default());
        assert_eq!(parameters.tint_strength, 1.0);
        assert_eq!(parameters.grain_strength, 0.0);
        assert_eq!(parameters.sheen_strength, 2.0);
        assert_eq!(parameters.rim_strength, 1.0);
        assert_eq!(parameters.backdrop_blur_radius_px, 100.0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_surface_inherits_native_window_alpha_when_required() {
        use wgpu::CompositeAlphaMode::{Inherit, Opaque};
        assert_eq!(
            surface_alpha_mode(&[Opaque, Inherit], Opaque, false),
            Inherit
        );
        assert_eq!(
            surface_alpha_mode(&[Opaque, Inherit], Inherit, true),
            Opaque
        );
        assert_eq!(surface_alpha_mode(&[Opaque], Opaque, false), Opaque);
    }

    #[test]
    fn gpu_timestamp_delta_converts_driver_ticks_and_rejects_invalid_values() {
        assert_eq!(super::gpu_duration_ns(100, 160, 2.0), Some(120.0));
        assert_eq!(super::gpu_duration_ns(0, 160, 1.0), None);
        assert_eq!(super::gpu_duration_ns(160, 100, 1.0), None);
        assert_eq!(super::gpu_duration_ns(100, 160, 0.0), None);
        assert_eq!(super::gpu_duration_ns(100, 160, f32::NAN), None);
    }
    use super::surface_alpha_mode;
    use wgpu::CompositeAlphaMode::{Opaque, PostMultiplied, PreMultiplied};

    #[test]
    fn redirected_hwnd_retains_opaque_when_transparency_capabilities_change() {
        let initially_supported = [Opaque, PreMultiplied];
        assert_eq!(
            super::compatible_surface_alpha_mode(&initially_supported, PreMultiplied, true),
            Opaque
        );
        assert_eq!(
            super::compatible_surface_alpha_mode(&[Opaque], PreMultiplied, true),
            Opaque
        );
        // A composition surface retains its explicitly selected mode.
        assert_eq!(
            super::compatible_surface_alpha_mode(&initially_supported, PreMultiplied, false),
            PreMultiplied
        );
        assert_eq!(
            super::compatible_surface_alpha_mode(&initially_supported, Opaque, false),
            Opaque
        );
    }

    #[test]
    fn metal_24_surface_keeps_transparent_compositing() {
        assert_eq!(
            surface_alpha_mode(&[Opaque, PostMultiplied], Opaque, false),
            PostMultiplied
        );
        assert_eq!(
            surface_alpha_mode(&[Opaque, PreMultiplied], Opaque, false),
            PreMultiplied
        );
        assert_eq!(surface_alpha_mode(&[Opaque], Opaque, false), Opaque);
    }

    #[test]
    fn opaque_effect_restores_transparency_when_glass_returns() {
        let supported = [Opaque, PostMultiplied];
        assert_eq!(surface_alpha_mode(&supported, PostMultiplied, true), Opaque);
        assert_eq!(
            surface_alpha_mode(&supported, Opaque, false),
            PostMultiplied
        );
        assert_eq!(surface_alpha_mode(&supported, PostMultiplied, true), Opaque);
        assert_eq!(
            surface_alpha_mode(&[PostMultiplied], PostMultiplied, true),
            PostMultiplied
        );
    }
}

// FVPROV1 86128 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiOWE3MTYzZDU0ZTU3ODlhNTBhZDZiNDllZDlkMTQ5ZTJiM2I3ZGIxNzI1MTE3NGE0ODM2YjQ3YzgzOTU5NjNmYiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNmJkMGQ0NjEyOTc0ZWJlMjQ4OWU5ODk0N2UzOGE4OWJlOWQ1ZTAyYjNlNWMzMDQwNzRkZmM1YjIyMjViNGYzZDhjZmVjZjk5NzU0MGZhOWVhZDhkZWJhZmFhYjJlN2E4NmI4ZGEwN2M5NGMwYmVmZTNkOWQ3ZmViODZlMWFhMDEifV19XQ== FVPROV1-END

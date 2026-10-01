//! Uploads decoded planes to the GPU and turns them into a picture.
//!
//! A decoded frame arrives as the two planes of NV12: luma, and an interleaved
//! chroma plane. Sending those to the GPU as they are costs a third of what RGBA
//! costs, and the colour matrix moves into a shader, where it used to be spent on
//! the CPU once per pixel per channel.
//!
//! egui paints its own textures with its own shader, which takes RGBA, so the
//! planes are not sampled directly by the grid. They are turned into a plain
//! texture by a small render pass, and that texture is handed to egui as a native
//! one. The grid then draws it exactly like any other texture - letterboxing,
//! dimming and the swipe keep working, because as far as egui is concerned it is
//! an ordinary texture.

use std::collections::HashMap;

use monitor_core::pipeline::VideoFrame;

/// Row alignment wgpu demands of a texture upload, in bytes.
const ROW_ALIGNMENT: usize = 256;

/// The format egui reads a picture in.
///
/// Deliberately not an sRGB one, and the shader hands over the encoded values
/// rather than linear light: egui samples a texture and takes what it reads for
/// a gamma value. An sRGB view would have the hardware decode it first, and the
/// picture would come out one gamma step too dark.
const OUTPUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// From the two NV12 planes to the texture egui draws.
///
/// The matrix runs on the encoded values, because that is the space video
/// defines it over, and its result is written out as it is.
const SHADER: &str = r#"
@group(0) @binding(0) var luma_plane: texture_2d<f32>;
@group(0) @binding(1) var chroma_plane: texture_2d<f32>;
@group(0) @binding(2) var plane_sampler: sampler;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vertex(@builtin(vertex_index) index: u32) -> VertexOut {
    // One oversized triangle rather than two, so no vertex buffer is needed.
    var corners = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let corner = corners[index];
    var out: VertexOut;
    out.position = vec4<f32>(corner, 0.0, 1.0);
    // Clip space grows upwards, a texture grows downwards.
    out.uv = vec2<f32>((corner.x + 1.0) * 0.5, (1.0 - corner.y) * 0.5);
    return out;
}

@fragment
fn fragment(in: VertexOut) -> @location(0) vec4<f32> {
    // BT.601 limited range, which is what both decoders hand over: whatever
    // range the camera announced was already folded into these planes.
    let luma = (textureSample(luma_plane, plane_sampler, in.uv).r - 16.0 / 255.0) * (255.0 / 219.0);
    let chroma = textureSample(chroma_plane, plane_sampler, in.uv).rg;
    let blue = (chroma.x - 0.5) * (255.0 / 224.0);
    let red = (chroma.y - 0.5) * (255.0 / 224.0);

    let colour = clamp(
        vec3<f32>(
            luma + 1.5748 * red,
            luma - 0.1873 * blue - 0.4681 * red,
            luma + 1.8556 * blue,
        ),
        vec3<f32>(0.0),
        vec3<f32>(1.0),
    );
    return vec4<f32>(colour, 1.0);
}
"#;

/// What the grid needs to draw one channel's newest picture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoSurface {
    pub id: egui::TextureId,
    pub size: egui::Vec2,
}

/// The GPU resources of one channel.
struct Channel {
    /// Luma and chroma planes, in the order the shader binds them.
    planes: [wgpu::Texture; 2],
    /// The picture the conversion pass writes and egui draws.
    output: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
    surface: VideoSurface,
    /// Picture size in pixels; `surface.size` is the same, rounded for egui.
    size: (u32, u32),
    /// Padded copy of one plane, reused between uploads so a picture does not
    /// allocate.
    scratch: Vec<u8>,
}

/// Uploads decoded pictures and keeps one set of textures per channel.
pub struct VideoRenderer {
    state: egui_wgpu::RenderState,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    channels: HashMap<usize, Channel>,
    /// Batches the conversion passes of one UI frame into a single submission.
    encoder: Option<wgpu::CommandEncoder>,
}

impl VideoRenderer {
    /// Builds the pipeline once, from the renderer eframe already runs.
    pub fn new(state: &egui_wgpu::RenderState) -> Self {
        let device = &state.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("xgview-nv12"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let plane = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("xgview-nv12-layout"),
            entries: &[
                plane(0),
                plane(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("xgview-nv12-pipeline-layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("xgview-nv12-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vertex"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fragment"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: OUTPUT_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("xgview-nv12-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        Self {
            state: state.clone(),
            pipeline,
            layout,
            sampler,
            channels: HashMap::new(),
            encoder: None,
        }
    }

    /// The picture of a channel, `None` until one has been uploaded.
    pub fn surface(&self, index: usize) -> Option<VideoSurface> {
        self.channels.get(&index).map(|channel| channel.surface)
    }

    /// Uploads one picture, rebuilding the textures when the size changed.
    pub fn upload(&mut self, index: usize, frame: &VideoFrame) {
        let size = (frame.width, frame.height);
        if size.0 == 0 || size.1 == 0 {
            return;
        }
        let (width, height) = (size.0 as usize, size.1 as usize);
        let chroma = chroma_size(size);
        let (chroma_width, chroma_height) = (chroma.0 as usize, chroma.1 as usize);
        if frame.y.len() < width * height || frame.uv.len() < chroma_width * 2 * chroma_height {
            return;
        }

        if self.channels.get(&index).is_none_or(|channel| channel.size != size) {
            self.rebuild(index, size);
        }
        let Some(channel) = self.channels.get_mut(&index) else {
            return;
        };

        // wgpu wants every uploaded row to start on a 256 byte boundary, so both
        // planes are copied into a padded buffer. The two share a row length: one
        // chroma pair covers two luma samples, so a chroma row of U and V bytes is
        // as long as a luma row.
        let stride = next_multiple_of(width, ROW_ALIGNMENT);
        copy_plane(&mut channel.scratch, &frame.y, stride, width, height);
        write_plane(&self.state.queue, &channel.planes[0], size, stride, &channel.scratch);
        copy_plane(&mut channel.scratch, &frame.uv, stride, width, chroma_height);
        write_plane(&self.state.queue, &channel.planes[1], chroma, stride, &channel.scratch);

        self.convert(index);
    }

    /// Submits the conversion passes recorded since the last call.
    pub fn flush(&mut self) {
        if let Some(encoder) = self.encoder.take() {
            self.state.queue.submit([encoder.finish()]);
        }
    }

    /// Drops the resources of a channel that is no longer shown.
    pub fn release(&mut self, index: usize) {
        if let Some(channel) = self.channels.remove(&index) {
            self.free(&channel);
        }
    }

    /// Releases the egui texture of a channel's picture.
    fn free(&self, channel: &Channel) {
        self.state.renderer.write().free_texture(&channel.surface.id);
    }

    /// Recreates the textures of a channel for a new picture size.
    fn rebuild(&mut self, index: usize, size: (u32, u32)) {
        let device = &self.state.device;
        let planes = [
            create_plane(device, size, wgpu::TextureFormat::R8Unorm, "xgview-luma"),
            create_plane(device, chroma_size(size), wgpu::TextureFormat::Rg8Unorm, "xgview-chroma"),
        ];
        let output = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("xgview-picture"),
            size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OUTPUT_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let output_view = output.create_view(&Default::default());
        let luma_view = planes[0].create_view(&Default::default());
        let chroma_view = planes[1].create_view(&Default::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("xgview-nv12-planes"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&luma_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&chroma_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        if let Some(previous) = self.channels.remove(&index) {
            self.free(&previous);
        }
        let id = self.state.renderer.write().register_native_texture(
            &self.state.device,
            &output_view,
            wgpu::FilterMode::Linear,
        );

        self.channels.insert(
            index,
            Channel {
                planes,
                output: output_view,
                bind_group,
                surface: VideoSurface { id, size: egui::vec2(size.0 as f32, size.1 as f32) },
                size,
                scratch: Vec::new(),
            },
        );
    }

    /// Records the pass turning a channel's planes into its texture.
    fn convert(&mut self, index: usize) {
        let Some(channel) = self.channels.get(&index) else {
            return;
        };
        let device = self.state.device.clone();
        let encoder = self
            .encoder
            .get_or_insert_with(|| device.create_command_encoder(&Default::default()));
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("xgview-nv12-convert"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &channel.output,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &channel.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// Size of the interleaved chroma plane, in samples of U and V pairs.
fn chroma_size(size: (u32, u32)) -> (u32, u32) {
    (size.0.div_ceil(2), size.1.div_ceil(2))
}

/// Rounds `value` up to the next multiple of `alignment`.
fn next_multiple_of(value: usize, alignment: usize) -> usize {
    value.div_ceil(alignment) * alignment
}

/// Creates one sampled plane texture.
fn create_plane(
    device: &wgpu::Device,
    size: (u32, u32),
    format: wgpu::TextureFormat,
    label: &str,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

/// Copies `rows` rows of `columns` bytes into a buffer whose rows are `stride`.
fn copy_plane(scratch: &mut Vec<u8>, source: &[u8], stride: usize, columns: usize, rows: usize) {
    scratch.clear();
    scratch.resize(stride * rows, 0);
    for row in 0..rows {
        let (from, to) = (row * columns, row * stride);
        if let (Some(source), Some(target)) =
            (source.get(from..from + columns), scratch.get_mut(to..to + columns))
        {
            target.copy_from_slice(source);
        }
    }
}

/// Uploads one padded plane into its texture.
fn write_plane(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    size: (u32, u32),
    stride: usize,
    data: &[u8],
) {
    queue.write_texture(
        texture.as_image_copy(),
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(stride as u32),
            rows_per_image: Some(size.1),
        },
        wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
    );
}

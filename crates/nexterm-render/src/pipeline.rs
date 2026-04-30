//! wgpu render pipeline: instanced cell shader, bind groups, vertex layout.

use bytemuck::{Pod, Zeroable};

// ---------------------------------------------------------------------------
// GPU data structures
// ---------------------------------------------------------------------------

/// Per-cell instance data sent to the GPU (72 bytes).
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct CellInstance {
    /// Cell top-left position in pixels.
    pub pos: [f32; 2],
    /// Atlas rect: [x, y, w, h] in texels. w=0 means no glyph (space).
    pub atlas_rect: [f32; 4],
    /// Offset from cell top-left to glyph bitmap top-left (pixels).
    pub glyph_offset: [f32; 2],
    /// Foreground colour (linear RGBA, [0..1]).
    pub fg: [f32; 4],
    /// Background colour (linear RGBA, [0..1]).
    pub bg: [f32; 4],
    /// Number of cells this instance spans (1.0 = normal, 2.0 = wide CJK).
    pub cell_span: f32,
    pub _pad: f32,
}

/// Uniform buffer shared by every cell (32 bytes, 16-byte aligned).
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct Uniforms {
    /// Cell size in pixels [w, h].
    pub cell_size: [f32; 2],
    /// Viewport size in pixels [w, h].
    pub viewport_size: [f32; 2],
    /// Atlas texture size in texels [w, h].
    pub atlas_size: [f32; 2],
    pub _pad: [f32; 2],
}

// ---------------------------------------------------------------------------
// WGSL shader
// ---------------------------------------------------------------------------

const SHADER_SOURCE: &str = r#"
struct Uniforms {
    cell_size:     vec2f,
    viewport_size: vec2f,
    atlas_size:    vec2f,
    _pad:          vec2f,
};

struct VertexOutput {
    @builtin(position) position: vec4f,
    @location(0)                              cell_uv:      vec2f,
    @location(1) @interpolate(flat)           atlas_rect:   vec4f,
    @location(2) @interpolate(flat)           glyph_offset: vec2f,
    @location(3) @interpolate(flat)           fg:           vec4f,
    @location(4) @interpolate(flat)           bg:           vec4f,
    @location(5) @interpolate(flat)           quad_size:    vec2f,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var atlas_tex:     texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;

@vertex
fn vs_main(
    @builtin(vertex_index) vi: u32,
    // -- per-instance attributes --
    @location(0) pos:          vec2f,
    @location(1) atlas_rect:   vec4f,
    @location(2) glyph_offset: vec2f,
    @location(3) fg:           vec4f,
    @location(4) bg:           vec4f,
    @location(5) cell_span:    vec2f,  // x = span (1 or 2), y = pad
) -> VertexOutput {
    // 6 vertices → 2 triangles forming a quad
    var corners = array<vec2f, 6>(
        vec2f(0.0, 0.0), vec2f(1.0, 0.0), vec2f(0.0, 1.0),
        vec2f(1.0, 0.0), vec2f(1.0, 1.0), vec2f(0.0, 1.0),
    );
    let corner = corners[vi];

    // Quad size: span * cell_width for x, normal cell_height for y
    let qs = vec2f(u.cell_size.x * cell_span.x, u.cell_size.y);

    // Pixel position of this vertex
    let pixel = pos + corner * qs;

    // Pixel → NDC (y-flip)
    let ndc = vec2f(
        pixel.x / u.viewport_size.x *  2.0 - 1.0,
        pixel.y / u.viewport_size.y * -2.0 + 1.0,
    );

    var out: VertexOutput;
    out.position     = vec4f(ndc, 0.0, 1.0);
    out.cell_uv      = corner;          // [0..1] within quad
    out.atlas_rect   = atlas_rect;
    out.glyph_offset = glyph_offset;
    out.fg           = fg;
    out.bg           = bg;
    out.quad_size    = qs;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4f {
    // Start with background fill
    var color = in.bg;

    let glyph_size = in.atlas_rect.zw;            // width, height in texels
    if (glyph_size.x > 0.0) {
        // Pixel coordinate within the quad
        let pix = in.cell_uv * in.quad_size;
        // Position relative to the glyph bitmap origin
        let g = pix - in.glyph_offset;

        if (g.x >= 0.0 && g.y >= 0.0 && g.x < glyph_size.x && g.y < glyph_size.y) {
            // pix already contains +0.5 (pixel center), so g is already at texel center
            let atlas_uv = (in.atlas_rect.xy + g) / u.atlas_size;
            let alpha = textureSample(atlas_tex, atlas_sampler, atlas_uv).r;
            color = mix(color, vec4f(in.fg.rgb, 1.0), alpha);
        }
    }

    return color;
}
"#;

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

/// Owns the wgpu render pipeline, bind-group layout, and sampler for the
/// instanced cell renderer.
pub struct CellPipeline {
    pub pipeline: wgpu::RenderPipeline,
    pub bind_group_layout: wgpu::BindGroupLayout,
    pub sampler: wgpu::Sampler,
}

impl CellPipeline {
    pub fn new(device: &wgpu::Device, surface_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cell-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SOURCE.into()),
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("cell-bgl"),
                entries: &[
                    // binding 0: uniforms
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    // binding 1: atlas texture
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    // binding 2: sampler
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cell-pl"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        // Instance buffer vertex layout
        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<CellInstance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2, // pos
                },
                wgpu::VertexAttribute {
                    offset: 8,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x4, // atlas_rect
                },
                wgpu::VertexAttribute {
                    offset: 24,
                    shader_location: 2,
                    format: wgpu::VertexFormat::Float32x2, // glyph_offset
                },
                wgpu::VertexAttribute {
                    offset: 32,
                    shader_location: 3,
                    format: wgpu::VertexFormat::Float32x4, // fg
                },
                wgpu::VertexAttribute {
                    offset: 48,
                    shader_location: 4,
                    format: wgpu::VertexFormat::Float32x4, // bg
                },
                wgpu::VertexAttribute {
                    offset: 64,
                    shader_location: 5,
                    format: wgpu::VertexFormat::Float32x2, // cell_span + pad
                },
            ],
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cell-rp"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[instance_layout],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        Self {
            pipeline,
            bind_group_layout,
            sampler,
        }
    }

    /// Create a bind group for the given uniform buffer and atlas texture.
    pub fn create_bind_group(
        &self,
        device: &wgpu::Device,
        uniform_buf: &wgpu::Buffer,
        atlas_view: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("cell-bg"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        })
    }
}

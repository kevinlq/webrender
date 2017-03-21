/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! The webrender API.
//!
//! The `webrender::renderer` module provides the interface to webrender, which
//! is accessible through [`Renderer`][renderer]
//!
//! [renderer]: struct.Renderer.html

use debug_colors;
use debug_render::DebugRenderer;
use device::{DepthFunction, Device, FrameId, ProgramId, TextureId, VertexFormat, GpuMarker, GpuProfiler};
use device::{GpuSample, TextureFilter, VAOId, VertexUsageHint, FileWatcherHandler, TextureTarget, ShaderError};
use euclid::Matrix4D;
use fnv::FnvHasher;
use frame_builder::FrameBuilderConfig;
use gleam::gl;
use gpu_store::{GpuStore, GpuStoreLayout};
use internal_types::{CacheTextureId, RendererFrame, ResultMsg, TextureUpdateOp};
use internal_types::{ExternalImageUpdateList, TextureUpdateList, PackedVertex, RenderTargetMode};
use internal_types::{ORTHO_NEAR_PLANE, ORTHO_FAR_PLANE, SourceTexture};
use internal_types::{BatchTextures, TextureSampler};
use prim_store::GradientData;
use profiler::{Profiler, BackendProfileCounters};
use profiler::{GpuProfileTag, RendererProfileTimers, RendererProfileCounters};
use record::ApiRecordingReceiver;
use render_backend::RenderBackend;
use render_task::RenderTaskData;
use std;
use std::cmp;
use std::collections::{HashMap, VecDeque};
use std::f32;
use std::hash::BuildHasherDefault;
use std::marker::PhantomData;
use std::mem;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use texture_cache::TextureCache;
use threadpool::ThreadPool;
use tiling::{AlphaBatchKind, BlurCommand, Frame, PrimitiveBatch, PrimitiveBatchData, RenderTarget};
use tiling::{AlphaRenderTarget, CacheClipInstance, PrimitiveInstance, ColorRenderTarget, RenderTargetKind};
use time::precise_time_ns;
use thread_profiler::{register_thread_with_profiler, write_profile};
use util::TransformedRectKind;
use webgl_types::GLContextHandleWrapper;
use webrender_traits::{ColorF, Epoch, PipelineId, RenderNotifier, RenderDispatcher};
use webrender_traits::{ExternalImageId, ExternalImageType, ImageData, ImageFormat, RenderApiSender};
use webrender_traits::{DeviceIntRect, DevicePoint, DeviceIntPoint, DeviceIntSize, DeviceUintSize};
use webrender_traits::{ImageDescriptor, BlobImageRenderer};
use webrender_traits::channel;
use webrender_traits::VRCompositorHandler;

pub const GPU_DATA_TEXTURE_POOL: usize = 5;
pub const MAX_VERTEX_TEXTURE_WIDTH: usize = 1024;

const GPU_TAG_CACHE_BOX_SHADOW: GpuProfileTag = GpuProfileTag { label: "C_BoxShadow", color: debug_colors::BLACK };
const GPU_TAG_CACHE_CLIP: GpuProfileTag = GpuProfileTag { label: "C_Clip", color: debug_colors::PURPLE };
const GPU_TAG_CACHE_TEXT_RUN: GpuProfileTag = GpuProfileTag { label: "C_TextRun", color: debug_colors::MISTYROSE };
const GPU_TAG_INIT: GpuProfileTag = GpuProfileTag { label: "Init", color: debug_colors::WHITE };
const GPU_TAG_SETUP_TARGET: GpuProfileTag = GpuProfileTag { label: "Target", color: debug_colors::SLATEGREY };
const GPU_TAG_PRIM_RECT: GpuProfileTag = GpuProfileTag { label: "Rect", color: debug_colors::RED };
const GPU_TAG_PRIM_IMAGE: GpuProfileTag = GpuProfileTag { label: "Image", color: debug_colors::GREEN };
const GPU_TAG_PRIM_IMAGE_RECT: GpuProfileTag = GpuProfileTag { label: "ImageRect", color: debug_colors::GREENYELLOW };
const GPU_TAG_PRIM_YUV_IMAGE: GpuProfileTag = GpuProfileTag { label: "YuvImage", color: debug_colors::DARKGREEN };
const GPU_TAG_PRIM_BLEND: GpuProfileTag = GpuProfileTag { label: "Blend", color: debug_colors::LIGHTBLUE };
const GPU_TAG_PRIM_HW_COMPOSITE: GpuProfileTag = GpuProfileTag { label: "HwComposite", color: debug_colors::DODGERBLUE };
const GPU_TAG_PRIM_COMPOSITE: GpuProfileTag = GpuProfileTag { label: "Composite", color: debug_colors::MAGENTA };
const GPU_TAG_PRIM_TEXT_RUN: GpuProfileTag = GpuProfileTag { label: "TextRun", color: debug_colors::BLUE };
const GPU_TAG_PRIM_GRADIENT: GpuProfileTag = GpuProfileTag { label: "Gradient", color: debug_colors::YELLOW };
const GPU_TAG_PRIM_ANGLE_GRADIENT: GpuProfileTag = GpuProfileTag { label: "AngleGradient", color: debug_colors::POWDERBLUE };
const GPU_TAG_PRIM_RADIAL_GRADIENT: GpuProfileTag = GpuProfileTag { label: "RadialGradient", color: debug_colors::LIGHTPINK };
const GPU_TAG_PRIM_BOX_SHADOW: GpuProfileTag = GpuProfileTag { label: "BoxShadow", color: debug_colors::CYAN };
const GPU_TAG_PRIM_BORDER: GpuProfileTag = GpuProfileTag { label: "Border", color: debug_colors::ORANGE };
const GPU_TAG_PRIM_CACHE_IMAGE: GpuProfileTag = GpuProfileTag { label: "CacheImage", color: debug_colors::SILVER };
const GPU_TAG_BLUR: GpuProfileTag = GpuProfileTag { label: "Blur", color: debug_colors::VIOLET };

#[derive(Debug, Copy, Clone)]
pub enum RendererKind {
    Native,
    OSMesa,
}

#[derive(Debug)]
pub struct GpuProfile {
    pub frame_id: FrameId,
    pub paint_time_ns: u64,
}

impl GpuProfile {
    fn new<T>(frame_id: FrameId, samples: &[GpuSample<T>]) -> GpuProfile {
        let mut paint_time_ns = 0;
        for sample in samples {
            paint_time_ns += sample.time_ns;
        }
        GpuProfile {
            frame_id: frame_id,
            paint_time_ns: paint_time_ns,
        }
    }
}

#[derive(Debug)]
pub struct CpuProfile {
    pub frame_id: FrameId,
    pub composite_time_ns: u64,
    pub draw_calls: usize,
}

impl CpuProfile {
    fn new(frame_id: FrameId,
           composite_time_ns: u64,
           draw_calls: usize) -> CpuProfile {
        CpuProfile {
            frame_id: frame_id,
            composite_time_ns: composite_time_ns,
            draw_calls: draw_calls,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum BlendMode {
    None,
    Alpha,
    PremultipliedAlpha,

    // Use the color of the text itself as a constant color blend factor.
    Subpixel(ColorF),
}

struct GpuDataTexture<L> {
    id: TextureId,
    layout: PhantomData<L>,
}

impl<L: GpuStoreLayout> GpuDataTexture<L> {
    fn new(device: &mut Device) -> GpuDataTexture<L> {
        let id = device.create_texture_ids(1, TextureTarget::Default)[0];

        GpuDataTexture {
            id: id,
            layout: PhantomData,
        }
    }

    fn init<T: Default>(&mut self,
                        device: &mut Device,
                        data: &mut Vec<T>) {
        if data.is_empty() {
            return;
        }

        let items_per_row = L::items_per_row::<T>();
        let rows_per_item = L::rows_per_item::<T>();

        // Extend the data array to be a multiple of the row size.
        // This ensures memory safety when the array is passed to
        // OpenGL to upload to the GPU.
        if items_per_row != 0 {
            while data.len() % items_per_row != 0 {
                data.push(T::default());
            }
        }

        let height = if items_per_row != 0 {
            data.len() / items_per_row
        } else {
            data.len() * rows_per_item
        };

        device.init_texture(self.id,
                            L::texture_width::<T>() as u32,
                            height as u32,
                            L::image_format(),
                            L::texture_filter(),
                            RenderTargetMode::None,
                            Some(unsafe { mem::transmute(data.as_slice()) } ));
    }
}

pub struct VertexDataTextureLayout {}

impl GpuStoreLayout for VertexDataTextureLayout {
    fn image_format() -> ImageFormat {
        ImageFormat::RGBAF32
    }

    fn texture_width<T>() -> usize {
        MAX_VERTEX_TEXTURE_WIDTH - (MAX_VERTEX_TEXTURE_WIDTH % Self::texels_per_item::<T>())
    }

    fn texture_filter() -> TextureFilter {
        TextureFilter::Nearest
    }
}

type VertexDataTexture = GpuDataTexture<VertexDataTextureLayout>;
pub type VertexDataStore<T> = GpuStore<T, VertexDataTextureLayout>;

pub struct GradientDataTextureLayout {}

impl GpuStoreLayout for GradientDataTextureLayout {
    fn image_format() -> ImageFormat {
        ImageFormat::RGBA8
    }

    fn texture_width<T>() -> usize {
        mem::size_of::<GradientData>() / Self::texel_size() / 2
    }

    fn texture_filter() -> TextureFilter {
        TextureFilter::Linear
    }
}

type GradientDataTexture = GpuDataTexture<GradientDataTextureLayout>;
pub type GradientDataStore = GpuStore<GradientData, GradientDataTextureLayout>;

const TRANSFORM_FEATURE: &'static str = "TRANSFORM";
const SUBPIXEL_AA_FEATURE: &'static str = "SUBPIXEL_AA";
const CLIP_FEATURE: &'static str = "CLIP";
const TEXTURE_RECT_FEATURE: &'static str = "TEXTURE_RECT";

enum ShaderKind {
    Primitive,
    Cache(VertexFormat),
    ClipCache,
}

struct LazilyCompiledShader {
    id: Option<ProgramId>,
    name: &'static str,
    kind: ShaderKind,
    features: Vec<&'static str>,
}

impl LazilyCompiledShader {
    fn new(kind: ShaderKind,
           name: &'static str,
           features: &[&'static str],
           device: &mut Device,
           precache: bool) -> Result<LazilyCompiledShader, ShaderError> {
        let mut shader = LazilyCompiledShader {
            id: None,
            name: name,
            kind: kind,
            features: features.to_vec(),
        };

        if precache {
            try!{ shader.get(device) };
        }

        Ok(shader)
    }

    fn get(&mut self, device: &mut Device) -> Result<ProgramId, ShaderError> {
        if self.id.is_none() {
            let id = try!{
                match self.kind {
                    ShaderKind::Primitive => {
                        create_prim_shader(self.name,
                                           device,
                                           &self.features,
                                           VertexFormat::Triangles)
                    }
                    ShaderKind::Cache(format) => {
                        create_prim_shader(self.name,
                                           device,
                                           &self.features,
                                           format)
                    }
                    ShaderKind::ClipCache => {
                        create_clip_shader(self.name, device)
                    }
                }
            };
            self.id = Some(id);
        }

        Ok(self.id.unwrap())
    }
}

struct PrimitiveShader {
    simple: LazilyCompiledShader,
    transform: LazilyCompiledShader,
}

struct FileWatcher {
    notifier: Arc<Mutex<Option<Box<RenderNotifier>>>>,
    result_tx: Sender<ResultMsg>,
}

impl FileWatcherHandler for FileWatcher {
    fn file_changed(&self, path: PathBuf) {
        self.result_tx.send(ResultMsg::RefreshShader(path)).ok();
        let mut notifier = self.notifier.lock();
        notifier.as_mut().unwrap().as_mut().unwrap().new_frame_ready();
    }
}

fn _get_ubo_max_len<T>(max_ubo_size: usize) -> usize {
    let item_size = mem::size_of::<T>();
    let max_items = max_ubo_size / item_size;

    // TODO(gw): Clamping to 1024 since some shader compilers
    //           seem to go very slow when you have high
    //           constants for array lengths. Investigate
    //           whether this clamping actually hurts performance!
    cmp::min(max_items, 1024)
}

impl PrimitiveShader {
    fn new(name: &'static str,
           device: &mut Device,
           features: &[&'static str],
           precache: bool) -> Result<PrimitiveShader, ShaderError> {
        let simple = try!{
            LazilyCompiledShader::new(ShaderKind::Primitive,
                                      name,
                                      features,
                                      device,
                                      precache)
        };

        let mut transform_features = features.to_vec();
        transform_features.push(TRANSFORM_FEATURE);

        let transform = try!{
            LazilyCompiledShader::new(ShaderKind::Primitive,
                                      name,
                                      &transform_features,
                                      device,
                                      precache)
        };

        Ok(PrimitiveShader {
            simple: simple,
            transform: transform,
        })
    }

    fn get(&mut self,
           device: &mut Device,
           transform_kind: TransformedRectKind) -> Result<ProgramId, ShaderError> {
        match transform_kind {
            TransformedRectKind::AxisAligned => self.simple.get(device),
            TransformedRectKind::Complex => self.transform.get(device),
        }
    }
}

fn create_prim_shader(name: &'static str,
                      device: &mut Device,
                      features: &[&'static str],
                      vertex_format: VertexFormat) -> Result<ProgramId, ShaderError> {
    let mut prefix = format!("#define WR_MAX_VERTEX_TEXTURE_WIDTH {}\n",
                              MAX_VERTEX_TEXTURE_WIDTH);

    for feature in features {
        prefix.push_str(&format!("#define WR_FEATURE_{}\n", feature));
    }

    debug!("PrimShader {}", name);

    let includes = &["prim_shared"];
    device.create_program_with_prefix(name, includes, Some(prefix), vertex_format)
}

fn create_clip_shader(name: &'static str, device: &mut Device) -> Result<ProgramId, ShaderError> {
    let prefix = format!("#define WR_MAX_VERTEX_TEXTURE_WIDTH {}\n
                          #define WR_FEATURE_TRANSFORM",
                          MAX_VERTEX_TEXTURE_WIDTH);

    debug!("ClipShader {}", name);

    let includes = &["prim_shared", "clip_shared"];
    device.create_program_with_prefix(name, includes, Some(prefix), VertexFormat::Clip)
}

struct GpuDataTextures {
    layer_texture: VertexDataTexture,
    render_task_texture: VertexDataTexture,
    prim_geom_texture: VertexDataTexture,
    data16_texture: VertexDataTexture,
    data32_texture: VertexDataTexture,
    data64_texture: VertexDataTexture,
    data128_texture: VertexDataTexture,
    resource_rects_texture: VertexDataTexture,
    gradient_data_texture: GradientDataTexture,
}

impl GpuDataTextures {
    fn new(device: &mut Device) -> GpuDataTextures {
        GpuDataTextures {
            layer_texture: VertexDataTexture::new(device),
            render_task_texture: VertexDataTexture::new(device),
            prim_geom_texture: VertexDataTexture::new(device),
            data16_texture: VertexDataTexture::new(device),
            data32_texture: VertexDataTexture::new(device),
            data64_texture: VertexDataTexture::new(device),
            data128_texture: VertexDataTexture::new(device),
            resource_rects_texture: VertexDataTexture::new(device),
            gradient_data_texture: GradientDataTexture::new(device),
        }
    }

    fn init_frame(&mut self, device: &mut Device, frame: &mut Frame) {
        self.data16_texture.init(device, &mut frame.gpu_data16);
        self.data32_texture.init(device, &mut frame.gpu_data32);
        self.data64_texture.init(device, &mut frame.gpu_data64);
        self.data128_texture.init(device, &mut frame.gpu_data128);
        self.prim_geom_texture.init(device, &mut frame.gpu_geometry);
        self.resource_rects_texture.init(device, &mut frame.gpu_resource_rects);
        self.layer_texture.init(device, &mut frame.layer_texture_data);
        self.render_task_texture.init(device, &mut frame.render_task_data);
        self.gradient_data_texture.init(device, &mut frame.gpu_gradient_data);

        device.bind_texture(TextureSampler::Layers, self.layer_texture.id);
        device.bind_texture(TextureSampler::RenderTasks, self.render_task_texture.id);
        device.bind_texture(TextureSampler::Geometry, self.prim_geom_texture.id);
        device.bind_texture(TextureSampler::Data16, self.data16_texture.id);
        device.bind_texture(TextureSampler::Data32, self.data32_texture.id);
        device.bind_texture(TextureSampler::Data64, self.data64_texture.id);
        device.bind_texture(TextureSampler::Data128, self.data128_texture.id);
        device.bind_texture(TextureSampler::ResourceRects, self.resource_rects_texture.id);
        device.bind_texture(TextureSampler::Gradients, self.gradient_data_texture.id);
    }
}

/// The renderer is responsible for submitting to the GPU the work prepared by the
/// RenderBackend.
pub struct Renderer {
    result_rx: Receiver<ResultMsg>,
    device: Device,
    pending_texture_updates: Vec<TextureUpdateList>,
    pending_shader_updates: Vec<PathBuf>,
    current_frame: Option<RendererFrame>,

    // These are "cache shaders". These shaders are used to
    // draw intermediate results to cache targets. The results
    // of these shaders are then used by the primitive shaders.
    cs_box_shadow: LazilyCompiledShader,
    cs_text_run: LazilyCompiledShader,
    cs_blur: LazilyCompiledShader,
    /// These are "cache clip shaders". These shaders are used to
    /// draw clip instances into the cached clip mask. The results
    /// of these shaders are also used by the primitive shaders.
    cs_clip_rectangle: LazilyCompiledShader,
    cs_clip_image: LazilyCompiledShader,

    // The are "primitive shaders". These shaders draw and blend
    // final results on screen. They are aware of tile boundaries.
    // Most draw directly to the framebuffer, but some use inputs
    // from the cache shaders to draw. Specifically, the box
    // shadow primitive shader stretches the box shadow cache
    // output, and the cache_image shader blits the results of
    // a cache shader (e.g. blur) to the screen.
    ps_rectangle: PrimitiveShader,
    ps_rectangle_clip: PrimitiveShader,
    ps_text_run: PrimitiveShader,
    ps_text_run_subpixel: PrimitiveShader,
    ps_image: PrimitiveShader,
    ps_image_rect: PrimitiveShader,
    ps_yuv_image: PrimitiveShader,
    ps_border: PrimitiveShader,
    ps_gradient: PrimitiveShader,
    ps_angle_gradient: PrimitiveShader,
    ps_radial_gradient: PrimitiveShader,
    ps_box_shadow: PrimitiveShader,
    ps_cache_image: PrimitiveShader,

    ps_blend: LazilyCompiledShader,
    ps_hw_composite: LazilyCompiledShader,
    ps_composite: LazilyCompiledShader,

    notifier: Arc<Mutex<Option<Box<RenderNotifier>>>>,

    enable_profiler: bool,
    max_recorded_profiles: usize,
    clear_framebuffer: bool,
    clear_color: ColorF,
    debug: DebugRenderer,
    render_target_debug: bool,
    backend_profile_counters: BackendProfileCounters,
    profile_counters: RendererProfileCounters,
    profiler: Profiler,
    last_time: u64,

    color_render_targets: Vec<TextureId>,
    alpha_render_targets: Vec<TextureId>,

    gpu_profile: GpuProfiler<GpuProfileTag>,
    prim_vao_id: VAOId,
    blur_vao_id: VAOId,
    clip_vao_id: VAOId,

    gdt_index: usize,
    gpu_data_textures: [GpuDataTextures; GPU_DATA_TEXTURE_POOL],

    pipeline_epoch_map: HashMap<PipelineId, Epoch, BuildHasherDefault<FnvHasher>>,
    /// Used to dispatch functions to the main thread's event loop.
    /// Required to allow GLContext sharing in some implementations like WGL.
    main_thread_dispatcher: Arc<Mutex<Option<Box<RenderDispatcher>>>>,

    /// A vector for fast resolves of texture cache IDs to
    /// native texture IDs. This maps to a free-list managed
    /// by the backend thread / texture cache. We free the
    /// texture memory associated with a TextureId when its
    /// texture cache ID is freed by the texture cache, but
    /// reuse the TextureId when the texture caches's free
    /// list reuses the texture cache ID. This saves having to
    /// use a hashmap, and allows a flat vector for performance.
    cache_texture_id_map: Vec<TextureId>,

    /// A special 1x1 dummy cache texture used for shaders that expect to work
    /// with the cache but are actually running in the first pass
    /// when no target is yet provided as a cache texture input.
    dummy_cache_texture_id: TextureId,

    dither_matrix_texture_id: TextureId,

    /// Optional trait object that allows the client
    /// application to provide external buffers for image data.
    external_image_handler: Option<Box<ExternalImageHandler>>,

    /// Map of external image IDs to native textures.
    external_images: HashMap<ExternalImageId, TextureId, BuildHasherDefault<FnvHasher>>,

    // Optional trait object that handles WebVR commands.
    // Some WebVR commands such as SubmitFrame must be synced with the WebGL render thread.
    vr_compositor_handler: Arc<Mutex<Option<Box<VRCompositorHandler>>>>,

    /// List of profile results from previous frames. Can be retrieved
    /// via get_frame_profiles().
    cpu_profiles: VecDeque<CpuProfile>,
    gpu_profiles: VecDeque<GpuProfile>,
}

#[derive(Debug)]
pub enum InitError {
    Shader(ShaderError),
    Thread(std::io::Error),
}

impl From<ShaderError> for InitError {
    fn from(err: ShaderError) -> Self { InitError::Shader(err) }
}

impl From<std::io::Error> for InitError {
    fn from(err: std::io::Error) -> Self { InitError::Thread(err) }
}

impl Renderer {
    /// Initializes webrender and creates a Renderer and RenderApiSender.
    ///
    /// # Examples
    /// Initializes a Renderer with some reasonable values. For more information see
    /// [RendererOptions][rendereroptions].
    /// [rendereroptions]: struct.RendererOptions.html
    ///
    /// ```rust,ignore
    /// # use webrender::renderer::Renderer;
    /// # use std::path::PathBuf;
    /// let opts = webrender::RendererOptions {
    ///    device_pixel_ratio: 1.0,
    ///    resource_override_path: None,
    ///    enable_aa: false,
    ///    enable_profiler: false,
    /// };
    /// let (renderer, sender) = Renderer::new(opts);
    /// ```
    pub fn new(gl: Rc<gl::Gl>,
               mut options: RendererOptions,
               initial_window_size: DeviceUintSize) -> Result<(Renderer, RenderApiSender), InitError> {
        let (api_tx, api_rx) = try!{ channel::msg_channel() };
        let (payload_tx, payload_rx) = try!{ channel::payload_channel() };
        let (result_tx, result_rx) = channel();

        register_thread_with_profiler("Compositor".to_owned());

        let notifier = Arc::new(Mutex::new(None));

        let file_watch_handler = FileWatcher {
            result_tx: result_tx.clone(),
            notifier: notifier.clone(),
        };

        let mut device = Device::new(gl,
                                     options.resource_override_path.clone(),
                                     Box::new(file_watch_handler));
        // device-pixel ratio doesn't matter here - we are just creating resources.
        device.begin_frame(1.0);

        let cs_box_shadow = try!{
            LazilyCompiledShader::new(ShaderKind::Cache(VertexFormat::Triangles),
                                      "cs_box_shadow",
                                      &[],
                                      &mut device,
                                      options.precache_shaders)
        };

        let cs_text_run = try!{
            LazilyCompiledShader::new(ShaderKind::Cache(VertexFormat::Triangles),
                                      "cs_text_run",
                                      &[],
                                      &mut device,
                                      options.precache_shaders)
        };

        let cs_blur = try!{
            LazilyCompiledShader::new(ShaderKind::Cache(VertexFormat::Blur),
                                     "cs_blur",
                                      &[],
                                      &mut device,
                                      options.precache_shaders)
        };

        let cs_clip_rectangle = try!{
            LazilyCompiledShader::new(ShaderKind::ClipCache,
                                      "cs_clip_rectangle",
                                      &[],
                                      &mut device,
                                      options.precache_shaders)
        };

        let cs_clip_image = try!{
            LazilyCompiledShader::new(ShaderKind::ClipCache,
                                      "cs_clip_image",
                                      &[],
                                      &mut device,
                                      options.precache_shaders)
        };

        let ps_rectangle = try!{
            PrimitiveShader::new("ps_rectangle",
                                 &mut device,
                                 &[],
                                 options.precache_shaders)
        };

        let ps_rectangle_clip = try!{
            PrimitiveShader::new("ps_rectangle",
                                 &mut device,
                                 &[ CLIP_FEATURE ],
                                 options.precache_shaders)
        };

        let ps_text_run = try!{
            PrimitiveShader::new("ps_text_run",
                                 &mut device,
                                 &[],
                                 options.precache_shaders)
        };

        let ps_text_run_subpixel = try!{
            PrimitiveShader::new("ps_text_run",
                                 &mut device,
                                 &[ SUBPIXEL_AA_FEATURE ],
                                 options.precache_shaders)
        };

        let ps_image = try!{
            PrimitiveShader::new("ps_image",
                                 &mut device,
                                 &[],
                                 options.precache_shaders)
        };

        let ps_image_rect = try!{
            PrimitiveShader::new("ps_image",
                                 &mut device,
                                 &[ TEXTURE_RECT_FEATURE ],
                                 options.precache_shaders)
        };

        let ps_yuv_image = try!{
            PrimitiveShader::new("ps_yuv_image",
                                 &mut device,
                                 &[],
                                 options.precache_shaders)
        };

        let ps_border = try!{
            PrimitiveShader::new("ps_border",
                                 &mut device,
                                 &[],
                                 options.precache_shaders)
        };

        let ps_box_shadow = try!{
            PrimitiveShader::new("ps_box_shadow",
                                 &mut device,
                                 &[],
                                 options.precache_shaders)
        };

        let ps_gradient = try!{
            PrimitiveShader::new("ps_gradient",
                                 &mut device,
                                 &[],
                                 options.precache_shaders)
        };

        let ps_angle_gradient = try!{
            PrimitiveShader::new("ps_angle_gradient",
                                 &mut device,
                                 &[],
                                 options.precache_shaders)
        };

        let ps_radial_gradient = try!{
            PrimitiveShader::new("ps_radial_gradient",
                                 &mut device,
                                 &[],
                                 options.precache_shaders)
        };

        let ps_cache_image = try!{
            PrimitiveShader::new("ps_cache_image",
                                 &mut device,
                                 &[],
                                 options.precache_shaders)
        };

        let ps_blend = try!{
            LazilyCompiledShader::new(ShaderKind::Primitive,
                                     "ps_blend",
                                     &[],
                                     &mut device,
                                     options.precache_shaders)
        };

        let ps_composite = try!{
            LazilyCompiledShader::new(ShaderKind::Primitive,
                                      "ps_composite",
                                      &[],
                                      &mut device,
                                      options.precache_shaders)
        };

        let ps_hw_composite = try!{
            LazilyCompiledShader::new(ShaderKind::Primitive,
                                     "ps_hardware_composite",
                                     &[],
                                     &mut device,
                                     options.precache_shaders)
        };

        let device_max_size = device.max_texture_size();
        let max_texture_size = cmp::min(device_max_size, options.max_texture_size.unwrap_or(device_max_size));

        let mut texture_cache = TextureCache::new(max_texture_size);
        let mut backend_profile_counters = BackendProfileCounters::new();

        let white_pixels: Vec<u8> = vec![
            0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff,
        ];
        let mask_pixels: Vec<u8> = vec![
            0xff, 0xff,
            0xff, 0xff,
        ];

        let dither_matrix: [u8; 64] = [
            00, 48, 12, 60, 03, 51, 15, 63,
            32, 16, 44, 28, 35, 19, 47, 31,
            08, 56, 04, 52, 11, 59, 07, 55,
            40, 24, 36, 20, 43, 27, 39, 23,
            02, 50, 14, 62, 01, 49, 13, 61,
            34, 18, 46, 30, 33, 17, 45, 29,
            10, 58, 06, 54, 09, 57, 05, 53,
            42, 26, 38, 22, 41, 25, 37, 21
        ];

        // TODO: Ensure that the white texture can never get evicted when the cache supports LRU eviction!
        let white_image_id = texture_cache.new_item_id();
        texture_cache.insert(white_image_id,
                             ImageDescriptor::new(2, 2, ImageFormat::RGBA8, false),
                             TextureFilter::Linear,
                             ImageData::Raw(Arc::new(white_pixels)),
                             &mut backend_profile_counters.texture_cache);

        let dummy_mask_image_id = texture_cache.new_item_id();
        texture_cache.insert(dummy_mask_image_id,
                             ImageDescriptor::new(2, 2, ImageFormat::A8, false),
                             TextureFilter::Linear,
                             ImageData::Raw(Arc::new(mask_pixels)),
                             &mut backend_profile_counters.texture_cache);

        let dummy_cache_texture_id = device.create_texture_ids(1, TextureTarget::Array)[0];
        device.init_texture(dummy_cache_texture_id,
                            1,
                            1,
                            ImageFormat::RGBA8,
                            TextureFilter::Linear,
                            RenderTargetMode::LayerRenderTarget(1),
                            None);

        let dither_matrix_texture_id = device.create_texture_ids(1, TextureTarget::Default)[0];
        device.init_texture(dither_matrix_texture_id,
                            8,
                            8,
                            ImageFormat::A8,
                            TextureFilter::Nearest,
                            RenderTargetMode::None,
                            Some(&dither_matrix));

        let debug_renderer = DebugRenderer::new(&mut device);

        let gpu_data_textures = [
            GpuDataTextures::new(&mut device),
            GpuDataTextures::new(&mut device),
            GpuDataTextures::new(&mut device),
            GpuDataTextures::new(&mut device),
            GpuDataTextures::new(&mut device),
        ];

        let x0 = 0.0;
        let y0 = 0.0;
        let x1 = 1.0;
        let y1 = 1.0;

        // TODO(gw): Consider separate VBO for quads vs border corners if VS ever shows up in profile!
        let quad_indices: [u16; 6] = [ 0, 1, 2, 2, 1, 3 ];
        let quad_vertices = [
            PackedVertex {
                pos: [x0, y0],
            },
            PackedVertex {
                pos: [x1, y0],
            },
            PackedVertex {
                pos: [x0, y1],
            },
            PackedVertex {
                pos: [x1, y1],
            },
        ];

        let prim_vao_id = device.create_vao(VertexFormat::Triangles, mem::size_of::<PrimitiveInstance>() as i32);
        device.bind_vao(prim_vao_id);
        device.update_vao_indices(prim_vao_id, &quad_indices, VertexUsageHint::Static);
        device.update_vao_main_vertices(prim_vao_id, &quad_vertices, VertexUsageHint::Static);

        let blur_vao_id = device.create_vao_with_new_instances(VertexFormat::Blur, mem::size_of::<BlurCommand>() as i32, prim_vao_id);
        let clip_vao_id = device.create_vao_with_new_instances(VertexFormat::Clip, mem::size_of::<CacheClipInstance>() as i32, prim_vao_id);

        device.end_frame();

        let main_thread_dispatcher = Arc::new(Mutex::new(None));
        let backend_notifier = notifier.clone();
        let backend_main_thread_dispatcher = main_thread_dispatcher.clone();

        let vr_compositor = Arc::new(Mutex::new(None));
        let backend_vr_compositor = vr_compositor.clone();

        // We need a reference to the webrender context from the render backend in order to share
        // texture ids
        let context_handle = match options.renderer_kind {
            RendererKind::Native => GLContextHandleWrapper::current_native_handle(),
            RendererKind::OSMesa => GLContextHandleWrapper::current_osmesa_handle(),
        };

        let config = FrameBuilderConfig::new(options.enable_scrollbars,
                                             options.enable_subpixel_aa,
                                             options.debug);

        let (device_pixel_ratio, enable_aa) = (options.device_pixel_ratio, options.enable_aa);
        let render_target_debug = options.render_target_debug;
        let payload_tx_for_backend = payload_tx.clone();
        let recorder = options.recorder;
        let workers = options.workers.take().unwrap_or_else(||{
            // TODO(gw): Use a heuristic to select best # of worker threads.
            Arc::new(Mutex::new(ThreadPool::new_with_name("WebRender:Worker".to_string(), 4)))
        });

        let blob_image_renderer = options.blob_image_renderer.take();
        try!{ thread::Builder::new().name("RenderBackend".to_string()).spawn(move || {
            let mut backend = RenderBackend::new(api_rx,
                                                 payload_rx,
                                                 payload_tx_for_backend,
                                                 result_tx,
                                                 device_pixel_ratio,
                                                 texture_cache,
                                                 enable_aa,
                                                 workers,
                                                 backend_notifier,
                                                 context_handle,
                                                 config,
                                                 recorder,
                                                 backend_main_thread_dispatcher,
                                                 blob_image_renderer,
                                                 backend_vr_compositor,
                                                 initial_window_size);
            backend.run(backend_profile_counters);
        })};

        let gpu_profile = GpuProfiler::new(device.rc_gl());

        let renderer = Renderer {
            result_rx: result_rx,
            device: device,
            current_frame: None,
            pending_texture_updates: Vec::new(),
            pending_shader_updates: Vec::new(),
            cs_box_shadow: cs_box_shadow,
            cs_text_run: cs_text_run,
            cs_blur: cs_blur,
            cs_clip_rectangle: cs_clip_rectangle,
            cs_clip_image: cs_clip_image,
            ps_rectangle: ps_rectangle,
            ps_rectangle_clip: ps_rectangle_clip,
            ps_text_run: ps_text_run,
            ps_text_run_subpixel: ps_text_run_subpixel,
            ps_image: ps_image,
            ps_image_rect: ps_image_rect,
            ps_yuv_image: ps_yuv_image,
            ps_border: ps_border,
            ps_box_shadow: ps_box_shadow,
            ps_gradient: ps_gradient,
            ps_angle_gradient: ps_angle_gradient,
            ps_radial_gradient: ps_radial_gradient,
            ps_cache_image: ps_cache_image,
            ps_blend: ps_blend,
            ps_hw_composite: ps_hw_composite,
            ps_composite: ps_composite,
            notifier: notifier,
            debug: debug_renderer,
            render_target_debug: render_target_debug,
            backend_profile_counters: BackendProfileCounters::new(),
            profile_counters: RendererProfileCounters::new(),
            profiler: Profiler::new(),
            enable_profiler: options.enable_profiler,
            max_recorded_profiles: options.max_recorded_profiles,
            clear_framebuffer: options.clear_framebuffer,
            clear_color: options.clear_color,
            last_time: 0,
            color_render_targets: Vec::new(),
            alpha_render_targets: Vec::new(),
            gpu_profile: gpu_profile,
            prim_vao_id: prim_vao_id,
            blur_vao_id: blur_vao_id,
            clip_vao_id: clip_vao_id,
            gdt_index: 0,
            gpu_data_textures: gpu_data_textures,
            pipeline_epoch_map: HashMap::with_hasher(Default::default()),
            main_thread_dispatcher: main_thread_dispatcher,
            cache_texture_id_map: Vec::new(),
            dummy_cache_texture_id: dummy_cache_texture_id,
            dither_matrix_texture_id: dither_matrix_texture_id,
            external_image_handler: None,
            external_images: HashMap::with_hasher(Default::default()),
            vr_compositor_handler: vr_compositor,
            cpu_profiles: VecDeque::new(),
            gpu_profiles: VecDeque::new(),
        };

        let sender = RenderApiSender::new(api_tx, payload_tx);
        Ok((renderer, sender))
    }

    pub fn gl(&self) -> &gl::Gl {
        self.device.gl()
    }

    /// Sets the new RenderNotifier.
    ///
    /// The RenderNotifier will be called when processing e.g. of a (scrolling) frame is done,
    /// and therefore the screen should be updated.
    pub fn set_render_notifier(&self, notifier: Box<RenderNotifier>) {
        let mut notifier_arc = self.notifier.lock().unwrap();
        *notifier_arc = Some(notifier);
    }

    /// Sets the new MainThreadDispatcher.
    ///
    /// Allows to dispatch functions to the main thread's event loop.
    pub fn set_main_thread_dispatcher(&self, dispatcher: Box<RenderDispatcher>) {
        let mut dispatcher_arc = self.main_thread_dispatcher.lock().unwrap();
        *dispatcher_arc = Some(dispatcher);
    }

    /// Sets the VRCompositorHandler.
    ///
    /// It's used to handle WebVR render commands.
    /// Some WebVR commands such as Vsync and SubmitFrame must be called in the WebGL render thread.
    pub fn set_vr_compositor_handler(&self, creator: Box<VRCompositorHandler>) {
        let mut handler_arc = self.vr_compositor_handler.lock().unwrap();
        *handler_arc = Some(creator);
    }

    /// Returns the Epoch of the current frame in a pipeline.
    pub fn current_epoch(&self, pipeline_id: PipelineId) -> Option<Epoch> {
        self.pipeline_epoch_map.get(&pipeline_id).cloned()
    }

    /// Returns a HashMap containing the pipeline ids that have been received by the renderer and
    /// their respective epochs since the last time the method was called.
    pub fn flush_rendered_epochs(&mut self) -> HashMap<PipelineId, Epoch, BuildHasherDefault<FnvHasher>> {
        return mem::replace(&mut self.pipeline_epoch_map, HashMap::with_hasher(Default::default()));
    }

    /// Processes the result queue.
    ///
    /// Should be called before `render()`, as texture cache updates are done here.
    pub fn update(&mut self) {
        profile_scope!("update");

        // Pull any pending results and return the most recent.
        while let Ok(msg) = self.result_rx.try_recv() {
            match msg {
                ResultMsg::NewFrame(frame, texture_update_list, external_image_update_list, profile_counters) => {
                    self.pending_texture_updates.push(texture_update_list);

                    // When a new frame is ready, we could start to update all pending external image requests here.
                    self.release_external_images(external_image_update_list);

                    self.backend_profile_counters = profile_counters;

                    // Update the list of available epochs for use during reftests.
                    // This is a workaround for https://github.com/servo/servo/issues/13149.
                    for (pipeline_id, epoch) in &frame.pipeline_epoch_map {
                        self.pipeline_epoch_map.insert(*pipeline_id, *epoch);
                    }

                    self.current_frame = Some(frame);
                }
                ResultMsg::RefreshShader(path) => {
                    self.pending_shader_updates.push(path);
                }
            }
        }
    }

    // Get the real (OpenGL) texture ID for a given source texture.
    // For a texture cache texture, the IDs are stored in a vector
    // map for fast access. For WebGL textures, the native texture ID
    // is stored inline. When we add support for external textures,
    // we will add a callback here that is able to ask the caller
    // for the image data.
    fn resolve_source_texture(&mut self, texture_id: &SourceTexture) -> TextureId {
        match *texture_id {
            SourceTexture::Invalid => TextureId::invalid(),
            SourceTexture::WebGL(id) => TextureId::new(id, TextureTarget::Default),
            SourceTexture::External(external_image) => {
                *self.external_images
                     .get(&external_image.id)
                     .expect("BUG: External image should be resolved by now!")
            }
            SourceTexture::TextureCache(index) => {
                self.cache_texture_id_map[index.0]
            }
        }
    }

    /// Set a callback for handling external images.
    pub fn set_external_image_handler(&mut self, handler: Box<ExternalImageHandler>) {
        self.external_image_handler = Some(handler);
    }

    /// Retrieve (and clear) the current list of recorded frame profiles.
    pub fn get_frame_profiles(&mut self) -> (Vec<CpuProfile>, Vec<GpuProfile>) {
        let cpu_profiles = self.cpu_profiles.drain(..).collect();
        let gpu_profiles = self.gpu_profiles.drain(..).collect();
        (cpu_profiles, gpu_profiles)
    }

    /// Renders the current frame.
    ///
    /// A Frame is supplied by calling [set_root_stacking_context()][newframe].
    /// [newframe]: ../../webrender_traits/struct.RenderApi.html#method.set_root_stacking_context
    pub fn render(&mut self, framebuffer_size: DeviceUintSize) {
        profile_scope!("render");

        if let Some(mut frame) = self.current_frame.take() {
            if let Some(ref mut frame) = frame.frame {
                let mut profile_timers = RendererProfileTimers::new();

                // Block CPU waiting for last frame's GPU profiles to arrive.
                // In general this shouldn't block unless heavily GPU limited.
                if let Some((gpu_frame_id, samples)) = self.gpu_profile.build_samples() {
                    if self.max_recorded_profiles > 0 {
                        while self.gpu_profiles.len() >= self.max_recorded_profiles {
                            self.gpu_profiles.pop_front();
                        }
                        self.gpu_profiles.push_back(GpuProfile::new(gpu_frame_id, &samples));
                    }
                    profile_timers.gpu_samples = samples;
                }

                let cpu_frame_id = profile_timers.cpu_time.profile(|| {
                    let cpu_frame_id = self.device.begin_frame(frame.device_pixel_ratio);
                    self.gpu_profile.begin_frame(cpu_frame_id);
                    {
                        let _gm = self.gpu_profile.add_marker(GPU_TAG_INIT);

                        self.device.disable_scissor();
                        self.device.disable_depth();
                        self.device.set_blend(false);

                        //self.update_shaders();
                        self.update_texture_cache();
                    }

                    self.draw_tile_frame(frame, &framebuffer_size);

                    self.gpu_profile.end_frame();
                    cpu_frame_id
                });

                let current_time = precise_time_ns();
                let ns = current_time - self.last_time;
                self.profile_counters.frame_time.set(ns);

                if self.max_recorded_profiles > 0 {
                    while self.cpu_profiles.len() >= self.max_recorded_profiles {
                        self.cpu_profiles.pop_front();
                    }
                    let cpu_profile = CpuProfile::new(cpu_frame_id,
                                                      profile_timers.cpu_time.get(),
                                                      self.profile_counters.draw_calls.get());
                    self.cpu_profiles.push_back(cpu_profile);
                }

                if self.enable_profiler {
                    self.profiler.draw_profile(&mut self.device,
                                               &frame.profile_counters,
                                               &self.backend_profile_counters,
                                               &self.profile_counters,
                                               &mut profile_timers,
                                               &mut self.debug);
                }

                self.profile_counters.reset();
                self.profile_counters.frame_counter.inc();

                let debug_size = DeviceUintSize::new(framebuffer_size.width as u32,
                                                     framebuffer_size.height as u32);
                self.debug.render(&mut self.device, &debug_size);
                self.device.end_frame();
                self.last_time = current_time;
            }

            // Restore frame - avoid borrow checker!
            self.current_frame = Some(frame);
        }
    }

    pub fn layers_are_bouncing_back(&self) -> bool {
        match self.current_frame {
            None => false,
            Some(ref current_frame) => !current_frame.layers_bouncing_back.is_empty(),
        }
    }

/*
    fn update_shaders(&mut self) {
        let update_uniforms = !self.pending_shader_updates.is_empty();

        for path in self.pending_shader_updates.drain(..) {
            panic!("todo");
            //self.device.refresh_shader(path);
        }

        if update_uniforms {
            self.update_uniform_locations();
        }
    }
*/

    fn update_texture_cache(&mut self) {
        let _gm = GpuMarker::new(self.device.rc_gl(), "texture cache update");
        let mut pending_texture_updates = mem::replace(&mut self.pending_texture_updates, vec![]);
        for update_list in pending_texture_updates.drain(..) {
            for update in update_list.updates {
                match update.op {
                    TextureUpdateOp::Create { width, height, format, filter, mode, data } => {
                        let CacheTextureId(cache_texture_index) = update.id;
                        if self.cache_texture_id_map.len() == cache_texture_index {
                            // Create a new native texture, as requested by the texture cache.
                            let texture_id = self.device
                                                 .create_texture_ids(1, TextureTarget::Default)[0];
                            self.cache_texture_id_map.push(texture_id);
                        }
                        let texture_id = self.cache_texture_id_map[cache_texture_index];

                        if let Some(image) = data {
                            match image {
                                ImageData::Raw(raw) => {
                                    self.device.init_texture(texture_id,
                                                             width,
                                                             height,
                                                             format,
                                                             filter,
                                                             mode,
                                                             Some(raw.as_slice()));
                                }
                                ImageData::External(ext_image) => {
                                    match ext_image.image_type {
                                        ExternalImageType::ExternalBuffer => {
                                            let handler = self.external_image_handler
                                                              .as_mut()
                                                              .expect("Found external image, but no handler set!");

                                            match handler.lock(ext_image.id).source {
                                                ExternalImageSource::RawData(raw) => {
                                                    self.device.init_texture(texture_id,
                                                                             width,
                                                                             height,
                                                                             format,
                                                                             filter,
                                                                             mode,
                                                                             Some(raw));
                                                }
                                                _ => panic!("No external buffer found"),
                                            };
                                            handler.unlock(ext_image.id);
                                        }
                                        _ => {
                                            panic!("External texture handle should not use TextureUpdateOp::Create.");
                                        }
                                    }
                                }
                                _ => {
                                    panic!("No suitable image buffer for TextureUpdateOp::Create.");
                                }
                            }
                        } else {
                            self.device.init_texture(texture_id,
                                                     width,
                                                     height,
                                                     format,
                                                     filter,
                                                     mode,
                                                     None);
                        }
                    }
                    TextureUpdateOp::Grow { width, height, format, filter, mode } => {
                        let texture_id = self.cache_texture_id_map[update.id.0];
                        self.device.resize_texture(texture_id,
                                                   width,
                                                   height,
                                                   format,
                                                   filter,
                                                   mode);
                    }
                    TextureUpdateOp::Update { page_pos_x, page_pos_y, width, height, data, stride, offset } => {
                        let texture_id = self.cache_texture_id_map[update.id.0];
                        self.device.update_texture(texture_id,
                                                   page_pos_x,
                                                   page_pos_y,
                                                   width, height, stride,
                                                   &data[offset as usize..]);
                    }
                    TextureUpdateOp::UpdateForExternalBuffer { rect, id, stride } => {
                        let handler = self.external_image_handler
                                          .as_mut()
                                          .expect("Found external image, but no handler set!");
                        let device = &mut self.device;
                        let cached_id = self.cache_texture_id_map[update.id.0];

                        match handler.lock(id).source {
                            ExternalImageSource::RawData(data) => {
                                device.update_texture(cached_id,
                                                      rect.origin.x,
                                                      rect.origin.y,
                                                      rect.size.width,
                                                      rect.size.height,
                                                      stride, data);
                            }
                            _ => panic!("No external buffer found"),
                        };
                        handler.unlock(id);
                    }
                    TextureUpdateOp::Free => {
                        let texture_id = self.cache_texture_id_map[update.id.0];
                        self.device.deinit_texture(texture_id);
                    }
                }
            }
        }
    }

    fn draw_instanced_batch<T>(&mut self,
                               data: &[T],
                               vao: VAOId,
                               shader: ProgramId,
                               textures: &BatchTextures,
                               projection: &Matrix4D<f32>) {
        self.device.bind_vao(vao);
        self.device.bind_program(shader, projection);

        for i in 0..textures.colors.len() {
            let texture_id = self.resolve_source_texture(&textures.colors[i]);
            self.device.bind_texture(TextureSampler::color(i), texture_id);
        }

        // TODO: this probably isn't the best place for this.
        self.device.bind_texture(TextureSampler::Dither, self.dither_matrix_texture_id);

        self.device.update_vao_instances(vao, data, VertexUsageHint::Stream);
        self.device.draw_indexed_triangles_instanced_u16(6, data.len() as i32);
        self.profile_counters.vertices.add(6 * data.len());
        self.profile_counters.draw_calls.inc();
    }

    fn submit_batch(&mut self,
                    batch: &PrimitiveBatch,
                    projection: &Matrix4D<f32>,
                    render_task_data: &Vec<RenderTaskData>,
                    cache_texture: TextureId,
                    render_target: Option<(TextureId, i32)>,
                    target_dimensions: DeviceUintSize) {
        let transform_kind = batch.key.flags.transform_kind();
        let needs_clipping = batch.key.flags.needs_clipping();
        debug_assert!(!needs_clipping ||
                      batch.key.blend_mode == BlendMode::Alpha ||
                      batch.key.blend_mode == BlendMode::PremultipliedAlpha);

        match batch.data {
            PrimitiveBatchData::Instances(ref data) => {
                let (marker, shader) = match batch.key.kind {
                    AlphaBatchKind::Composite => unreachable!(),
                    AlphaBatchKind::HardwareComposite => {
                        let shader = self.ps_hw_composite.get(&mut self.device);
                        (GPU_TAG_PRIM_HW_COMPOSITE, shader)
                    }
                    AlphaBatchKind::Blend => {
                        let shader = self.ps_blend.get(&mut self.device);
                        (GPU_TAG_PRIM_BLEND, shader)
                    }
                    AlphaBatchKind::Rectangle => {
                        let shader = if needs_clipping {
                            self.ps_rectangle_clip.get(&mut self.device, transform_kind)
                        } else {
                            self.ps_rectangle.get(&mut self.device, transform_kind)
                        };
                        (GPU_TAG_PRIM_RECT, shader)
                    }
                    AlphaBatchKind::TextRun => {
                        let shader = match batch.key.blend_mode {
                            BlendMode::Subpixel(..) => self.ps_text_run_subpixel.get(&mut self.device, transform_kind),
                            BlendMode::Alpha | BlendMode::PremultipliedAlpha | BlendMode::None => self.ps_text_run.get(&mut self.device, transform_kind),
                        };
                        (GPU_TAG_PRIM_TEXT_RUN, shader)
                    }
                    AlphaBatchKind::Image => {
                        let shader = self.ps_image.get(&mut self.device, transform_kind);
                        (GPU_TAG_PRIM_IMAGE, shader)
                    }
                    AlphaBatchKind::ImageRect => {
                        let shader = self.ps_image_rect.get(&mut self.device, transform_kind);
                        (GPU_TAG_PRIM_IMAGE_RECT, shader)
                    }
                    AlphaBatchKind::YuvImage => {
                        let shader = self.ps_yuv_image.get(&mut self.device, transform_kind);
                        (GPU_TAG_PRIM_YUV_IMAGE, shader)
                    }
                    AlphaBatchKind::Border => {
                        let shader = self.ps_border.get(&mut self.device, transform_kind);
                        (GPU_TAG_PRIM_BORDER, shader)
                    }
                    AlphaBatchKind::AlignedGradient => {
                        let shader = self.ps_gradient.get(&mut self.device, transform_kind);
                        (GPU_TAG_PRIM_GRADIENT, shader)
                    }
                    AlphaBatchKind::AngleGradient => {
                        let shader = self.ps_angle_gradient.get(&mut self.device, transform_kind);
                        (GPU_TAG_PRIM_ANGLE_GRADIENT, shader)
                    }
                    AlphaBatchKind::RadialGradient => {
                        let shader = self.ps_radial_gradient.get(&mut self.device, transform_kind);
                        (GPU_TAG_PRIM_RADIAL_GRADIENT, shader)
                    }
                    AlphaBatchKind::BoxShadow => {
                        let shader = self.ps_box_shadow.get(&mut self.device, transform_kind);
                        (GPU_TAG_PRIM_BOX_SHADOW, shader)
                    }
                    AlphaBatchKind::CacheImage => {
                        let shader = self.ps_cache_image.get(&mut self.device, transform_kind);
                        (GPU_TAG_PRIM_CACHE_IMAGE, shader)
                    }
                };

                let shader = shader.unwrap();

                let _gm = self.gpu_profile.add_marker(marker);
                let vao = self.prim_vao_id;
                self.draw_instanced_batch(data,
                                          vao,
                                          shader,
                                          &batch.key.textures,
                                          projection);
            }
            PrimitiveBatchData::Composite(ref instance) => {
                let _gm = self.gpu_profile.add_marker(GPU_TAG_PRIM_COMPOSITE);
                let vao = self.prim_vao_id;
                let shader = self.ps_composite.get(&mut self.device).unwrap();

                // TODO(gw): This code branch is all a bit hacky. We rely
                // on pulling specific values from the render target data
                // and also cloning the single primitive instance to be
                // able to pass to draw_instanced_batch(). We should
                // think about a cleaner way to achieve this!

                // Before submitting the composite batch, do the
                // framebuffer readbacks that are needed for each
                // composite operation in this batch.
                let cache_texture_dimensions = self.device.get_texture_dimensions(cache_texture);

                let backdrop = &render_task_data[instance.task_index as usize];
                let readback = &render_task_data[instance.user_data[0] as usize];
                let source = &render_task_data[instance.user_data[1] as usize];

                // Bind the FBO to blit the backdrop to.
                // Called per-instance in case the layer (and therefore FBO)
                // changes. The device will skip the GL call if the requested
                // target is already bound.
                let cache_draw_target = (cache_texture, readback.data[4] as i32);
                self.device.bind_draw_target(Some(cache_draw_target), Some(cache_texture_dimensions));

                let src_x = backdrop.data[0] - backdrop.data[4] + source.data[4];
                let src_y = backdrop.data[1] - backdrop.data[5] + source.data[5];

                let dest_x = readback.data[0];
                let dest_y = readback.data[1];

                let width = readback.data[2];
                let height = readback.data[3];

                let mut src = DeviceIntRect::new(DeviceIntPoint::new(src_x as i32, src_y as i32),
                                                 DeviceIntSize::new(width as i32, height as i32));
                let mut dest = DeviceIntRect::new(DeviceIntPoint::new(dest_x as i32, dest_y as i32),
                                                  DeviceIntSize::new(width as i32, height as i32));

                // Need to invert the y coordinates and flip the image vertically when
                // reading back from the framebuffer.
                if render_target.is_none() {
                    src.origin.y = target_dimensions.height as i32 - src.size.height - src.origin.y;
                    dest.origin.y += dest.size.height;
                    dest.size.height = -dest.size.height;
                }

                self.device.blit_render_target(render_target,
                                               Some(src),
                                               dest);

                // Restore draw target to current pass render target + layer.
                self.device.bind_draw_target(render_target, Some(target_dimensions));

                self.draw_instanced_batch(&[instance.clone()],
                                          vao,
                                          shader,
                                          &batch.key.textures,
                                          projection);
            }
        }
    }

    fn draw_color_target(&mut self,
                         render_target: Option<(TextureId, i32)>,
                         target: &ColorRenderTarget,
                         target_size: DeviceUintSize,
                         color_cache_texture: TextureId,
                         clear_color: Option<[f32; 4]>,
                         render_task_data: &Vec<RenderTaskData>,
                         projection: &Matrix4D<f32>) {
        {
            let _gm = self.gpu_profile.add_marker(GPU_TAG_SETUP_TARGET);
            self.device.bind_draw_target(render_target, Some(target_size));
            self.device.disable_depth();
            self.device.enable_depth_write();
            self.device.set_blend(false);
            self.device.set_blend_mode_alpha();
            match render_target {
                Some(..) => {
                    // TODO(gw): Applying a scissor rect and minimal clear here
                    // is a very large performance win on the Intel and nVidia
                    // GPUs that I have tested with. It's possible it may be a
                    // performance penalty on other GPU types - we should test this
                    // and consider different code paths.
                    self.device.clear_target_rect(clear_color,
                                                  Some(1.0),
                                                  target.used_rect());
                }
                None => {
                    self.device.clear_target(clear_color, Some(1.0));
                }
            }

            let isolate_clear_color = Some([0.0, 0.0, 0.0, 0.0]);
            for isolate_clear in &target.isolate_clears {
                self.device.clear_target_rect(isolate_clear_color,
                                              None,
                                              *isolate_clear);
            }

            self.device.disable_depth_write();
        }

        // Draw any blurs for this target.
        // Blurs are rendered as a standard 2-pass
        // separable implementation.
        // TODO(gw): In the future, consider having
        //           fast path blur shaders for common
        //           blur radii with fixed weights.
        if !target.vertical_blurs.is_empty() || !target.horizontal_blurs.is_empty() {
            let _gm = self.gpu_profile.add_marker(GPU_TAG_BLUR);
            let vao = self.blur_vao_id;

            self.device.set_blend(false);
            let shader = self.cs_blur.get(&mut self.device).unwrap();

            self.draw_instanced_batch(&target.vertical_blurs,
                                      vao,
                                      shader,
                                      &BatchTextures::no_texture(),
                                      &projection);
            self.draw_instanced_batch(&target.horizontal_blurs,
                                      vao,
                                      shader,
                                      &BatchTextures::no_texture(),
                                      &projection);
        }

        // Draw any box-shadow caches for this target.
        if !target.box_shadow_cache_prims.is_empty() {
            self.device.set_blend(false);
            let _gm = self.gpu_profile.add_marker(GPU_TAG_CACHE_BOX_SHADOW);
            let vao = self.prim_vao_id;
            let shader = self.cs_box_shadow.get(&mut self.device).unwrap();
            self.draw_instanced_batch(&target.box_shadow_cache_prims,
                                      vao,
                                      shader,
                                      &BatchTextures::no_texture(),
                                      &projection);
        }

        // Draw any textrun caches for this target. For now, this
        // is only used to cache text runs that are to be blurred
        // for text-shadow support. In the future it may be worth
        // considering using this for (some) other text runs, since
        // it removes the overhead of submitting many small glyphs
        // to multiple tiles in the normal text run case.
        if !target.text_run_cache_prims.is_empty() {
            self.device.set_blend(true);
            self.device.set_blend_mode_alpha();

            let _gm = self.gpu_profile.add_marker(GPU_TAG_CACHE_TEXT_RUN);
            let vao = self.prim_vao_id;
            let shader = self.cs_text_run.get(&mut self.device).unwrap();

            self.draw_instanced_batch(&target.text_run_cache_prims,
                                      vao,
                                      shader,
                                      &target.text_run_textures,
                                      &projection);
        }

        let _gm2 = GpuMarker::new(self.device.rc_gl(), "alpha batches");
        self.device.set_blend(false);
        let mut prev_blend_mode = BlendMode::None;

        self.device.set_depth_func(DepthFunction::Less);
        self.device.enable_depth();
        self.device.enable_depth_write();

        for batch in &target.alpha_batcher.opaque_batches {
            self.submit_batch(batch,
                              &projection,
                              render_task_data,
                              color_cache_texture,
                              render_target,
                              target_size);
        }

        self.device.disable_depth_write();

        for batch in &target.alpha_batcher.alpha_batches {
            if batch.key.blend_mode != prev_blend_mode {
                match batch.key.blend_mode {
                    BlendMode::None => {
                        self.device.set_blend(false);
                    }
                    BlendMode::Alpha => {
                        self.device.set_blend(true);
                        self.device.set_blend_mode_alpha();
                    }
                    BlendMode::PremultipliedAlpha => {
                        self.device.set_blend(true);
                        self.device.set_blend_mode_premultiplied_alpha();
                    }
                    BlendMode::Subpixel(color) => {
                        self.device.set_blend(true);
                        self.device.set_blend_mode_subpixel(color);
                    }
                }
                prev_blend_mode = batch.key.blend_mode;
            }

            self.submit_batch(batch,
                              &projection,
                              render_task_data,
                              color_cache_texture,
                              render_target,
                              target_size);
        }

        self.device.disable_depth();
        self.device.set_blend(false);
    }

    fn draw_alpha_target(&mut self,
                         render_target: (TextureId, i32),
                         target: &AlphaRenderTarget,
                         target_size: DeviceUintSize,
                         projection: &Matrix4D<f32>) {
        {
            let _gm = self.gpu_profile.add_marker(GPU_TAG_SETUP_TARGET);
            self.device.bind_draw_target(Some(render_target), Some(target_size));
            self.device.disable_depth();
            self.device.disable_depth_write();

            // TODO(gw): Applying a scissor rect and minimal clear here
            // is a very large performance win on the Intel and nVidia
            // GPUs that I have tested with. It's possible it may be a
            // performance penalty on other GPU types - we should test this
            // and consider different code paths.
            let clear_color = [1.0, 1.0, 1.0, 0.0];
            self.device.clear_target_rect(Some(clear_color),
                                          None,
                                          target.used_rect());
        }

        // Draw the clip items into the tiled alpha mask.
        {
            let _gm = self.gpu_profile.add_marker(GPU_TAG_CACHE_CLIP);
            let vao = self.clip_vao_id;
            // switch to multiplicative blending
            self.device.set_blend(true);
            self.device.set_blend_mode_multiply();

            // draw rounded cornered rectangles
            if !target.clip_batcher.rectangles.is_empty() {
                let _gm2 = GpuMarker::new(self.device.rc_gl(), "clip rectangles");
                let shader = self.cs_clip_rectangle.get(&mut self.device).unwrap();
                self.draw_instanced_batch(&target.clip_batcher.rectangles,
                                          vao,
                                          shader,
                                          &BatchTextures::no_texture(),
                                          &projection);
            }
            // draw image masks
            for (mask_texture_id, items) in target.clip_batcher.images.iter() {
                let _gm2 = GpuMarker::new(self.device.rc_gl(), "clip images");
                let texture_id = self.resolve_source_texture(mask_texture_id);
                self.device.bind_texture(TextureSampler::Mask, texture_id);
                let shader = self.cs_clip_image.get(&mut self.device).unwrap();
                self.draw_instanced_batch(items,
                                          vao,
                                          shader,
                                          &BatchTextures::no_texture(),
                                          &projection);
            }
        }
    }

    fn update_deferred_resolves(&mut self, frame: &mut Frame) {
        // The first thing we do is run through any pending deferred
        // resolves, and use a callback to get the UV rect for this
        // custom item. Then we patch the resource_rects structure
        // here before it's uploaded to the GPU.
        if !frame.deferred_resolves.is_empty() {
            let handler = self.external_image_handler
                              .as_mut()
                              .expect("Found external image, but no handler set!");

            for deferred_resolve in &frame.deferred_resolves {
                GpuMarker::fire(self.device.gl(), "deferred resolve");
                let props = &deferred_resolve.image_properties;
                let ext_image = props.external_image
                                     .expect("BUG: Deferred resolves must be external images!");
                let image = handler.lock(ext_image.id);
                let texture_target = match ext_image.image_type {
                    ExternalImageType::Texture2DHandle => TextureTarget::Default,
                    ExternalImageType::TextureRectHandle => TextureTarget::Rect,
                    _ => {
                        panic!("{:?} is not a suitable image type in update_deferred_resolves().",
                            ext_image.image_type);
                    }
                };

                let texture_id = match image.source {
                    ExternalImageSource::NativeTexture(texture_id) => TextureId::new(texture_id, texture_target),
                    _ => panic!("No native texture found."),
                };

                self.external_images.insert(ext_image.id, texture_id);
                let resource_rect_index = deferred_resolve.resource_address.0 as usize;
                let resource_rect = &mut frame.gpu_resource_rects[resource_rect_index];
                resource_rect.uv0 = DevicePoint::new(image.u0, image.v0);
                resource_rect.uv1 = DevicePoint::new(image.u1, image.v1);
            }
        }
    }

    fn unlock_external_images(&mut self) {
        if !self.external_images.is_empty() {
            let handler = self.external_image_handler
                              .as_mut()
                              .expect("Found external image, but no handler set!");

            for (external_id, _) in self.external_images.drain() {
                handler.unlock(external_id);
            }
        }
    }

    fn release_external_images(&mut self, mut pending_external_image_updates: ExternalImageUpdateList) {
        if !pending_external_image_updates.is_empty() {
            let handler = self.external_image_handler
                              .as_mut()
                              .expect("found external image updates, but no handler set!");

            for external_id in pending_external_image_updates.drain(..) {
                handler.release(external_id);
            }
        }
    }

    fn draw_tile_frame(&mut self,
                       frame: &mut Frame,
                       framebuffer_size: &DeviceUintSize) {
        let _gm = GpuMarker::new(self.device.rc_gl(), "tile frame draw");
        self.update_deferred_resolves(frame);

        // Some tests use a restricted viewport smaller than the main screen size.
        // Ensure we clear the framebuffer in these tests.
        // TODO(gw): Find a better solution for this?
        let needs_clear = frame.window_size.width < framebuffer_size.width ||
                          frame.window_size.height < framebuffer_size.height;

        self.device.disable_depth_write();
        self.device.disable_stencil();
        self.device.set_blend(false);

        if frame.passes.is_empty() {
            self.device.clear_target(Some(self.clear_color.to_array()), Some(1.0));
        } else {
            // Assign render targets to the passes.
            for pass in &mut frame.passes {
                debug_assert!(pass.color_texture_id.is_none());
                debug_assert!(pass.alpha_texture_id.is_none());

                if pass.needs_render_target_kind(RenderTargetKind::Color) {
                    pass.color_texture_id = Some(self.color_render_targets
                                                     .pop()
                                                     .unwrap_or_else(|| {
                                                         self.device
                                                             .create_texture_ids(1, TextureTarget::Array)[0]
                                                      }));
                }

                if pass.needs_render_target_kind(RenderTargetKind::Alpha) {
                    pass.alpha_texture_id = Some(self.alpha_render_targets
                                                     .pop()
                                                     .unwrap_or_else(|| {
                                                         self.device
                                                             .create_texture_ids(1, TextureTarget::Array)[0]
                                                      }));
                }
            }

            // Init textures and render targets to match this scene.
            for pass in &frame.passes {
                if let Some(texture_id) = pass.color_texture_id {
                    let target_count = pass.required_target_count(RenderTargetKind::Color);
                    self.device.init_texture(texture_id,
                                             frame.cache_size.width as u32,
                                             frame.cache_size.height as u32,
                                             ImageFormat::RGBA8,
                                             TextureFilter::Linear,
                                             RenderTargetMode::LayerRenderTarget(target_count as i32),
                                             None);
                }
                if let Some(texture_id) = pass.alpha_texture_id {
                    let target_count = pass.required_target_count(RenderTargetKind::Alpha);
                    self.device.init_texture(texture_id,
                                             frame.cache_size.width as u32,
                                             frame.cache_size.height as u32,
                                             ImageFormat::A8,
                                             TextureFilter::Nearest,
                                             RenderTargetMode::LayerRenderTarget(target_count as i32),
                                             None);
                }
            }

            // TODO(gw): This is a hack / workaround for #728.
            // We should find a better way to implement these updates rather
            // than wasting this extra memory, but for now it removes a large
            // number of driver stalls.
            self.gpu_data_textures[self.gdt_index].init_frame(&mut self.device, frame);
            self.gdt_index = (self.gdt_index + 1) % GPU_DATA_TEXTURE_POOL;

            let mut src_color_id = self.dummy_cache_texture_id;
            let mut src_alpha_id = self.dummy_cache_texture_id;

            for pass in &mut frame.passes {
                let size;
                let clear_color;
                let projection;

                if pass.is_framebuffer {
                    clear_color = if self.clear_framebuffer || needs_clear {
                        Some(frame.background_color.map_or(self.clear_color.to_array(), |color| {
                            color.to_array()
                        }))
                    } else {
                        None
                    };
                    size = framebuffer_size;
                    projection = Matrix4D::ortho(0.0,
                                                 size.width as f32,
                                                 size.height as f32,
                                                 0.0,
                                                 ORTHO_NEAR_PLANE,
                                                 ORTHO_FAR_PLANE)
                } else {
                    size = &frame.cache_size;
                    clear_color = Some([1.0, 1.0, 1.0, 0.0]);
                    projection = Matrix4D::ortho(0.0,
                                                 size.width as f32,
                                                 0.0,
                                                 size.height as f32,
                                                 ORTHO_NEAR_PLANE,
                                                 ORTHO_FAR_PLANE);
                }

                self.device.bind_texture(TextureSampler::CacheA8, src_alpha_id);
                self.device.bind_texture(TextureSampler::CacheRGBA8, src_color_id);

                for (target_index, target) in pass.alpha_targets.targets.iter().enumerate() {
                    self.draw_alpha_target((pass.alpha_texture_id.unwrap(), target_index as i32),
                                           target,
                                           *size,
                                           &projection);
                }

                for (target_index, target) in pass.color_targets.targets.iter().enumerate() {
                    let render_target = pass.color_texture_id.map(|texture_id| {
                        (texture_id, target_index as i32)
                    });
                    self.draw_color_target(render_target,
                                           target,
                                           *size,
                                           src_color_id,
                                           clear_color,
                                           &frame.render_task_data,
                                           &projection);

                }

                src_color_id = pass.color_texture_id.unwrap_or(self.dummy_cache_texture_id);
                src_alpha_id = pass.alpha_texture_id.unwrap_or(self.dummy_cache_texture_id);

                // Return the texture IDs to the pool for next frame.
                if let Some(texture_id) = pass.color_texture_id.take() {
                    self.color_render_targets.push(texture_id);
                }
                if let Some(texture_id) = pass.alpha_texture_id.take() {
                    self.alpha_render_targets.push(texture_id);
                }
            }

            self.color_render_targets.reverse();
            self.alpha_render_targets.reverse();
            self.draw_render_target_debug(framebuffer_size);
        }

        self.unlock_external_images();
    }

    pub fn debug_renderer<'a>(&'a mut self) -> &'a mut DebugRenderer {
        &mut self.debug
    }

    pub fn get_profiler_enabled(&mut self) -> bool {
        self.enable_profiler
    }

    pub fn set_profiler_enabled(&mut self, enabled: bool) {
        self.enable_profiler = enabled;
    }

    pub fn save_cpu_profile(&self, filename: &str) {
        write_profile(filename);
    }

    fn draw_render_target_debug(&mut self,
                                framebuffer_size: &DeviceUintSize) {
        if self.render_target_debug {
            // TODO(gw): Make the layout of the render targets a bit more sophisticated.
            // Right now, it just draws them in one row at the bottom of the screen,
            // with a fixed size.
            let rt_debug_x0 = 16;
            let rt_debug_y0 = 16;
            let rt_debug_spacing = 16;
            let rt_debug_size = 512;
            let mut current_target = 0;

            for texture_id in self.color_render_targets.iter().chain(self.alpha_render_targets.iter()) {
                let layer_count = self.device.get_render_target_layer_count(*texture_id);
                for layer_index in 0..layer_count {
                    let x0 = rt_debug_x0 + (rt_debug_spacing + rt_debug_size) * current_target;
                    let y0 = rt_debug_y0;

                    // If we have more targets than fit on one row in screen, just early exit.
                    if x0 > framebuffer_size.width as i32 {
                        return;
                    }

                    let dest_rect = DeviceIntRect::new(DeviceIntPoint::new(x0, y0),
                                                       DeviceIntSize::new(rt_debug_size, rt_debug_size));
                    self.device.blit_render_target(Some((*texture_id, layer_index as i32)),
                                                   None,
                                                   dest_rect);

                    current_target += 1;
                }
            }
        }
    }

    // De-initialize the Renderer safely, assuming the GL is still alive and active.
    pub fn deinit(mut self) {
        //Note: this is a fake frame, only needed because texture deletion is require to happen inside a frame
        self.device.begin_frame(1.0);
        self.device.deinit_texture(self.dummy_cache_texture_id);
        self.device.end_frame();
    }
}

pub enum ExternalImageSource<'a> {
    RawData(&'a [u8]),      // raw buffers.
    NativeTexture(u32),     // Is a gl::GLuint texture handle
}

/// The data that an external client should provide about
/// an external image. The timestamp is used to test if
/// the renderer should upload new texture data this
/// frame. For instance, if providing video frames, the
/// application could call wr.render() whenever a new
/// video frame is ready. If the callback increments
/// the returned timestamp for a given image, the renderer
/// will know to re-upload the image data to the GPU.
/// Note that the UV coords are supplied in texel-space!
pub struct ExternalImage<'a> {
    pub u0: f32,
    pub v0: f32,
    pub u1: f32,
    pub v1: f32,
    pub source: ExternalImageSource<'a>,
}

/// The interfaces that an application can implement to support providing
/// external image buffers.
/// When the the application passes an external image to WR, it should kepp that
/// external image life time untile the release() call.
pub trait ExternalImageHandler {
    /// Lock the external image. Then, WR could start to read the image content.
    /// The WR client should not change the image content until the unlock()
    /// call.
    fn lock(&mut self, key: ExternalImageId) -> ExternalImage;
    /// Unlock the external image. The WR should not read the image content
    /// after this call.
    fn unlock(&mut self, key: ExternalImageId);
    /// Tell the WR client that it could start to release this external image.
    fn release(&mut self, key: ExternalImageId);
}

pub struct RendererOptions {
    pub device_pixel_ratio: f32,
    pub resource_override_path: Option<PathBuf>,
    pub enable_aa: bool,
    pub enable_profiler: bool,
    pub max_recorded_profiles: usize,
    pub debug: bool,
    pub enable_scrollbars: bool,
    pub precache_shaders: bool,
    pub renderer_kind: RendererKind,
    pub enable_subpixel_aa: bool,
    pub clear_framebuffer: bool,
    pub clear_color: ColorF,
    pub render_target_debug: bool,
    pub max_texture_size: Option<u32>,
    pub workers: Option<Arc<Mutex<ThreadPool>>>,
    pub blob_image_renderer: Option<Box<BlobImageRenderer>>,
    pub recorder: Option<Box<ApiRecordingReceiver>>,
}

impl Default for RendererOptions {
    fn default() -> RendererOptions {
        RendererOptions {
            device_pixel_ratio: 1.0,
            resource_override_path: None,
            enable_aa: true,
            enable_profiler: false,
            max_recorded_profiles: 0,
            debug: false,
            enable_scrollbars: false,
            precache_shaders: false,
            renderer_kind: RendererKind::Native,
            enable_subpixel_aa: false,
            clear_framebuffer: true,
            clear_color: ColorF::new(1.0, 1.0, 1.0, 1.0),
            render_target_debug: false,
            max_texture_size: None,
            workers: None,
            blob_image_renderer: None,
            recorder: None,
        }
    }
}

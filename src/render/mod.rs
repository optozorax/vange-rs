use crate::{
    config::settings,
    level, model,
    space::{Camera, Transform},
};

use bytemuck::{Pod, Zeroable};
use glsl_to_spirv;
use wgpu::util::DeviceExt as _;

use std::{
    collections::HashMap,
    fs::File,
    io::{BufReader, Error as IoError, Read, Write},
    mem,
    path::PathBuf,
    sync::Arc,
};

pub mod body;
pub mod collision;
pub mod debug;
pub mod global;
pub mod mipmap;
pub mod object;
mod shadow;
pub mod terrain;

pub use shadow::FORMAT as SHADOW_FORMAT;
pub const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;
pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

pub struct GpuTransform {
    pub pos_scale: [f32; 4],
    pub orientation: [f32; 4],
}

impl GpuTransform {
    pub fn new(t: &Transform) -> Self {
        GpuTransform {
            pos_scale: [t.disp.x, t.disp.y, t.disp.z, t.scale],
            orientation: [t.rot.v.x, t.rot.v.y, t.rot.v.z, t.rot.s],
        }
    }
}

pub struct ScreenTargets<'a> {
    pub extent: wgpu::Extent3d,
    pub color: &'a wgpu::TextureView,
    pub depth: &'a wgpu::TextureView,
}

pub struct SurfaceData {
    pub constants: wgpu::Buffer,
    pub height: (wgpu::TextureView, wgpu::Sampler),
    pub meta: (wgpu::TextureView, wgpu::Sampler),
}

pub type ShapeVertex = [f32; 4];

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ShapePolygon {
    pub indices: [u16; 4],
    pub normal: [i8; 4],
    pub origin_square: [f32; 4],
}
unsafe impl Pod for ShapePolygon {}
unsafe impl Zeroable for ShapePolygon {}

pub struct ShapeVertexDesc {
    attributes: [wgpu::VertexAttributeDescriptor; 3],
}

impl ShapeVertexDesc {
    pub fn new() -> Self {
        ShapeVertexDesc {
            attributes: wgpu::vertex_attr_array![0 => Ushort4, 1 => Char4Norm, 2 => Float4],
        }
    }

    pub fn buffer_desc(&self) -> wgpu::VertexBufferDescriptor {
        wgpu::VertexBufferDescriptor {
            stride: mem::size_of::<ShapePolygon>() as wgpu::BufferAddress,
            step_mode: wgpu::InputStepMode::Instance,
            attributes: &self.attributes,
        }
    }
}

pub struct Shaders {
    vs: wgpu::ShaderModule,
    fs: wgpu::ShaderModule,
}

impl Shaders {
    fn fail(name: &str, source: &str, log: &str) -> ! {
        println!("Generated shader:");
        for (i, line) in source.lines().enumerate() {
            println!("{:3}| {}", i + 1, line);
        }
        let msg = log.replace("\\n", "\n");
        panic!("\nUnable to compile '{}': {}", name, msg);
    }

    pub fn new(
        name: &str,
        specialization: &[&str],
        device: &wgpu::Device,
    ) -> Result<Self, IoError> {
        let base_path = PathBuf::from("res").join("shader");
        let path = base_path.join(name).with_extension("glsl");
        if !path.is_file() {
            panic!("Shader not found: {:?}", path);
        }

        let mut buf_vs = b"#version 450\n#define SHADER_VS\n".to_vec();
        let mut buf_fs = b"#version 450\n#define SHADER_FS\n".to_vec();

        let mut code = String::new();
        BufReader::new(File::open(&path)?).read_to_string(&mut code)?;
        // parse meta-data
        {
            let mut lines = code.lines();
            let first = lines.next().unwrap();
            if first.starts_with("//!include") {
                for include_pair in first.split_whitespace().skip(1) {
                    let mut temp = include_pair.split(':');
                    let target = match temp.next().unwrap() {
                        "vs" => &mut buf_vs,
                        "fs" => &mut buf_fs,
                        other => panic!("Unknown target: {}", other),
                    };
                    let include = temp.next().unwrap();
                    let inc_path = base_path.join(include).with_extension("inc.glsl");
                    match File::open(&inc_path) {
                        Ok(include) => BufReader::new(include).read_to_end(target)?,
                        Err(e) => panic!("Unable to include {:?}: {:?}", inc_path, e),
                    };
                }
            }
            let second = lines.next().unwrap();
            if second.starts_with("//!specialization") {
                for define in second.split_whitespace().skip(1) {
                    let value = if specialization.contains(&define) {
                        1
                    } else {
                        0
                    };
                    write!(buf_vs, "#define {} {}\n", define, value)?;
                    write!(buf_fs, "#define {} {}\n", define, value)?;
                }
            }
        }

        write!(
            buf_vs,
            "\n{}",
            code.replace("attribute", "in").replace("varying", "out")
        )?;
        write!(buf_fs, "\n{}", code.replace("varying", "in"))?;

        let str_vs = String::from_utf8_lossy(&buf_vs);
        let str_fs = String::from_utf8_lossy(&buf_fs);
        debug!("vs:\n{}", str_vs);
        debug!("fs:\n{}", str_fs);

        let (mut spv_vs, mut spv_fs) = (Vec::new(), Vec::new());
        match glsl_to_spirv::compile(&str_vs, glsl_to_spirv::ShaderType::Vertex) {
            Ok(mut file) => file.read_to_end(&mut spv_vs).unwrap(),
            Err(ref e) => {
                Self::fail(name, &str_vs, e);
            }
        };
        match glsl_to_spirv::compile(&str_fs, glsl_to_spirv::ShaderType::Fragment) {
            Ok(mut file) => file.read_to_end(&mut spv_fs).unwrap(),
            Err(ref e) => {
                Self::fail(name, &str_fs, e);
            }
        };

        Ok(Shaders {
            vs: device.create_shader_module(wgpu::util::make_spirv(&spv_vs)),
            fs: device.create_shader_module(wgpu::util::make_spirv(&spv_fs)),
        })
    }

    pub fn new_compute(
        name: &str,
        group_size: [u32; 3],
        specialization: &[&str],
        device: &wgpu::Device,
    ) -> Result<wgpu::ShaderModule, IoError> {
        let base_path = PathBuf::from("res").join("shader");
        let path = base_path.join(name).with_extension("glsl");
        if !path.is_file() {
            panic!("Shader not found: {:?}", path);
        }

        let mut buf = b"#version 450\n".to_vec();
        write!(
            buf,
            "layout(local_size_x = {}, local_size_y = {}, local_size_z = {}) in;\n",
            group_size[0], group_size[1], group_size[2]
        )?;
        write!(buf, "#define SHADER_CS\n")?;

        let mut code = String::new();
        BufReader::new(File::open(&path)?).read_to_string(&mut code)?;
        // parse meta-data
        {
            let mut lines = code.lines();
            let first = lines.next().unwrap();
            if first.starts_with("//!include") {
                for include_pair in first.split_whitespace().skip(1) {
                    let mut temp = include_pair.split(':');
                    let target = match temp.next().unwrap() {
                        "cs" => &mut buf,
                        other => panic!("Unknown target: {}", other),
                    };
                    let include = temp.next().unwrap();
                    let inc_path = base_path.join(include).with_extension("inc.glsl");
                    BufReader::new(File::open(inc_path)?).read_to_end(target)?;
                }
            }
            let second = lines.next().unwrap();
            if second.starts_with("//!specialization") {
                for define in second.split_whitespace().skip(1) {
                    let value = if specialization.contains(&define) {
                        1
                    } else {
                        0
                    };
                    write!(buf, "#define {} {}\n", define, value)?;
                }
            }
        }

        write!(buf, "\n{}", code)?;
        let str_cs = String::from_utf8_lossy(&buf);
        debug!("cs:\n{}", str_cs);

        let mut spv = Vec::new();
        match glsl_to_spirv::compile(&str_cs, glsl_to_spirv::ShaderType::Compute) {
            Ok(mut file) => file.read_to_end(&mut spv).unwrap(),
            Err(ref e) => {
                Self::fail(name, &str_cs, e);
            }
        };

        Ok(device.create_shader_module(wgpu::util::make_spirv(&spv)))
    }
}

pub struct Palette {
    pub view: wgpu::TextureView,
}

impl Palette {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, data: &[[u8; 4]]) -> Self {
        let extent = wgpu::Extent3d {
            width: 0x100,
            height: 1,
            depth: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Palette"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D1,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsage::SAMPLED | wgpu::TextureUsage::COPY_DST,
        });

        queue.write_texture(
            wgpu::TextureCopyView {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
            },
            bytemuck::cast_slice(data),
            wgpu::TextureDataLayout {
                offset: 0,
                bytes_per_row: 0x100 * 4,
                rows_per_image: 0,
            },
            extent,
        );

        Palette {
            view: texture.create_view(&wgpu::TextureViewDescriptor::default()),
        }
    }
}

struct InstanceArray {
    data: Vec<object::Instance>,
    // holding the mesh alive, while the key is just a raw pointer
    mesh: Arc<model::Mesh>,
    // actual hardware buffer for this data
    buffer: Option<wgpu::Buffer>,
}

pub struct Batcher {
    instances: HashMap<*const model::Mesh, InstanceArray>,
    debug_shapes: Vec<Arc<model::Shape>>,
    debug_instances: Vec<object::Instance>,
}

impl Batcher {
    pub fn new() -> Self {
        Batcher {
            instances: HashMap::new(),
            debug_shapes: Vec::new(),
            debug_instances: Vec::new(),
        }
    }

    pub fn add_mesh(&mut self, mesh: &Arc<model::Mesh>, instance: object::Instance) {
        self.instances
            .entry(&**mesh)
            .or_insert_with(|| InstanceArray {
                data: Vec::new(),
                mesh: Arc::clone(mesh),
                buffer: None,
            })
            .data
            .push(instance);
    }

    pub fn add_model(
        &mut self,
        model: &model::VisualModel,
        base_transform: &Transform,
        debug_shape_scale: Option<f32>,
        gpu_body: &body::GpuBody,
        color: object::BodyColor,
    ) {
        use cgmath::{One as _, Rotation3 as _, Transform as _};

        // body
        self.add_mesh(
            &model.body,
            object::Instance::new(base_transform, 0.0, gpu_body, color),
        );
        if let Some(shape_scale) = debug_shape_scale {
            self.debug_shapes.push(Arc::clone(&model.shape));
            self.debug_instances.push(object::Instance::new(
                base_transform,
                shape_scale,
                gpu_body,
                color,
            ));
        }

        // wheels
        for w in model.wheels.iter() {
            if let Some(ref mesh) = w.mesh {
                let transform = base_transform.concat(&Transform {
                    disp: mesh.offset.into(),
                    rot: cgmath::Quaternion::one(),
                    scale: 1.0,
                });
                self.add_mesh(
                    mesh,
                    object::Instance::new(&transform, 0.0, gpu_body, color),
                );
            }
        }

        // slots
        for s in model.slots.iter() {
            if let Some(ref mesh) = s.mesh {
                let mut local = Transform {
                    disp: cgmath::vec3(s.pos[0] as f32, s.pos[1] as f32, s.pos[2] as f32),
                    rot: cgmath::Quaternion::from_angle_y(cgmath::Deg(s.angle as f32)),
                    scale: s.scale / base_transform.scale,
                };
                local.disp -= local.transform_vector(cgmath::Vector3::from(mesh.offset));
                let transform = base_transform.concat(&local);
                self.add_mesh(
                    mesh,
                    object::Instance::new(&transform, 0.0, gpu_body, color),
                );
            }
        }
    }

    pub fn prepare(&mut self, device: &wgpu::Device) {
        for array in self.instances.values_mut() {
            if !array.data.is_empty() {
                array.buffer = Some(
                    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("instance"),
                        contents: bytemuck::cast_slice(&array.data),
                        usage: wgpu::BufferUsage::VERTEX,
                    }),
                );
            }
        }
    }

    pub fn draw<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>) {
        for array in self.instances.values() {
            if array.data.is_empty() {
                continue;
            }
            pass.set_vertex_buffer(0, array.mesh.vertex_buf.slice(..));
            pass.set_vertex_buffer(1, array.buffer.as_ref().unwrap().slice(..));
            pass.draw(
                0..array.mesh.num_vertices as u32,
                0..array.data.len() as u32,
            );
        }
    }

    pub fn clear(&mut self) {
        for array in self.instances.values_mut() {
            array.data.clear();
            array.buffer = None;
        }
        self.debug_shapes.clear();
        self.debug_instances.clear();
    }
}

pub struct PipelineSet {
    main: wgpu::RenderPipeline,
    shadow: wgpu::RenderPipeline,
}

pub enum PipelineKind {
    Main,
    Shadow,
}

impl PipelineSet {
    pub fn select(&self, kind: PipelineKind) -> &wgpu::RenderPipeline {
        match kind {
            PipelineKind::Main => &self.main,
            PipelineKind::Shadow => &self.shadow,
        }
    }
}

pub struct Render {
    global: global::Context,
    pub object: object::Context,
    pub terrain: terrain::Context,
    pub debug: debug::Context,
    pub shadow: Option<shadow::Shadow>,
    pub light_config: settings::Light,
    pub fog_config: settings::Fog,
    screen_size: wgpu::Extent3d,
}

impl Render {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        level: &level::Level,
        object_palette: &[[u8; 4]],
        settings: &settings::Render,
        screen_size: wgpu::Extent3d,
        store_buffer: wgpu::BindingResource,
    ) -> Self {
        let shadow = if settings.light.shadow.size != 0 {
            Some(shadow::Shadow::new(&settings.light, device))
        } else {
            None
        };

        let global = global::Context::new(
            device,
            queue,
            store_buffer,
            shadow.as_ref().map(|shadow| &shadow.view),
        );
        let object = object::Context::new(device, queue, object_palette, &global);
        let terrain = terrain::Context::new(
            device,
            queue,
            level,
            &global,
            &settings.terrain,
            &settings.light.shadow.terrain,
            screen_size,
        );
        let debug = debug::Context::new(device, &settings.debug, &global, &object);

        Render {
            global,
            object,
            terrain,
            debug,
            shadow,
            light_config: settings.light.clone(),
            fog_config: settings.fog.clone(),
            screen_size,
        }
    }

    pub fn draw_world(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        batcher: &mut Batcher,
        cam: &Camera,
        targets: ScreenTargets,
        device: &wgpu::Device,
    ) {
        batcher.prepare(device);
        //TODO: common routine for draw passes
        //TODO: use `write_buffer`

        if let Some(ref mut shadow) = self.shadow {
            shadow.update_view(cam);

            let constants = global::Constants::new(&shadow.cam, &self.light_config, None);
            let global_staging = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("temp-global-shadow"),
                contents: bytemuck::bytes_of(&constants),
                usage: wgpu::BufferUsage::COPY_SRC,
            });
            encoder.copy_buffer_to_buffer(
                &global_staging,
                0,
                &self.global.uniform_buf,
                0,
                mem::size_of::<global::Constants>() as wgpu::BufferAddress,
            );

            self.terrain.prepare(
                encoder,
                device,
                &self.global,
                &self.fog_config,
                cam,
                wgpu::Extent3d {
                    width: shadow.size,
                    height: shadow.size,
                    depth: 1,
                },
            );

            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachmentDescriptor {
                    attachment: &shadow.view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: true,
                    }),
                    stencil_ops: None,
                }),
            });

            pass.set_bind_group(0, &self.global.shadow_bind_group, &[]);
            self.terrain.draw_shadow(&mut pass);

            // draw vehicle models
            pass.set_pipeline(&self.object.pipelines.shadow);
            pass.set_bind_group(1, &self.object.bind_group, &[]);
            batcher.draw(&mut pass);
        }
        // main pass
        {
            let constants = global::Constants::new(
                cam,
                &self.light_config,
                self.shadow.as_ref().map(|shadow| &shadow.cam),
            );
            let global_staging = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("temp-global"),
                contents: bytemuck::bytes_of(&constants),
                usage: wgpu::BufferUsage::COPY_SRC,
            });
            encoder.copy_buffer_to_buffer(
                &global_staging,
                0,
                &self.global.uniform_buf,
                0,
                mem::size_of::<global::Constants>() as wgpu::BufferAddress,
            );

            self.terrain.prepare(
                encoder,
                device,
                &self.global,
                &self.fog_config,
                cam,
                self.screen_size,
            );

            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[wgpu::RenderPassColorAttachmentDescriptor {
                    attachment: targets.color,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear({
                            let c = self.fog_config.color;
                            wgpu::Color {
                                r: c[0] as f64,
                                g: c[1] as f64,
                                b: c[2] as f64,
                                a: c[3] as f64,
                            }
                        }),
                        store: true,
                    },
                }],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachmentDescriptor {
                    attachment: targets.depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: true,
                    }),
                    stencil_ops: None,
                }),
            });

            pass.set_bind_group(0, &self.global.bind_group, &[]);
            self.terrain.draw(&mut pass);

            // draw vehicle models
            pass.set_pipeline(&self.object.pipelines.main);
            pass.set_bind_group(1, &self.object.bind_group, &[]);
            batcher.draw(&mut pass);
        }
    }

    pub fn reload(&mut self, device: &wgpu::Device) {
        info!("Reloading shaders");
        self.object.reload(device);
        self.terrain.reload(device);
    }

    pub fn resize(&mut self, extent: wgpu::Extent3d, device: &wgpu::Device) {
        self.terrain.resize(extent, device);
        self.screen_size = extent;
    }

    /*
    pub fn surface_data(&self) -> SurfaceData {
        SurfaceData {
            constants: self.terrain_data.suf_constants.clone(),
            height: self.terrain_data.height.clone(),
            meta: self.terrain_data.meta.clone(),
        }
    }*/

    /*
    pub fn target_color(&self) -> gfx::handle::RenderTargetView<R, ColorFormat> {
        self.terrain_data.out_color.clone()
    }*/
}

use std::{sync::Arc, time::Instant};

use anyhow::{anyhow, bail, Result};
use cgmath::{Matrix4, Point3, Vector3};
use vulkano::buffer::{BufferAccess, CpuAccessibleBuffer, DeviceLocalBuffer};
use vulkano::command_buffer::PrimaryCommandBuffer;
use vulkano::descriptor_set::{PersistentDescriptorSet, WriteDescriptorSet};
use vulkano::pipeline::layout::PipelineLayoutCreateInfo;
use vulkano::pipeline::{ComputePipeline, Pipeline, PipelineLayout};
use vulkano::{
    buffer::{BufferUsage, CpuBufferPool, TypedBufferAccess},
    command_buffer::{AutoCommandBufferBuilder, CommandBufferUsage, SubpassContents},
    descriptor_set::{
        layout::{
            DescriptorSetLayout, DescriptorSetLayoutBinding, DescriptorSetLayoutCreateInfo,
            DescriptorType,
        },
        SingleLayoutDescSetPool,
    },
    device::{
        physical::{PhysicalDevice, PhysicalDeviceType},
        Device, Queue,
    },
    format::Format,
    image::{view::ImageView, AttachmentImage, ImageUsage},
    instance::{Instance, InstanceCreateInfo},
    pipeline::{
        graphics::{
            input_assembly::{InputAssemblyState, PrimitiveTopology},
            vertex_input::BuffersDefinition,
            viewport::{Viewport, ViewportState},
        },
        GraphicsPipeline, PipelineBindPoint,
    },
    render_pass::{Framebuffer, FramebufferCreateInfo, RenderPass, Subpass},
    shader::ShaderStages,
    single_pass_renderpass,
    swapchain::{
        acquire_next_image, AcquireError, ColorSpace, Surface, Swapchain, SwapchainCreateInfo,
    },
    sync::{FlushError, GpuFuture},
};
use vulkano_win::VkSurfaceBuild;
use winit::{
    dpi::PhysicalSize,
    event::{Event, WindowEvent},
    event_loop::ControlFlow,
    window::Window,
};

mod shader;

const NUM_PARTICLES_PERAXIS: usize = 100;
const VIEW_DISTANCE: f32 = 20.0;

struct Renderer {
    device: Arc<Device>,
    queue: Arc<Queue>,
    surface: Arc<Surface<Window>>,
    color_format: Format,
    depth_format: Format,
    render_pass: Arc<RenderPass>,
    framebuffers: Framebuffers,
    inflight: Option<Box<dyn GpuFuture>>,
    compute_pipeline: Arc<ComputePipeline>,
    graphics_pipeline: Arc<GraphicsPipeline>,
    points: Arc<DeviceLocalBuffer<[shader::Point]>>,
    _velocities: Arc<DeviceLocalBuffer<[shader::Velocity]>>,
    pressures: Arc<DeviceLocalBuffer<[u32]>>,

    compute_storage_descriptors: Arc<PersistentDescriptorSet>,
    compute_uniforms: CpuBufferPool<shader::compute::ty::Uniforms>,
    compute_uniform_descriptor_pool: SingleLayoutDescSetPool,

    vertex_uniforms: CpuBufferPool<shader::vertex::ty::Uniforms>,
    vertex_uniform_descriptor_pool: SingleLayoutDescSetPool,

    matrix: cgmath::Matrix4<f32>,
    last_fps_print: Instant,
    frames: u32,
    which_pressure_buffer: bool,
}

enum Framebuffers {
    // We have not yet initalized framebuffers.
    NotCreated,

    // We have a swapchain, but it's invalid.
    Invalid {
        swapchain: Arc<Swapchain<Window>>,
    },

    // We have a valid swapchain.
    Valid {
        swapchain: Arc<Swapchain<Window>>,
        framebuffers: Vec<Arc<Framebuffer>>,
    },
}
impl Default for Framebuffers {
    fn default() -> Self {
        Self::NotCreated
    }
}
impl Framebuffers {
    fn invalidate(&mut self) {
        *self = match std::mem::take(self) {
            Framebuffers::Valid {
                swapchain,
                framebuffers: _,
            } => Framebuffers::Invalid { swapchain },
            x => x,
        }
    }
}

impl Renderer {
    fn create_device(
        instance: &Arc<Instance>,
        surface: &Surface<Window>,
    ) -> Result<(Arc<Device>, Arc<Queue>)> {
        // Look for a graphics card that meets our requirements.
        let required_extensions = vulkano::device::DeviceExtensions {
            khr_swapchain: true,
            ..vulkano::device::DeviceExtensions::none()
        };

        let (best_device, queue_family) = PhysicalDevice::enumerate(instance)
            .filter(|d| {
                d.supported_extensions()
                    .is_superset_of(&required_extensions)
            })
            .flat_map(|d| {
                d.queue_families()
                    .filter(|q| {
                        q.supports_graphics()
                            && q.supports_compute()
                            && q.supports_surface(surface).unwrap_or(false)
                    })
                    .map(move |q| (d, q))
            })
            .min_by_key(|(d, _)| match d.properties().device_type {
                PhysicalDeviceType::DiscreteGpu => 0,
                PhysicalDeviceType::IntegratedGpu => 1,
                PhysicalDeviceType::VirtualGpu => 2,
                PhysicalDeviceType::Cpu => 3,
                PhysicalDeviceType::Other => 4,
            })
            .expect("No Vulkan device!");

        println!("Using device: {:#?}", best_device);

        // Initialize the Vulkan device
        let (device, mut queues) = Device::new(
            best_device,
            vulkano::device::DeviceCreateInfo {
                enabled_extensions: required_extensions.union(best_device.required_extensions()),
                queue_create_infos: vec![vulkano::device::QueueCreateInfo::family(queue_family)],
                ..Default::default()
            },
        )?;
        assert!(queues.len() == 1);
        Ok((device, queues.next().unwrap()))
    }

    fn new(device: Arc<Device>, queue: Arc<Queue>, surface: Arc<Surface<Window>>) -> Result<Self> {
        let color_format = device
            .physical_device()
            .surface_formats(&surface, Default::default())?
            .iter()
            .find(|(f, c)| {
                *c == ColorSpace::SrgbNonLinear
                    && [Format::R8G8B8A8_UNORM, Format::B8G8R8A8_UNORM].contains(f)
            })
            .ok_or_else(|| anyhow!("no suitable color formats"))?
            .0;
        let depth_format = Format::D16_UNORM;

        // Render some test points
        let points: Vec<shader::Point> = (0..NUM_PARTICLES_PERAXIS)
            .flat_map(|x| (0..NUM_PARTICLES_PERAXIS).map(move |y| (x, y)))
            .flat_map(|(x, y)| {
                (0..NUM_PARTICLES_PERAXIS).map(move |z| (x as f32, y as f32, z as f32))
            })
            .map(|(x, y, z)| shader::Point {
                position: [
                    (x - NUM_PARTICLES_PERAXIS as f32 / 2.0) / (NUM_PARTICLES_PERAXIS as f32 / 2.0),
                    (y - NUM_PARTICLES_PERAXIS as f32 / 2.0) / (NUM_PARTICLES_PERAXIS as f32 / 2.0),
                    (z - NUM_PARTICLES_PERAXIS as f32 / 2.0) / (NUM_PARTICLES_PERAXIS as f32 / 2.0),
                    1.0,
                ],
            })
            .collect();

        let points_buffer = DeviceLocalBuffer::array(
            device.clone(),
            points.len() as u64,
            BufferUsage {
                transfer_destination: true,
                vertex_buffer: true,
                storage_buffer: true,
                ..Default::default()
            },
            [queue.family()],
        )?;
        let velocities_buffer = DeviceLocalBuffer::array(
            device.clone(),
            points.len() as u64,
            BufferUsage {
                transfer_destination: true,
                storage_buffer: true,
                ..Default::default()
            },
            [queue.family()],
        )?;
        let pressures_buffer = DeviceLocalBuffer::array(
            device.clone(),
            shader::compute::NUM_CELLS_TOTAL as u64 * 2,
            BufferUsage {
                transfer_destination: true,
                storage_buffer: true,
                ..Default::default()
            },
            [queue.family()],
        )?;

        // Upload the initial values to the GPU.
        let mut cmd_builder = AutoCommandBufferBuilder::primary(
            device.clone(),
            queue.family(),
            CommandBufferUsage::OneTimeSubmit,
        )?;
        cmd_builder
            .fill_buffer(velocities_buffer.clone(), 0)?
            .fill_buffer(pressures_buffer.clone(), 0)?
            .copy_buffer(
                CpuAccessibleBuffer::from_iter(
                    device.clone(),
                    BufferUsage::transfer_source(),
                    false,
                    points.iter().copied(),
                )?,
                points_buffer.clone(),
            )?;
        let inflight = cmd_builder.build()?.execute(queue.clone())?;

        let render_pass = single_pass_renderpass!(device.clone(),
            attachments: {
                color: {
                    load: Clear,
                    store: Store,
                    format: color_format,
                    samples: 1,
                },
                depth: {
                    load: Clear,
                    store: DontCare,
                    format: depth_format,
                    samples: 1,
                }
            },
            pass: {
                color: [color],
                depth_stencil: {depth}
            }
        )?;

        let cs = shader::compute::load(device.clone())?;
        let vs = shader::vertex::load(device.clone())?;
        let fs = shader::fragment::load(device.clone())?;

        let compute_storage_layout = DescriptorSetLayout::new(
            device.clone(),
            DescriptorSetLayoutCreateInfo {
                bindings: std::collections::BTreeMap::from([
                    (
                        0,
                        DescriptorSetLayoutBinding {
                            stages: ShaderStages {
                                compute: true,
                                ..Default::default()
                            },
                            ..DescriptorSetLayoutBinding::descriptor_type(
                                DescriptorType::StorageBuffer,
                            )
                        },
                    ),
                    (
                        1,
                        DescriptorSetLayoutBinding {
                            stages: ShaderStages {
                                compute: true,
                                ..Default::default()
                            },
                            ..DescriptorSetLayoutBinding::descriptor_type(
                                DescriptorType::StorageBuffer,
                            )
                        },
                    ),
                    (
                        2,
                        DescriptorSetLayoutBinding {
                            stages: ShaderStages {
                                compute: true,
                                ..Default::default()
                            },
                            ..DescriptorSetLayoutBinding::descriptor_type(
                                DescriptorType::StorageBuffer,
                            )
                        },
                    ),
                ]),
                ..Default::default()
            },
        )?;

        let compute_uniform_layout = DescriptorSetLayout::new(
            device.clone(),
            DescriptorSetLayoutCreateInfo {
                bindings: std::collections::BTreeMap::from([(
                    0,
                    DescriptorSetLayoutBinding {
                        stages: ShaderStages {
                            compute: true,
                            ..Default::default()
                        },
                        ..DescriptorSetLayoutBinding::descriptor_type(DescriptorType::UniformBuffer)
                    },
                )]),
                ..Default::default()
            },
        )?;

        let compute_pipeline = ComputePipeline::with_pipeline_layout(
            device.clone(),
            cs.entry_point("main").unwrap(),
            &shader::compute::SpecializationConstants::new(),
            PipelineLayout::new(
                device.clone(),
                PipelineLayoutCreateInfo {
                    set_layouts: vec![
                        compute_storage_layout.clone(),
                        compute_uniform_layout.clone(),
                    ],
                    ..Default::default()
                },
            )?,
            None,
        )?;

        let compute_storage_descriptors = PersistentDescriptorSet::new(
            compute_storage_layout,
            [
                WriteDescriptorSet::buffer(0, points_buffer.clone()),
                WriteDescriptorSet::buffer(1, velocities_buffer.clone()),
                WriteDescriptorSet::buffer(2, pressures_buffer.clone()),
            ],
        )?;

        let compute_uniforms = CpuBufferPool::new(
            device.clone(),
            BufferUsage::uniform_buffer_transfer_destination(),
        );
        let compute_uniform_descriptor_pool = SingleLayoutDescSetPool::new(compute_uniform_layout);

        let graphics_pipeline = GraphicsPipeline::start()
            .render_pass(Subpass::from(render_pass.clone(), 0).unwrap())
            .vertex_input_state(BuffersDefinition::new().vertex::<shader::Point>())
            .input_assembly_state(InputAssemblyState::new().topology(PrimitiveTopology::PointList))
            .vertex_shader(vs.entry_point("main").unwrap(), ())
            .viewport_state(ViewportState::viewport_dynamic_scissor_irrelevant())
            .fragment_shader(fs.entry_point("main").unwrap(), ())
            .build(device.clone())?;

        let vertex_uniforms = CpuBufferPool::new(
            device.clone(),
            BufferUsage::uniform_buffer_transfer_destination(),
        );
        let matrix = Self::create_matrix(surface.window().inner_size());

        let graphics_uniform_descriptor_pool =
            SingleLayoutDescSetPool::new(DescriptorSetLayout::new(
                device.clone(),
                DescriptorSetLayoutCreateInfo {
                    bindings: std::collections::BTreeMap::from([(
                        0,
                        DescriptorSetLayoutBinding {
                            stages: ShaderStages {
                                vertex: true,
                                ..Default::default()
                            },
                            ..DescriptorSetLayoutBinding::descriptor_type(
                                DescriptorType::UniformBuffer,
                            )
                        },
                    )]),
                    ..Default::default()
                },
            )?);

        Ok(Self {
            device,
            queue,
            surface,
            color_format,
            depth_format,
            render_pass,
            framebuffers: Framebuffers::NotCreated,
            inflight: Some(inflight.boxed()),
            compute_pipeline,
            graphics_pipeline,
            points: points_buffer,
            _velocities: velocities_buffer,
            pressures: pressures_buffer,
            compute_storage_descriptors,
            compute_uniforms,
            compute_uniform_descriptor_pool,
            vertex_uniforms,
            vertex_uniform_descriptor_pool: graphics_uniform_descriptor_pool,
            matrix,
            last_fps_print: Instant::now(),
            frames: 0,
            which_pressure_buffer: false,
        })
    }

    fn create_matrix(dimensions: PhysicalSize<u32>) -> Matrix4<f32> {
        let aspect = dimensions.width as f32 / dimensions.height as f32;
        let proj = cgmath::perspective(cgmath::Deg(90.0), aspect, 1.0, 100.0);
        let view = Matrix4::look_at_rh(
            Point3::new(0.0, -VIEW_DISTANCE, -VIEW_DISTANCE),
            Point3::new(0.0, 0.0, 0.0),
            Vector3::new(0.0, 1.0, 0.0),
        );
        proj * view
    }

    fn resize(&mut self) {
        self.framebuffers.invalidate()
    }

    fn render(&mut self) -> Result<()> {
        let dimensions = self.surface.window().inner_size();

        // First, ensure we have framebuffers set up.
        let (swapchain, framebuffers) = match std::mem::take(&mut self.framebuffers) {
            // We do, good!
            Framebuffers::Valid {
                swapchain,
                framebuffers,
            } => (swapchain, framebuffers),

            // We don't.
            fbs => {
                // Create a swapchain of color buffers, and a shared depth buffer.
                let info = SwapchainCreateInfo {
                    image_usage: ImageUsage::color_attachment(),
                    image_extent: dimensions.into(),
                    image_format: Some(self.color_format),
                    ..Default::default()
                };

                let (swapchain, images) = if let Framebuffers::Invalid { swapchain } = fbs {
                    // We have an existing swapchain, recreate it.
                    println!("recreate");
                    swapchain.recreate(info)
                } else {
                    println!("new");
                    Swapchain::new(self.device.clone(), self.surface.clone(), info)
                }?;

                let depth_buffer = ImageView::new_default(AttachmentImage::transient(
                    self.device.clone(),
                    dimensions.into(),
                    self.depth_format,
                )?)?;

                // Bind the color and depth buffers to framebuffers.
                let framebuffers = images
                    .into_iter()
                    .map(|i| {
                        let color_buffer = ImageView::new_default(i)?;

                        Framebuffer::new(
                            self.render_pass.clone(),
                            FramebufferCreateInfo {
                                attachments: vec![color_buffer, depth_buffer.clone()],
                                ..Default::default()
                            },
                        )
                        .map_err(anyhow::Error::new)
                    })
                    .collect::<Result<Vec<_>>>()?;

                // Recreate the projection matrix, since the window size may have changed.
                self.matrix = Self::create_matrix(dimensions);

                (swapchain, framebuffers)
            }
        };

        // Acquire a framebuffer.
        let (fb_idx, mut suboptimal, acquired) = match acquire_next_image(swapchain.clone(), None) {
            Ok(result) => result,
            Err(AcquireError::OutOfDate) => {
                // Recreate the swapchain and try again next frame.
                self.framebuffers = Framebuffers::Invalid { swapchain };
                return Ok(());
            }
            Err(e) => bail!(e),
        };

        let compute_uniforms = shader::compute::ty::Uniforms {
            which_pressure_buffer: self.which_pressure_buffer as u32,
        };
        let compute_uniform_buffer = self.compute_uniforms.next(compute_uniforms)?;

        let vertex_uniforms = shader::vertex::ty::Uniforms {
            matrix: self.matrix.into(),
        };
        let vertex_uniform_buffer = self.vertex_uniforms.next(vertex_uniforms)?;

        let mut cmd_builder = AutoCommandBufferBuilder::primary(
            self.device.clone(),
            self.queue.family(),
            CommandBufferUsage::OneTimeSubmit,
        )
        .expect("Could not create command buffer builder");

        let pressure_dst_range = if self.which_pressure_buffer {
            0..shader::compute::NUM_CELLS_TOTAL as u64
        } else {
            shader::compute::NUM_CELLS_TOTAL as u64..(shader::compute::NUM_CELLS_TOTAL as u64 * 2)
        };

        cmd_builder
            .fill_buffer(self.pressures.slice(pressure_dst_range).unwrap(), 0)?
            .bind_pipeline_compute(self.compute_pipeline.clone())
            .bind_descriptor_sets(
                PipelineBindPoint::Compute,
                self.compute_pipeline.layout().clone(),
                0,
                (
                    self.compute_storage_descriptors.clone(),
                    self.compute_uniform_descriptor_pool
                        .next([WriteDescriptorSet::buffer(0, compute_uniform_buffer)])?,
                ),
            )
            .dispatch([self.points.len() as u32 / 32, 1, 1])?;

        cmd_builder
            .begin_render_pass(
                framebuffers[fb_idx].clone(),
                SubpassContents::Inline,
                [
                    [0.0, 0.0, 0.0, 0.0].into(), // color clear value
                    1.0.into(),                  // depth clear value
                ],
            )?
            .set_viewport(
                0,
                [Viewport {
                    origin: [0.0, 0.0],
                    dimensions: dimensions.into(),
                    depth_range: 0.0..1.0,
                }],
            )
            .bind_pipeline_graphics(self.graphics_pipeline.clone())
            .bind_descriptor_sets(
                PipelineBindPoint::Graphics,
                self.graphics_pipeline.layout().clone(),
                0,
                self.vertex_uniform_descriptor_pool
                    .next([WriteDescriptorSet::buffer(0, vertex_uniform_buffer)])?,
            )
            .bind_vertex_buffers(0, self.points.clone())
            .draw(self.points.len() as u32, 1, 0, 0)?
            .end_render_pass()?;

        let commands = cmd_builder.build()?;

        let inflight = self
            .inflight
            .take()
            .unwrap_or_else(|| vulkano::sync::now(self.device.clone()).boxed())
            .join(acquired)
            .then_execute(self.queue.clone(), commands)?
            .then_swapchain_present(self.queue.clone(), swapchain.clone(), fb_idx)
            .then_signal_fence_and_flush();

        match inflight {
            Ok(i) => self.inflight = Some(i.boxed()),
            Err(FlushError::OutOfDate) => suboptimal = true,
            Err(e) => bail!(e),
        }

        self.framebuffers = if suboptimal {
            Framebuffers::Invalid { swapchain }
        } else {
            Framebuffers::Valid {
                swapchain,
                framebuffers,
            }
        };

        self.frames += 1;
        if self.last_fps_print.elapsed().as_secs() >= 1 {
            println!("FPS: {}", self.frames);
            self.last_fps_print = Instant::now();
            self.frames = 0;
        }

        Ok(())
    }
}

fn main() {
    let event_loop = winit::event_loop::EventLoop::new();

    // Create a Vulkan instance
    let instance = Instance::new(InstanceCreateInfo {
        enabled_extensions: vulkano_win::required_extensions(),
        ..Default::default()
    })
    .expect("failed to create Vulkan instance");

    // Create a window with a Vulkan surface
    let surface = winit::window::WindowBuilder::new()
        .build_vk_surface(&event_loop, instance.clone())
        .expect("failed to create window");

    let (device, queue) =
        Renderer::create_device(&instance, &surface).expect("failed to create device");
    let mut renderer = Renderer::new(device, queue, surface).expect("failed to create renderer");

    event_loop.run(move |event, _, control_flow| match event {
        Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } => {
            *control_flow = ControlFlow::Exit;
        }
        Event::WindowEvent {
            event: WindowEvent::Resized(_),
            ..
        } => {
            renderer.resize();
        }
        Event::RedrawEventsCleared => renderer.render().expect("render failed"),
        _ => (),
    })
}
//! Linux: copying a frame Chromium rendered offscreen (a dmabuf it lends
//! only for the length of its paint call) into a dmabuf of ours, on the GPU,
//! with Vulkan. Ours can then be handed to the Wayland compositor and kept
//! on screen for as long as the compositor holds it.
//!
//! The frames are 8-bit BGRA (`DRM_FORMAT_ARGB8888`), one plane, with the
//! DRM format modifier Chromium chose. A copy waits for the GPU before it
//! returns, so a buffer is complete when it is attached.
//!
//! Everything here is `unsafe` Vulkan behind a safe interface: the device,
//! the images, and the memory are owned by [`Copier`] and [`Target`] and
//! destroyed with them.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use ash::vk;

/// One plane of a dmabuf: its file, its row pitch, and where it starts.
#[derive(Debug, Clone, Copy)]
pub struct Plane {
    pub fd: RawFd,
    pub stride: u32,
    pub offset: u64,
}

/// A frame Chromium lent: BGRA, `width` × `height` pixels.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    pub planes: &'a [Plane],
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
}

/// `DRM_FORMAT_MOD_INVALID`: no explicit modifier.
pub const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

const FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;
const DMA_BUF: vk::ExternalMemoryHandleTypeFlags = vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT;

/// The Vulkan device that copies frames, on the GPU Chromium renders with.
pub struct Copier {
    _entry: ash::Entry,
    instance: ash::Instance,
    physical: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    family: u32,
    /// The queue family frames come from and go to (another driver's or
    /// process's use of the memory).
    foreign: u32,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    memory_fd: ash::khr::external_memory_fd::Device,
    modifiers_fn: ash::ext::image_drm_format_modifier::Device,
    memory_types: vk::PhysicalDeviceMemoryProperties,
}

impl std::fmt::Debug for Copier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Copier")
            .field("gpu", &self.name())
            .finish_non_exhaustive()
    }
}

/// A buffer of ours: a BGRA image in exportable memory, and its dmabuf.
pub struct Target {
    device: ash::Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
    fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub offset: u32,
    pub modifier: u64,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("modifier", &format_args!("{:#x}", self.modifier))
            .finish_non_exhaustive()
    }
}

impl Target {
    /// The buffer's dmabuf; it stays open while the target lives.
    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        // SAFETY: the image and memory were made on `device`, and no copy
        // into them is pending (each copy waits for its fence).
        unsafe {
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

fn vk_err(what: &str) -> impl Fn(vk::Result) -> String + '_ {
    move |e| format!("Vulkan: {what}: {e}")
}

const DEVICE_EXTENSIONS: [&std::ffi::CStr; 3] = [
    ash::khr::external_memory_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    ash::ext::image_drm_format_modifier::NAME,
];

impl Copier {
    /// Open each GPU that can import and export dmabufs with modifiers, in
    /// the order Vulkan lists them; the caller keeps the first that copies
    /// a frame ([`Copier::copy`] fails on a GPU that cannot read it).
    pub fn all() -> Result<Vec<Copier>, String> {
        // SAFETY: loading the system's Vulkan loader; it stays loaded while
        // the entry lives.
        let entry = unsafe { ash::Entry::load() }.map_err(|e| format!("Vulkan: {e}"))?;
        let app = vk::ApplicationInfo::default()
            .application_name(c"dreamer")
            .api_version(vk::API_VERSION_1_2);
        let info = vk::InstanceCreateInfo::default().application_info(&app);
        // SAFETY: valid create info; destroyed in the last copier's drop
        // (each copier gets its own instance below).
        let probe = unsafe { entry.create_instance(&info, None) }.map_err(vk_err("instance"))?;
        // SAFETY: a live instance.
        let count = unsafe { probe.enumerate_physical_devices() }
            .map_err(vk_err("devices"))?
            .len();
        // SAFETY: nothing made on the probe instance.
        unsafe { probe.destroy_instance(None) };
        let mut out = Vec::new();
        let mut why = Vec::new();
        for index in 0..count {
            match Copier::open(entry.clone(), &info, index) {
                Ok(c) => out.push(c),
                Err(e) => why.push(e),
            }
        }
        if out.is_empty() {
            return Err(if why.is_empty() {
                "Vulkan: no GPU".into()
            } else {
                why.join("; ")
            });
        }
        Ok(out)
    }

    fn open(
        entry: ash::Entry,
        info: &vk::InstanceCreateInfo,
        index: usize,
    ) -> Result<Self, String> {
        // SAFETY: valid create info; destroyed on every error path below
        // and in `Drop`.
        let instance = unsafe { entry.create_instance(info, None) }.map_err(vk_err("instance"))?;
        // SAFETY: `instance` is live in this closure; anything it made is
        // destroyed before an error returns.
        let made = unsafe { Self::device(&entry, &instance, index) };
        match made {
            Ok(parts) => Ok(Self::from_parts(entry, instance, parts)),
            Err(e) => {
                // SAFETY: nothing else made on it survives.
                unsafe { instance.destroy_instance(None) };
                Err(e)
            }
        }
    }

    /// # Safety
    /// `instance` must be live.
    unsafe fn device(
        _entry: &ash::Entry,
        instance: &ash::Instance,
        index: usize,
    ) -> Result<DeviceParts, String> {
        unsafe {
            let physical = *instance
                .enumerate_physical_devices()
                .map_err(vk_err("devices"))?
                .get(index)
                .ok_or("Vulkan: the GPU went away")?;
            let props = instance.get_physical_device_properties(physical);
            let name = props
                .device_name_as_c_str()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let available: Vec<_> = instance
                .enumerate_device_extension_properties(physical)
                .map_err(vk_err("extensions"))?;
            let has = |n: &std::ffi::CStr| {
                available
                    .iter()
                    .any(|e| e.extension_name_as_c_str().is_ok_and(|x| x == n))
            };
            if let Some(missing) = DEVICE_EXTENSIONS.iter().find(|n| !has(n)) {
                return Err(format!("{name}: no {}", missing.to_string_lossy()));
            }
            let foreign_ext = has(ash::ext::queue_family_foreign::NAME);
            let mut names: Vec<*const std::ffi::c_char> =
                DEVICE_EXTENSIONS.iter().map(|n| n.as_ptr()).collect();
            if foreign_ext {
                names.push(ash::ext::queue_family_foreign::NAME.as_ptr());
            }
            let family = instance
                .get_physical_device_queue_family_properties(physical)
                .iter()
                .position(|f| {
                    f.queue_flags
                        .intersects(vk::QueueFlags::TRANSFER | vk::QueueFlags::GRAPHICS)
                })
                .ok_or_else(|| format!("{name}: no queue that copies"))?
                as u32;
            let priorities = [1.0];
            let queues = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(family)
                .queue_priorities(&priorities)];
            let create = vk::DeviceCreateInfo::default()
                .queue_create_infos(&queues)
                .enabled_extension_names(&names);
            let device = instance
                .create_device(physical, &create, None)
                .map_err(|e| format!("{name}: device: {e}"))?;
            Ok(DeviceParts {
                physical,
                device,
                family,
                foreign: if foreign_ext {
                    vk::QUEUE_FAMILY_FOREIGN_EXT
                } else {
                    vk::QUEUE_FAMILY_EXTERNAL
                },
            })
        }
    }

    fn from_parts(entry: ash::Entry, instance: ash::Instance, p: DeviceParts) -> Self {
        // SAFETY: a live device with the extensions these load enabled.
        unsafe {
            let queue = p.device.get_device_queue(p.family, 0);
            let pool = p
                .device
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .queue_family_index(p.family)
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                    None,
                )
                .unwrap_or_default();
            let cmd = p
                .device
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(pool)
                        .command_buffer_count(1),
                )
                .ok()
                .and_then(|b| b.first().copied())
                .unwrap_or_default();
            let fence = p
                .device
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .unwrap_or_default();
            let memory_fd = ash::khr::external_memory_fd::Device::new(&instance, &p.device);
            let modifiers_fn =
                ash::ext::image_drm_format_modifier::Device::new(&instance, &p.device);
            let memory_types = instance.get_physical_device_memory_properties(p.physical);
            Copier {
                _entry: entry,
                instance,
                physical: p.physical,
                device: p.device,
                queue,
                family: p.family,
                foreign: p.foreign,
                pool,
                cmd,
                fence,
                memory_fd,
                modifiers_fn,
                memory_types,
            }
        }
    }

    /// The GPU's name, for messages.
    pub fn name(&self) -> String {
        // SAFETY: a live instance and its physical device.
        let props = unsafe { self.instance.get_physical_device_properties(self.physical) };
        props
            .device_name_as_c_str()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Of `wanted` (the modifiers the compositor takes for ARGB8888), those
    /// this GPU can copy into with one plane.
    pub fn modifiers(&self, wanted: &[u64]) -> Vec<u64> {
        // SAFETY: two-call query on a live physical device; the slice
        // outlives the second call.
        let supported: Vec<vk::DrmFormatModifierPropertiesEXT> = unsafe {
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            self.instance
                .get_physical_device_format_properties2(self.physical, FORMAT, &mut props);
            let n = list.drm_format_modifier_count as usize;
            let mut out = vec![vk::DrmFormatModifierPropertiesEXT::default(); n];
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
                .drm_format_modifier_properties(&mut out);
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            self.instance
                .get_physical_device_format_properties2(self.physical, FORMAT, &mut props);
            out
        };
        wanted
            .iter()
            .copied()
            .filter(|m| {
                supported.iter().any(|s| {
                    s.drm_format_modifier == *m
                        && s.drm_format_modifier_plane_count == 1
                        && s.drm_format_modifier_tiling_features
                            .contains(vk::FormatFeatureFlags::TRANSFER_DST)
                })
            })
            .collect()
    }

    fn memory_type(&self, bits: u32, flags: vk::MemoryPropertyFlags) -> Option<u32> {
        (0..self.memory_types.memory_type_count).find(|&i| {
            bits & (1 << i) != 0
                && self.memory_types.memory_types[i as usize]
                    .property_flags
                    .contains(flags)
        })
    }

    /// A buffer of ours, `width` × `height`, with one of `modifiers` (the
    /// driver picks): exportable as a dmabuf.
    pub fn target(&self, width: u32, height: u32, modifiers: &[u64]) -> Result<Target, String> {
        if modifiers.is_empty() {
            return Err("no DRM format modifier both the GPU and the compositor take".into());
        }
        let mut list =
            vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(modifiers);
        let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(DMA_BUF);
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(FORMAT)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut list)
            .push_next(&mut external);
        // SAFETY: valid create info on a live device; every object made is
        // destroyed on the error paths, or owned by the returned target.
        unsafe {
            let image = self
                .device
                .create_image(&info, None)
                .map_err(vk_err("image"))?;
            let fail = |e: String, memory: Option<vk::DeviceMemory>| {
                self.device.destroy_image(image, None);
                if let Some(m) = memory {
                    self.device.free_memory(m, None);
                }
                e
            };
            let req = self.device.get_image_memory_requirements(image);
            let Some(kind) = self
                .memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
                .or_else(|| {
                    self.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::empty())
                })
            else {
                return Err(fail("Vulkan: no memory type for the image".into(), None));
            };
            let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(DMA_BUF);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(kind)
                .push_next(&mut export)
                .push_next(&mut dedicated);
            let memory = match self.device.allocate_memory(&alloc, None) {
                Ok(m) => m,
                Err(e) => return Err(fail(format!("Vulkan: memory: {e}"), None)),
            };
            if let Err(e) = self.device.bind_image_memory(image, memory, 0) {
                return Err(fail(format!("Vulkan: bind: {e}"), Some(memory)));
            }
            let mut props = vk::ImageDrmFormatModifierPropertiesEXT::default();
            if let Err(e) = self
                .modifiers_fn
                .get_image_drm_format_modifier_properties(image, &mut props)
            {
                return Err(fail(format!("Vulkan: modifier: {e}"), Some(memory)));
            }
            let layout = self.device.get_image_subresource_layout(
                image,
                vk::ImageSubresource::default()
                    .aspect_mask(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT),
            );
            let fd = match self.memory_fd.get_memory_fd(
                &vk::MemoryGetFdInfoKHR::default()
                    .memory(memory)
                    .handle_type(DMA_BUF),
            ) {
                Ok(fd) => OwnedFd::from_raw_fd(fd),
                Err(e) => return Err(fail(format!("Vulkan: export: {e}"), Some(memory))),
            };
            Ok(Target {
                device: self.device.clone(),
                image,
                memory,
                fd,
                width,
                height,
                stride: layout.row_pitch as u32,
                offset: layout.offset as u32,
                modifier: props.drm_format_modifier,
            })
        }
    }

    /// Copy `frame` into `into` (cropped to the smaller of the two), and
    /// wait for the GPU. `frame`'s files are only read; the caller still
    /// owns them.
    pub fn copy(&self, frame: &Frame<'_>, into: &Target) -> Result<(), String> {
        let [plane] = frame.planes else {
            return Err(format!(
                "{} planes in a frame; one is supported",
                frame.planes.len()
            ));
        };
        // SAFETY: the imported image and memory live until the end of this
        // block, after the GPU finished with them (the fence wait).
        unsafe {
            let source = self.import(frame, plane)?;
            let result = self.record_and_wait(source.0, frame, into);
            self.device.destroy_image(source.0, None);
            self.device.free_memory(source.1, None);
            result
        }
    }

    /// # Safety
    /// `plane.fd` must be an open dmabuf of `frame`.
    unsafe fn import(
        &self,
        frame: &Frame<'_>,
        plane: &Plane,
    ) -> Result<(vk::Image, vk::DeviceMemory), String> {
        unsafe {
            let layouts = [vk::SubresourceLayout {
                offset: plane.offset,
                size: 0,
                row_pitch: u64::from(plane.stride),
                array_pitch: 0,
                depth_pitch: 0,
            }];
            let mut explicit = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
                .drm_format_modifier(frame.modifier)
                .plane_layouts(&layouts);
            let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(DMA_BUF);
            let info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(FORMAT)
                .extent(vk::Extent3D {
                    width: frame.width,
                    height: frame.height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::TRANSFER_SRC)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .push_next(&mut explicit)
                .push_next(&mut external);
            let image = self
                .device
                .create_image(&info, None)
                .map_err(vk_err("importing a frame"))?;
            let mut fd_props = vk::MemoryFdPropertiesKHR::default();
            let req = self.device.get_image_memory_requirements(image);
            let bits =
                match self
                    .memory_fd
                    .get_memory_fd_properties(DMA_BUF, plane.fd, &mut fd_props)
                {
                    Ok(()) => req.memory_type_bits & fd_props.memory_type_bits,
                    Err(e) => {
                        self.device.destroy_image(image, None);
                        return Err(format!("Vulkan: the frame's dmabuf: {e}"));
                    }
                };
            let Some(kind) = self.memory_type(bits, vk::MemoryPropertyFlags::empty()) else {
                self.device.destroy_image(image, None);
                return Err("Vulkan: no memory type takes the frame".into());
            };
            // Vulkan owns the file it imports: give it a copy.
            let dup = libc::fcntl(plane.fd, libc::F_DUPFD_CLOEXEC, 0);
            if dup < 0 {
                self.device.destroy_image(image, None);
                return Err(format!(
                    "the frame's dmabuf: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let mut import = vk::ImportMemoryFdInfoKHR::default()
                .handle_type(DMA_BUF)
                .fd(dup);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(kind)
                .push_next(&mut import)
                .push_next(&mut dedicated);
            let memory = match self.device.allocate_memory(&alloc, None) {
                Ok(m) => m,
                Err(e) => {
                    libc::close(dup);
                    self.device.destroy_image(image, None);
                    return Err(format!("Vulkan: importing the frame's memory: {e}"));
                }
            };
            if let Err(e) = self.device.bind_image_memory(image, memory, 0) {
                self.device.destroy_image(image, None);
                self.device.free_memory(memory, None);
                return Err(format!("Vulkan: bind: {e}"));
            }
            Ok((image, memory))
        }
    }

    /// # Safety
    /// `source` must be a live image of `frame`'s size on this device.
    unsafe fn record_and_wait(
        &self,
        source: vk::Image,
        frame: &Frame<'_>,
        into: &Target,
    ) -> Result<(), String> {
        let color = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let barrier = |image, old, new, from, to, src: vk::AccessFlags, dst: vk::AccessFlags| {
            vk::ImageMemoryBarrier::default()
                .image(image)
                .old_layout(old)
                .new_layout(new)
                .src_queue_family_index(from)
                .dst_queue_family_index(to)
                .src_access_mask(src)
                .dst_access_mask(dst)
                .subresource_range(color)
        };
        let layers = vk::ImageSubresourceLayers::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .layer_count(1);
        let region = vk::ImageCopy::default()
            .src_subresource(layers)
            .dst_subresource(layers)
            .extent(vk::Extent3D {
                width: frame.width.min(into.width),
                height: frame.height.min(into.height),
                depth: 1,
            });
        unsafe {
            let d = &self.device;
            d.reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty())
                .map_err(vk_err("reset"))?;
            d.begin_command_buffer(
                self.cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .map_err(vk_err("begin"))?;
            // Chromium's frame comes from outside this queue (acquired, its
            // contents kept); ours is overwritten whole.
            let before = [
                barrier(
                    source,
                    vk::ImageLayout::GENERAL,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    self.foreign,
                    self.family,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::TRANSFER_READ,
                ),
                barrier(
                    into.image,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::QUEUE_FAMILY_IGNORED,
                    vk::QUEUE_FAMILY_IGNORED,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::TRANSFER_WRITE,
                ),
            ];
            d.cmd_pipeline_barrier(
                self.cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &before,
            );
            d.cmd_copy_image(
                self.cmd,
                source,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                into.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            );
            // Ours goes to the compositor; Chromium's goes back.
            let after = [
                barrier(
                    into.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::GENERAL,
                    self.family,
                    self.foreign,
                    vk::AccessFlags::TRANSFER_WRITE,
                    vk::AccessFlags::empty(),
                ),
                barrier(
                    source,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::ImageLayout::GENERAL,
                    self.family,
                    self.foreign,
                    vk::AccessFlags::TRANSFER_READ,
                    vk::AccessFlags::empty(),
                ),
            ];
            d.cmd_pipeline_barrier(
                self.cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &after,
            );
            d.end_command_buffer(self.cmd).map_err(vk_err("end"))?;
            let cmds = [self.cmd];
            let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
            d.queue_submit(self.queue, &submit, self.fence)
                .map_err(vk_err("submit"))?;
            let waited = d.wait_for_fences(&[self.fence], true, 1_000_000_000);
            d.reset_fences(&[self.fence]).map_err(vk_err("fence"))?;
            waited.map_err(vk_err("waiting for the copy"))
        }
    }
}

struct DeviceParts {
    physical: vk::PhysicalDevice,
    device: ash::Device,
    family: u32,
    foreign: u32,
}

impl Drop for Copier {
    fn drop(&mut self) {
        // SAFETY: the device is idle (every copy waited for its fence);
        // targets hold a clone of the device handle but must be dropped
        // first, which their owner (the subsurface) guarantees by order.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

use crate::geometry::{Point, Rect};
use crate::overlays::OverlayKind;
use crate::reactor::Reactor;
use crate::vulkan::texture::Texture;
use crate::vulkan::GpuTask;

use ash::version::DeviceV1_0;
use ash::{vk, Device};

use anyhow::Result;

use crossbeam::atomic::AtomicCell;
use futures::future::RemoteHandle;
// use futures::lock::Mutex;
use handlegraph::handle::{Handle, NodeId};
use handlegraph::pathhandlegraph::PathId;
#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};
use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;

use crate::app::selection::SelectionBuffer;

use crate::vulkan::{draw_system::nodes::NodeVertices, GfaestusVk};

use super::{ComputeManager, ComputePipeline};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadState {
    Idle,
    Loading,
    // ShouldReload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderState {
    Idle,
    Rendering,
    // ShouldRerender,
}

//////////////

#[derive(Debug)]
pub struct PathViewState {
    loading: AtomicCell<LoadState>,
    rendering: AtomicCell<RenderState>,

    should_rerender: AtomicCell<bool>,
    should_reload: AtomicCell<bool>,
}

#[derive(Debug)]
pub struct PathState {
    should_reload: AtomicCell<bool>,
}

impl std::default::Default for PathState {
    fn default() -> Self {
        Self {
            should_reload: true.into(),
        }
    }
}

impl std::default::Default for PathViewState {
    fn default() -> Self {
        Self {
            loading: AtomicCell::new(LoadState::Idle),
            rendering: AtomicCell::new(RenderState::Idle),

            should_reload: AtomicCell::new(true),
            should_rerender: AtomicCell::new(false),
        }
    }
}

impl PathViewState {
    pub fn loading(&self) -> LoadState {
        self.loading.load()
    }
    pub fn rendering(&self) -> RenderState {
        self.rendering.load()
    }

    pub fn should_reload(&self) -> bool {
        self.should_reload.load()
    }

    pub fn should_rerender(&self) -> bool {
        self.should_rerender.load()
    }

    pub fn force_reload(&self) {
        self.should_reload.store(true);
    }

    pub fn force_rerender(&self) {
        self.should_rerender.store(true);
    }
}

////////////

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RowState {
    Null,
    NeedLoad(PathId),
    Loaded(PathId),
}

impl std::default::Default for RowState {
    fn default() -> Self {
        Self::Null
    }
}

impl RowState {
    pub fn is_null(&self) -> bool {
        *self == Self::Null
    }

    pub fn is_loaded(&self) -> bool {
        matches!(self, RowState::Loaded(_))
    }

    pub fn is_loaded_path(&self, path: PathId) -> bool {
        *self == RowState::Loaded(path)
    }

    pub fn path(&self) -> Option<PathId> {
        match *self {
            RowState::NeedLoad(i) => Some(i),
            RowState::Loaded(i) => Some(i),
            RowState::Null => None,
        }
    }

    pub fn same_path(&self, path: PathId) -> bool {
        let this_path = match *self {
            RowState::NeedLoad(i) => i,
            RowState::Loaded(i) => i,
            RowState::Null => return false,
        };

        this_path == path
    }
}
#[allow(dead_code)]
pub struct PathViewRenderer {
    rgb_pipeline: ComputePipeline,
    val_pipeline: ComputePipeline,
    descriptor_set_layout: vk::DescriptorSetLayout,

    descriptor_pool: vk::DescriptorPool,
    buffer_desc_set: vk::DescriptorSet,

    pub width: usize,
    pub height: usize,

    translation: Arc<AtomicCell<f32>>,
    scaling: Arc<AtomicCell<f32>>,

    center: Arc<AtomicCell<f64>>,
    radius: Arc<AtomicCell<f64>>,

    pub state: Arc<PathViewState>,

    path_order: Arc<Mutex<Vec<PathId>>>,
    path_load_states: Arc<Vec<PathState>>,

    row_states: Arc<Vec<AtomicCell<RowState>>>,

    path_data: Arc<Mutex<Vec<u32>>>,
    path_count: Arc<AtomicCell<usize>>,

    path_buffer: vk::Buffer,
    path_allocation: vk_mem::Allocation,
    path_allocation_info: vk_mem::AllocationInfo,

    pub output_image: Texture,

    fence_id: AtomicCell<Option<usize>>,

    initialized: AtomicCell<bool>,
}

impl PathViewRenderer {
    pub fn fence_id(&self) -> Option<usize> {
        self.fence_id.load()
    }

    pub fn block_on_fence(&self, comp_manager: &mut ComputeManager) {
        if let Some(fid) = self.fence_id.load() {
            comp_manager.block_on_fence(fid).unwrap();
            comp_manager.free_fence(fid, false).unwrap();
            self.fence_id.store(None);

            self.state.rendering.store(RenderState::Idle);
            self.state.should_rerender.store(false);

            self.initialized.store(true);
        }
    }

    pub fn new(
        app: &GfaestusVk,
        rgb_overlay_desc_layout: vk::DescriptorSetLayout,
        val_overlay_desc_layout: vk::DescriptorSetLayout,
    ) -> Result<Self> {
        let width = 2048;
        let height = 64;
        let size = width * height;

        let device = app.vk_context().device();

        let (path_buffer, path_allocation, path_allocation_info) = {
            let usage = vk::BufferUsageFlags::STORAGE_BUFFER
                | vk::BufferUsageFlags::TRANSFER_DST;
            // | vk::BufferUsageFlags::TRANSFER_SRC;
            let memory_usage = vk_mem::MemoryUsage::CpuToGpu;

            let data = vec![0u32; size];

            let (buffer, allocation, allocation_info) =
                app.create_buffer_with_data(usage, memory_usage, true, &data)?;

            app.set_debug_object_name(
                buffer,
                "Path View Renderer (Path Buffer)",
            )?;

            (buffer, allocation, allocation_info)
        };

        dbg!();

        let output_image = {
            let format = vk::Format::R8G8B8A8_UNORM;

            let texture = Texture::allocate(
                app,
                app.transient_command_pool,
                app.graphics_queue,
                width,
                height,
                format,
                vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::STORAGE
                    | vk::ImageUsageFlags::SAMPLED,
            )?;

            texture
        };

        dbg!();

        let descriptor_pool = {
            let buffer_size = vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_BUFFER,
                // ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: 1,
            };

            let image_size = vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_IMAGE,
                // ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: 1,
            };

            let pool_sizes = [buffer_size, image_size];

            let pool_info = vk::DescriptorPoolCreateInfo::builder()
                .pool_sizes(&pool_sizes)
                .max_sets(2)
                .build();

            unsafe { device.create_descriptor_pool(&pool_info, None) }
        }?;

        dbg!();

        let descriptor_set_layout = Self::create_descriptor_set_layout(device)?;

        let descriptor_sets = {
            let layouts = vec![descriptor_set_layout];

            let alloc_info = vk::DescriptorSetAllocateInfo::builder()
                .descriptor_pool(descriptor_pool)
                .set_layouts(&layouts)
                .build();

            unsafe { device.allocate_descriptor_sets(&alloc_info) }
        }?;

        dbg!();

        let buffer_desc_set = descriptor_sets[0];

        {
            let path_buf_info = vk::DescriptorBufferInfo::builder()
                .buffer(path_buffer)
                .offset(0)
                .range(vk::WHOLE_SIZE)
                .build();

            let path_buf_infos = [path_buf_info];

            let path_write = vk::WriteDescriptorSet::builder()
                .dst_set(buffer_desc_set)
                .dst_binding(0)
                .dst_array_element(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&path_buf_infos)
                .build();

            let output_img_info = vk::DescriptorImageInfo::builder()
                .image_layout(vk::ImageLayout::GENERAL)
                .image_view(output_image.view)
                // .sampler(sampler)
                .build();
            let image_infos = [output_img_info];

            let output_write = vk::WriteDescriptorSet::builder()
                .dst_set(buffer_desc_set)
                .dst_binding(1)
                .dst_array_element(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&image_infos)
                .build();

            let desc_writes = [path_write, output_write];

            unsafe { device.update_descriptor_sets(&desc_writes, &[]) };
        }

        dbg!();

        let pipeline_layout = {
            use vk::ShaderStageFlags as Flags;

            let pc_range = vk::PushConstantRange::builder()
                .stage_flags(Flags::COMPUTE)
                .offset(0)
                .size(24)
                .build();

            let pc_ranges = [pc_range];
            // let pc_ranges = [];

            let layouts = [descriptor_set_layout, rgb_overlay_desc_layout];

            let layout_info = vk::PipelineLayoutCreateInfo::builder()
                .set_layouts(&layouts)
                .push_constant_ranges(&pc_ranges)
                .build();

            unsafe { device.create_pipeline_layout(&layout_info, None) }
        }?;

        dbg!();

        let rgb_pipeline = ComputePipeline::new(
            device,
            descriptor_set_layout,
            pipeline_layout,
            crate::include_shader!("compute/path_view.comp.spv"),
        )?;

        let pipeline_layout = {
            use vk::ShaderStageFlags as Flags;

            let pc_range = vk::PushConstantRange::builder()
                .stage_flags(Flags::COMPUTE)
                .offset(0)
                .size(24)
                .build();

            let pc_ranges = [pc_range];
            // let pc_ranges = [];

            let layouts = [descriptor_set_layout, val_overlay_desc_layout];

            let layout_info = vk::PipelineLayoutCreateInfo::builder()
                .set_layouts(&layouts)
                .push_constant_ranges(&pc_ranges)
                .build();

            unsafe { device.create_pipeline_layout(&layout_info, None) }
        }?;

        dbg!();

        let val_pipeline = ComputePipeline::new(
            device,
            descriptor_set_layout,
            pipeline_layout,
            crate::include_shader!("compute/path_view_val.comp.spv"),
        )?;

        let path_load_states: Arc<Vec<PathState>> = {
            let mut states = Vec::new();
            states.resize_with(64, || PathState::default());
            Arc::new(states)
        };

        let row_states: Arc<Vec<AtomicCell<RowState>>> = {
            let mut states = Vec::new();
            states.resize_with(64, || RowState::default().into());
            Arc::new(states)
        };

        dbg!();

        Ok(Self {
            rgb_pipeline,
            val_pipeline,
            descriptor_set_layout,

            descriptor_pool,
            buffer_desc_set,

            width,
            height,

            translation: Arc::new(AtomicCell::new(0.0)),
            scaling: Arc::new(AtomicCell::new(0.0)),

            center: Arc::new(AtomicCell::new(0.5)),
            radius: Arc::new(AtomicCell::new(0.5)),
            state: Arc::new(PathViewState::default()),

            path_order: Arc::new(Mutex::new(Vec::with_capacity(64))),
            path_load_states,

            row_states,

            path_data: Arc::new(Mutex::new(vec![0u32; width * height])),
            path_count: Arc::new(AtomicCell::new(0)),

            path_buffer,
            path_allocation,
            path_allocation_info,

            output_image,
            // output_buffer,
            // output_allocation,
            // output_allocation_info,
            fence_id: AtomicCell::new(None),

            initialized: false.into(),
        })
    }

    pub fn initialized(&self) -> bool {
        self.initialized.load()
    }

    fn enforce_view_limits(&self) {
        let center = self.center.load();
        let radius = self.radius.load();

        let mut new_center = center;
        let mut new_radius = radius;

        if radius > 0.5 {
            new_radius = 0.5;
        }

        if center < radius {
            new_center = radius;
        }

        if center + radius >= 1.0 {
            new_center = 1.0 - radius;
        }

        self.center.store(new_center);
        self.radius.store(new_radius);
    }

    pub fn reset_zoom(&self) {
        // self.set_visible_range(0.0, 1.0);
        self.center.store(0.5);
        self.radius.store(0.5);

        self.should_reload();
        self.should_rerender();
    }

    pub fn set_visible_range(&self, left: f64, right: f64) {
        // let center =
        let l = left.min(right).clamp(0.0, 4096.0);
        let r = left.max(right).clamp(0.0, 4096.0);

        let len = r - l;
        let mid = left + (len / 2.0);

        self.center.store(mid);
        self.radius.store(len);

        self.should_reload();
        self.should_rerender();
    }

    pub fn force_reload(&self) {
        self.state.should_reload.store(true);
    }

    pub fn pan(&self, pixel_delta: f64) {
        let center = self.center.load();
        let radius = self.radius.load();

        let t = self.translation.load();
        self.translation.store(t + pixel_delta as f32);

        self.state.should_rerender.store(true);

        self.center.store(center + pixel_delta);

        self.enforce_view_limits();

        log::warn!(
            "center: {}\tradius: {}",
            self.center.load(),
            self.radius.load()
        );
    }

    pub fn zoom(&self, delta: f64) {
        // let center = self.center.load();
        let radius = self.radius.load();

        let radius_ = (radius * delta).clamp(0.000_000_1, 0.5);

        self.radius.store(radius_);

        self.enforce_view_limits();

        if radius_ != radius {
            self.state.should_rerender.store(true);

            let s = self.scaling.load();
            self.scaling.store(s * delta as f32);
        }
    }

    pub fn should_rerender(&self) -> bool {
        self.state.should_rerender.load()
    }

    pub fn render_idle(&self) -> bool {
        matches!(self.state.rendering.load(), RenderState::Idle)
    }
    pub fn is_rendering(&self) -> bool {
        matches!(self.state.rendering.load(), RenderState::Rendering)
    }

    // only reload when there's nothing currently being loaded, and at
    // least one row should be reloaded
    pub fn should_reload(&self) -> bool {
        let is_loading = self.is_loading();

        let should_load = self
            .row_states
            .iter()
            .any(|r| matches!(r.load(), RowState::NeedLoad(_)));

        !is_loading && should_load
    }

    pub fn loading_idle(&self) -> bool {
        matches!(self.state.loading.load(), LoadState::Idle)
    }

    pub fn is_loading(&self) -> bool {
        matches!(self.state.loading.load(), LoadState::Loading)
    }

    pub fn mark_load_paths(
        &self,
        paths: impl IntoIterator<Item = PathId>,
    ) -> Result<()> {
        let to_mark: FxHashSet<_> = paths.into_iter().collect();

        let mut already_marked: FxHashMap<PathId, usize> = FxHashMap::default();

        let mut unused_rows: FxHashSet<usize> = FxHashSet::default();

        for (ix, row) in self.row_states.iter().enumerate() {
            match row.load() {
                RowState::NeedLoad(path) | RowState::Loaded(path) => {
                    if to_mark.contains(&path) {
                        already_marked.insert(path, ix);
                    } else {
                        unused_rows.insert(ix);
                    }
                }
                RowState::Null => {
                    unused_rows.insert(ix);
                }
            }
        }

        let premarked =
            already_marked.keys().copied().collect::<FxHashSet<_>>();
        let remaining = to_mark.difference(&premarked);

        let mut unused_rows = unused_rows.into_iter().collect::<Vec<_>>();
        unused_rows.sort();

        for (path, ix) in remaining.copied().zip(unused_rows.into_iter()) {
            self.row_states[ix].store(RowState::NeedLoad(path));
        }

        Ok(())
    }

    pub fn find_path_row(&self, path: PathId) -> Option<usize> {
        for (ix, row) in self.row_states.iter().enumerate() {
            if row.load().same_path(path) {
                return Some(ix);
            }
        }

        None
    }

    pub fn load_paths(
        &self,
        app: &GfaestusVk,
        reactor: &mut Reactor,
    ) -> Result<()> {
        let center = self.center.load();
        let radius = self.radius.load();

        let left = center - radius;
        let right = center + radius;

        log::warn!("loading with l: {}, r: {}", left, right);

        let translation = self.translation.clone();
        let scaling = self.scaling.clone();

        let graph = reactor.graph_query.clone();

        let width = self.width;
        let height = self.height;

        let gpu_tasks = reactor.gpu_tasks.clone();

        let buffer = self.path_buffer;

        let state = self.state.clone();

        let path_data = self.path_data.clone();
        let path_count = self.path_count.clone();

        let rows = self.row_states.clone();

        let fut = async move {
            let mut loaded_paths: Vec<(usize, PathId, Vec<u32>)> = Vec::new();

            let mut num_paths = 0;

            let mut first_path = true;

            let mut first = true;

            let mut first_p = None;
            let mut last_p = None;

            for (y, c) in rows.iter().enumerate() {
                let row = c.load();

                // num_paths is used by the shader to know how many
                // rows it can use, so we just want the highest
                // non-null row, since rows are always used from in order
                if !matches!(row, RowState::Null) {
                    num_paths = y;
                }

                if let RowState::NeedLoad(path) = row {
                    let mut path_row = Vec::with_capacity(width);

                    let steps = graph.path_pos_steps(path).unwrap();
                    let (_, _, path_len) = steps.last().unwrap();

                    // let len = (*path_len as f64) / 4096.0;
                    // let len = (*path_len as f64) / 2048.0;
                    let len = *path_len as f64;
                    let start = left * len;
                    let end = start + (right - left) * len;

                    let s = start as usize;
                    let e = end as usize;

                    if first {
                        log::warn!(
                            "path_len: {}\tleft: {}\tright: {}",
                            path_len,
                            left,
                            right
                        );
                        log::warn!("start: {}\tend: {}", s, e);
                    }

                    for x in 0..width {
                        let n = (x as f64) / width as f64;
                        let p_ = ((n as f64) * (end - start)) as usize;

                        let p = s + p_.max(1);

                        if first_path {
                            last_p = Some(p);
                        }

                        if first {
                            first = false;
                            first_p = Some(p);
                        }

                        let ix = match steps
                            .binary_search_by_key(&p, |(_, _, p)| *p)
                        {
                            Ok(i) => i,
                            Err(i) => i,
                        };

                        let ix = ix.min(steps.len() - 1);

                        let (handle, _step, _pos) = steps[ix];

                        let v = handle.id().0 - 1;
                        path_row.push(v as u32);
                    }
                    first_path = false;

                    loaded_paths.push((y, path, path_row));
                }
            }

            let (loaded, path_data_local): (Vec<(usize, PathId)>, Vec<u32>) = {
                let mut lock = path_data.lock();

                let mut loaded = Vec::new();

                for (y, path, path_data) in loaded_paths {
                    let offset = y * width;
                    let end = offset + width;

                    let slice = &mut lock[offset..end];
                    slice.clone_from_slice(&path_data);

                    loaded.push((y, path));
                }

                path_count.store(num_paths);

                (loaded, lock.to_owned())
            };

            log::warn!("{:?}\t{:?}", first_p, last_p);

            let data = Arc::new(path_data_local);
            let dst = buffer;
            let task = GpuTask::CopyDataToBuffer { data, dst };

            let copy_complete = gpu_tasks.queue_task(task);

            if let Ok(complete) = copy_complete {
                log::error!("in copy_complete");
                let _ = complete.await;
                // the path buffer has been updated here
                state.loading.store(LoadState::Idle);
                state.should_rerender.store(true);

                let mut need_load = 0;
                let mut loaded_ = 0;
                let mut null = 0;

                for (ix, path) in loaded {
                    let c = &rows[ix];

                    let row = c.load();

                    match row {
                        RowState::NeedLoad(p) => {
                            if p == path {
                                c.store(RowState::Loaded(p));
                            } else if p != path {
                                log::warn!(
                                    "path view row {} state is inconsistent!",
                                    ix
                                );
                            }
                        }
                        RowState::Loaded(p) => {
                            if p != path {
                                log::warn!(
                                    "path view row {} state is inconsistent!",
                                    ix
                                );
                            }
                        }
                        RowState::Null => {
                            log::warn!(
                                "path view row {} state loaded but is null!",
                                ix
                            );
                        }
                    }

                    match c.load() {
                        RowState::Null => null += 1,
                        RowState::NeedLoad(_) => need_load += 1,
                        RowState::Loaded(_) => loaded_ += 1,
                    }
                }

                log::error!(
                    "null: {}\tneed load: {}\tloaded: {}",
                    null,
                    need_load,
                    loaded_
                );

                translation.store(0.0);
                scaling.store(1.0);
            } else {
                log::error!("error queing GPU task in load_paths");
                state.loading.store(LoadState::Idle);
            }
        };

        reactor.spawn_forget(fut)?;

        Ok(())
    }

    pub fn get_node_at(&self, x: usize, y: usize) -> Option<NodeId> {
        let ix = y * self.width + x;

        let raw = self.path_data.try_lock().and_then(|l| l.get(ix).copied())?;

        if raw == 0 {
            return None;
        }

        let id = raw;
        let node = NodeId::from(id as u64);

        Some(node)
    }

    pub fn running(&self, comp_manager: &mut ComputeManager) -> Result<bool> {
        if let Some(fid) = self.fence_id.load() {
            let is_ready = comp_manager.is_fence_ready(fid)?;
            Ok(!is_ready)
        } else {
            Ok(false)
        }
    }

    pub fn dispatch_complete(
        &self,
        comp_manager: &mut ComputeManager,
    ) -> Result<bool> {
        dbg!();
        if let Some(fid) = self.fence_id.load() {
            dbg!();
            if comp_manager.is_fence_ready(fid)? {
                dbg!();
                comp_manager.block_on_fence(fid).unwrap();
                comp_manager.free_fence(fid, false).unwrap();
                self.fence_id.store(None);

                Ok(true)
            } else {
                dbg!();
                Ok(false)
            }
        } else {
            dbg!();
            Ok(false)
        }
    }

    pub fn dispatch_managed(
        &self,
        comp_manager: &mut ComputeManager,
        app: &GfaestusVk,
        rgb_overlay_desc: vk::DescriptorSet,
        val_overlay_desc: vk::DescriptorSet,
        overlay_kind: OverlayKind,
    ) -> Result<()> {
        if self.is_rendering() {
            return Ok(());
        }

        if let Some(fid) = self.fence_id.load() {
            dbg!();
            // handle this, but how
        } else {
            let path_count = self.path_count.load();
            dbg!();
            self.state.should_rerender.store(false);
            self.state.rendering.store(RenderState::Rendering);
            let fence_id = comp_manager.dispatch_with(|device, cmd_buf| {
                let (barrier, src_stage, dst_stage) =
                    GfaestusVk::image_transition_barrier(
                        self.output_image.image,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        vk::ImageLayout::GENERAL,
                    );

                unsafe {
                    device.cmd_pipeline_barrier(
                        cmd_buf,
                        src_stage,
                        dst_stage,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[barrier],
                    )
                };

                match overlay_kind {
                    OverlayKind::RGB => {
                        self.dispatch_cmd_rgb(
                            cmd_buf,
                            app,
                            rgb_overlay_desc,
                            path_count,
                        )
                        .unwrap();
                    }
                    OverlayKind::Value => {
                        self.dispatch_cmd_val(
                            cmd_buf,
                            app,
                            val_overlay_desc,
                            path_count,
                        )
                        .unwrap();
                    }
                }

                let (barrier, src_stage, dst_stage) =
                    GfaestusVk::image_transition_barrier(
                        self.output_image.image,
                        vk::ImageLayout::GENERAL,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    );

                unsafe {
                    device.cmd_pipeline_barrier(
                        cmd_buf,
                        src_stage,
                        dst_stage,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[barrier],
                    )
                };
            })?;

            self.fence_id.store(Some(fence_id));
        }

        Ok(())
    }

    pub fn dispatch_cmd_val(
        &self,
        cmd_buf: vk::CommandBuffer,
        app: &GfaestusVk,
        val_overlay_desc: vk::DescriptorSet,
        path_count: usize,
    ) -> Result<()> {
        let device = app.vk_context().device();

        unsafe {
            device.cmd_bind_pipeline(
                cmd_buf,
                vk::PipelineBindPoint::COMPUTE,
                self.val_pipeline.pipeline,
            );

            let desc_sets = [self.buffer_desc_set, val_overlay_desc];

            let null = [];
            device.cmd_bind_descriptor_sets(
                cmd_buf,
                vk::PipelineBindPoint::COMPUTE,
                self.val_pipeline.pipeline_layout,
                0,
                &desc_sets[0..=1],
                &null,
            );

            let push_constants = [
                path_count as u32,
                self.width as u32,
                self.height as u32,
                0u32,
            ];

            let float_consts = [self.translation.load(), self.scaling.load()];

            let mut bytes: Vec<u8> = Vec::with_capacity(24);
            bytes.extend_from_slice(bytemuck::cast_slice(&push_constants));
            bytes.extend_from_slice(bytemuck::cast_slice(&float_consts));

            use vk::ShaderStageFlags as Flags;
            device.cmd_push_constants(
                cmd_buf,
                self.rgb_pipeline.pipeline_layout,
                Flags::COMPUTE,
                0,
                &bytes,
            )
        };

        let x_group_count = self.width / 256;
        // let y_group_count = path_count;
        let y_group_count = 64;
        let z_group_count = 1;

        unsafe {
            device.cmd_dispatch(
                cmd_buf,
                x_group_count as u32,
                y_group_count as u32,
                z_group_count as u32,
            )
        };

        Ok(())
    }

    pub fn dispatch_cmd_rgb(
        &self,
        cmd_buf: vk::CommandBuffer,
        app: &GfaestusVk,
        rgb_overlay_desc: vk::DescriptorSet,
        path_count: usize,
    ) -> Result<()> {
        let device = app.vk_context().device();

        unsafe {
            device.cmd_bind_pipeline(
                cmd_buf,
                vk::PipelineBindPoint::COMPUTE,
                self.rgb_pipeline.pipeline,
            );

            let desc_sets = [self.buffer_desc_set, rgb_overlay_desc];

            let null = [];
            device.cmd_bind_descriptor_sets(
                cmd_buf,
                vk::PipelineBindPoint::COMPUTE,
                self.rgb_pipeline.pipeline_layout,
                0,
                &desc_sets[0..=1],
                &null,
            );

            let push_constants = [
                path_count as u32,
                self.width as u32,
                self.height as u32,
                0u32,
            ];

            // let (left, right) = self.offsets.load();
            // self.offsets.store((0.0, 1.0));
            // let float_consts = [left, right];
            let float_consts = [self.translation.load(), self.scaling.load()];

            let mut bytes: Vec<u8> = Vec::with_capacity(24);
            bytes.extend_from_slice(bytemuck::cast_slice(&push_constants));
            bytes.extend_from_slice(bytemuck::cast_slice(&float_consts));

            use vk::ShaderStageFlags as Flags;
            device.cmd_push_constants(
                cmd_buf,
                self.rgb_pipeline.pipeline_layout,
                Flags::COMPUTE,
                0,
                &bytes,
            )
        };

        let x_group_count = self.width / 256;
        // let y_group_count = path_count;
        let y_group_count = 64;
        let z_group_count = 1;

        unsafe {
            device.cmd_dispatch(
                cmd_buf,
                x_group_count as u32,
                y_group_count as u32,
                z_group_count as u32,
            )
        };

        Ok(())
    }

    fn layout_binding() -> [vk::DescriptorSetLayoutBinding; 2] {
        use vk::ShaderStageFlags as Stages;

        let path_buffer = vk::DescriptorSetLayoutBinding::builder()
            .binding(0)
            .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
            .descriptor_count(1)
            .stage_flags(Stages::COMPUTE)
            .build();

        let output_image = vk::DescriptorSetLayoutBinding::builder()
            .binding(1)
            .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
            .descriptor_count(1)
            .stage_flags(Stages::COMPUTE)
            .build();

        // let output_buffer = vk::DescriptorSetLayoutBinding::builder()
        //     .binding(1)
        //     .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
        //     .descriptor_count(1)
        //     .stage_flags(Stages::COMPUTE)
        //     .build();

        // let overlay_sampler = vk::DescriptorSetLayoutBinding::builder()
        //     .binding(2)
        //     .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        //     .descriptor_count(1)
        //     .stage_flags(Stages::COMPUTE)
        //     .build();

        // [path_buffer, output_buffer, overlay_sampler]
        [path_buffer, output_image]
    }

    fn create_descriptor_set_layout(
        device: &Device,
    ) -> Result<vk::DescriptorSetLayout> {
        let bindings = Self::layout_binding();

        let layout_info = vk::DescriptorSetLayoutCreateInfo::builder()
            .bindings(&bindings)
            .build();

        let layout =
            unsafe { device.create_descriptor_set_layout(&layout_info, None) }?;

        Ok(layout)
    }
}

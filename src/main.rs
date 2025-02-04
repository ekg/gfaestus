#[allow(unused_imports)]
use compute::EdgePreprocess;
use crossbeam::atomic::AtomicCell;
use futures::SinkExt;
use gfaestus::annotations::{BedRecords, ClusterCache, Gff3Records};
use gfaestus::context::{ContextEntry, ContextMenu};
use gfaestus::quad_tree::QuadTree;
use gfaestus::reactor::{ModalError, ModalHandler, ModalSuccess, Reactor};
use gfaestus::vulkan::context::EdgeRendererType;
use gfaestus::vulkan::draw_system::edges::EdgeRenderer;
use gfaestus::vulkan::texture::{Gradients, Gradients_};

use parking_lot::RwLock;
use rustc_hash::FxHashMap;
use std::collections::HashMap;

use winit::event::{ElementState, Event, MouseButton, WindowEvent};
use winit::event_loop::ControlFlow;

#[allow(unused_imports)]
use winit::window::{Window, WindowBuilder};

use gfaestus::app::{mainview::*, Args, OverlayCreatorMsg, Select};
use gfaestus::app::{App, AppMsg};
use gfaestus::geometry::*;
use gfaestus::graph_query::*;
use gfaestus::input::*;
use gfaestus::overlays::*;
use gfaestus::universe::*;
use gfaestus::view::View;
use gfaestus::vulkan::render_pass::Framebuffers;

use gfaestus::gui::{widgets::*, windows::*, *};

use gfaestus::vulkan::debug;

#[allow(unused_imports)]
use gfaestus::vulkan::draw_system::{
    nodes::Overlay, post::PostProcessPipeline,
};

use gfaestus::vulkan::draw_system::selection::{
    SelectionOutlineBlurPipeline, SelectionOutlineEdgePipeline,
};

use gfaestus::vulkan::compute::{
    ComputeManager, GpuSelection, NodeTranslation,
};

use anyhow::Result;

use ash::version::DeviceV1_0;
use ash::{vk, Device};

#[allow(unused_imports)]
use futures::executor::{ThreadPool, ThreadPoolBuilder};

use std::sync::Arc;

#[allow(unused_imports)]
use handlegraph::{
    handle::{Direction, Handle, NodeId},
    handlegraph::*,
    mutablehandlegraph::*,
    packed::*,
    pathhandlegraph::*,
};

#[allow(unused_imports)]
use handlegraph::packedgraph::PackedGraph;

use gfaestus::vulkan::*;

use flexi_logger::{Duplicate, FileSpec, Logger, LoggerHandle};

#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};

fn universe_from_gfa_layout(
    graph_query: &GraphQuery,
    layout_path: &str,
) -> Result<(Universe<FlatLayout>, GraphStats)> {
    let graph = graph_query.graph();

    let universe = Universe::from_laid_out_graph(&graph, layout_path)?;

    let stats = GraphStats {
        node_count: graph.node_count(),
        edge_count: graph.edge_count(),
        path_count: graph.path_count(),
        total_len: graph.total_length(),
    };

    Ok((universe, stats))
}

fn set_up_logger(args: &Args) -> Result<LoggerHandle> {
    let spec = match (args.trace, args.debug, args.quiet) {
        (true, _, _) => "trace",
        (_, true, _) => "debug",
        (_, _, true) => "",
        _ => "info",
    };

    let logger = Logger::try_with_env_or_str(spec)?
        .log_to_file(FileSpec::default())
        .duplicate_to_stderr(Duplicate::Debug)
        .start()?;

    Ok(logger)
}

fn main() {
    let args: Args = argh::from_env();

    let _logger = set_up_logger(&args).unwrap();

    log::debug!("Logger initalized");

    let gfa_file = &args.gfa;
    let layout_file = &args.layout;
    log::debug!("using {} and {}", gfa_file, layout_file);

    let (mut gfaestus, event_loop, window) = match GfaestusVk::new(&args) {
        Ok(app) => app,
        Err(err) => {
            error!("Error initializing Gfaestus");
            error!("{:?}", err.root_cause());
            std::process::exit(1);
        }
    };

    let renderer_config = gfaestus.vk_context().renderer_config;

    let num_cpus = num_cpus::get();

    let futures_cpus;
    let rayon_cpus;

    // TODO this has to be done much more intelligently
    if num_cpus < 4 {
        futures_cpus = 1;
        rayon_cpus = 1;
    } else if num_cpus == 4 {
        futures_cpus = 1;
        rayon_cpus = 2;
    } else if num_cpus <= 6 {
        futures_cpus = 2;
        rayon_cpus = num_cpus - 3;
    } else {
        futures_cpus = 3;
        rayon_cpus = num_cpus - 4;
    }

    log::debug!("futures thread pool: {}", futures_cpus);
    log::debug!("rayon   thread pool: {}", rayon_cpus);

    // TODO make sure to set thread pool size to less than number of CPUs
    let thread_pool = ThreadPoolBuilder::new()
        .pool_size(futures_cpus)
        .create()
        .unwrap();

    let rayon_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(rayon_cpus)
        .build()
        .unwrap();

    info!("Loading GFA");
    let t = std::time::Instant::now();

    let graph_query = Arc::new(GraphQuery::load_gfa(gfa_file).unwrap());

    let mut app = App::new((100.0, 100.0)).expect("error when creating App");

    let mut reactor = gfaestus::reactor::Reactor::init(
        thread_pool.clone(),
        rayon_pool,
        graph_query.clone(),
        app.channels(),
    );

    let graph_query_worker =
        GraphQueryWorker::new(graph_query.clone(), thread_pool.clone());

    let (mut universe, stats) =
        universe_from_gfa_layout(&graph_query, layout_file).unwrap();

    let (top_left, bottom_right) = universe.layout().bounding_box();

    let _center = Point {
        x: top_left.x + (bottom_right.x - top_left.x) / 2.0,
        y: top_left.y + (bottom_right.y - top_left.y) / 2.0,
    };

    info!(
        "layout bounding box\t({:.2}, {:.2})\t({:.2}, {:.2})",
        top_left.x, top_left.y, bottom_right.x, bottom_right.y
    );
    info!(
        "layout width: {:.2}\theight: {:.2}",
        bottom_right.x - top_left.x,
        bottom_right.y - top_left.y
    );

    info!("GFA loaded in {:.3} sec", t.elapsed().as_secs_f64());

    info!(
        "Loaded {} nodes\t{} points",
        universe.layout().nodes().len(),
        universe.layout().nodes().len() * 2
    );

    let mut compute_manager = ComputeManager::new(
        gfaestus.vk_context().device().clone(),
        gfaestus.graphics_family_index,
        gfaestus.graphics_queue,
    )
    .unwrap();

    let gpu_selection =
        GpuSelection::new(&gfaestus, graph_query.node_count()).unwrap();

    let node_translation =
        NodeTranslation::new(&gfaestus, graph_query.node_count()).unwrap();

    let mut select_fence_id: Option<usize> = None;
    let mut translate_fence_id: Option<usize> = None;

    let (winit_tx, winit_rx) =
        crossbeam::channel::unbounded::<WindowEvent<'static>>();

    let mut input_manager = InputManager::new(winit_rx, app.shared_state());

    input_manager.add_binding(winit::event::VirtualKeyCode::A, move || {
        println!("i'm a bound command!");
    });

    let app_rx = input_manager.clone_app_rx();
    let main_view_rx = input_manager.clone_main_view_rx();
    let gui_rx = input_manager.clone_gui_rx();

    let node_vertices = universe.node_vertices();

    let mut main_view = MainView::new(
        &gfaestus,
        app.clone_channels(),
        app.settings.clone(),
        app.shared_state().clone(),
        graph_query.node_count(),
    )
    .unwrap();

    let tree_bounding_box = {
        let height = (bottom_right.x - top_left.x) / 4.0;

        let mut tl = top_left;
        let mut br = bottom_right;

        tl.y = -height;
        br.y = height;

        Rect::new(tl, br)
    };

    let mut gui = Gui::new(
        &gfaestus,
        &mut reactor,
        app.shared_state().clone(),
        app.channels(),
        app.settings.clone(),
        &graph_query,
    )
    .unwrap();

    // create default overlays
    {
        let node_seq_script = "
fn node_color(id) {
  let h = handle(id, false);
  let seq = graph.sequence(h);
  let hash = hash_bytes(seq);
  let color = hash_color(hash);
  color
}
";

        let step_count_script = "
fn node_color(id) {
  let h = handle(id, false);

  let steps = graph.steps_on_handle(h);
  let count = 0.0;

  for step in steps {
    count += 1.0;
  }

  count
}
";

        create_overlay(
            &gfaestus,
            &mut main_view,
            &reactor,
            "Node Seq Hash",
            node_seq_script,
        )
        .expect("Error creating node seq hash overlay");

        create_overlay(
            &gfaestus,
            &mut main_view,
            &reactor,
            "Node Step Count",
            step_count_script,
        )
        .expect("Error creating step count overlay");
    }

    app.shared_state()
        .overlay_state
        .set_current_overlay(Some(0));

    let mut initial_view: Option<View> = None;
    let mut initialized_view = false;

    let new_overlay_rx = app.channels().new_overlay_rx.clone();

    let mut modal_handler =
        ModalHandler::new(app.shared_state().show_modal.to_owned());

    /*
    let mut receiver = {
        let (res_tx, res_rx) =
            futures::channel::mpsc::channel::<Option<String>>(2);

        let callback = move |text: &mut String, ui: &mut egui::Ui| {
            let _text_box = ui.text_edit_singleline(text);

            let ok_btn = ui.button("OK");
            let cancel_btn = ui.button("cancel");

            if ok_btn.clicked() {
                return Ok(ModalSuccess::Success);
            }

            if cancel_btn.clicked() {
                return Ok(ModalSuccess::Cancel);
            }

            Err(ModalError::Continue)
        };

        let prepared = ModalHandler::prepare_callback(
            &app.shared_state().show_modal,
            String::new(),
            callback,
            res_tx,
        );

        app.channels().modal_tx.send(prepared).unwrap();

        res_rx
    };

    {
        let _ = reactor.spawn_forget(async move {
            use futures::stream::StreamExt;
            log::warn!("awaiting modal result");
            let val = receiver.next().await;
            log::warn!("result: {:?}", val);
        });
    }
    */

    gui.app_view_state().graph_stats().send(GraphStatsMsg {
        node_count: Some(stats.node_count),
        edge_count: Some(stats.edge_count),
        path_count: Some(stats.path_count),
        total_len: Some(stats.total_len),
    });

    main_view
        .node_draw_system
        .vertices
        .upload_vertices(&gfaestus, &node_vertices)
        .unwrap();

    let mut edge_renderer = if gfaestus.vk_context().renderer_config.edges
        == EdgeRendererType::Disabled
    {
        log::warn!(
            "Device does not support tessellation shaders, disabling edges"
        );
        None
    } else {
        let edge_renderer = EdgeRenderer::new(
            &gfaestus,
            &graph_query.graph_arc(),
            universe.layout(),
        )
        .unwrap();

        Some(edge_renderer)
    };

    let mut dirty_swapchain = false;

    let mut selection_edge =
        SelectionOutlineEdgePipeline::new(&gfaestus, 1).unwrap();

    let mut selection_blur =
        SelectionOutlineBlurPipeline::new(&gfaestus, 1).unwrap();

    let gui_msg_tx = gui.clone_gui_msg_tx();

    // let gradients_ = Gradients_::initialize(
    //     &gfaestus,
    //     gfaestus.transient_command_pool,
    //     gfaestus.graphics_queue,
    //     1024,
    // )
    // .unwrap();

    let gradients = Gradients::initialize(
        &gfaestus,
        gfaestus.transient_command_pool,
        gfaestus.graphics_queue,
        1024,
    )
    .unwrap();

    gui.populate_overlay_list(
        main_view
            .node_draw_system
            .pipelines
            .overlay_names()
            .into_iter(),
    );

    const FRAME_HISTORY_LEN: usize = 10;
    let mut frame_time_history = [0.0f32; FRAME_HISTORY_LEN];
    let mut frame = 0;

    // hack to make the initial view correct -- we need to have the
    // event loop run and get a resize event before we know the
    // correct size, but we don't want to modify the current view
    // whenever the window resizes, so we use a timeout instead
    let initial_resize_timer = std::time::Instant::now();

    gui_msg_tx.send(GuiMsg::SetLightMode).unwrap();

    let mut context_menu = ContextMenu::new(&app);
    let open_context = AtomicCell::new(false);

    // let mut cluster_caches: HashMap<String, ClusterCache> = HashMap::default();
    // let mut step_caches: FxHashMap<PathId, Vec<(Handle, _, usize)>> =
    //     FxHashMap::default();

    if let Some(script_file) = args.run_script.as_ref() {
        if script_file == "-" {
            use bstr::ByteSlice;
            use std::io::prelude::*;

            let mut stdin = std::io::stdin();
            let mut script_bytes = Vec::new();
            let read = stdin.read_to_end(&mut script_bytes).unwrap();

            if let Ok(script) = script_bytes[0..read].to_str() {
                warn!("executing script {}", script_file);
                gui.console.eval_line(&mut reactor, true, script).unwrap();
            }
        } else {
            warn!("executing script file {}", script_file);
            gui.console
                .eval_file(&mut reactor, true, script_file)
                .unwrap();
        }
    }

    {
        for annot_path in &args.annotation_files {
            if annot_path.exists() {
                if let Some(path_str) = annot_path.to_str() {
                    let script = format!("load_collection(\"{}\");", path_str);
                    log::warn!("executing script: {}", script);
                    gui.console.eval_line(&mut reactor, true, &script).unwrap();
                }
            }
        }
    }

    event_loop.run(move |event, _, control_flow| {

        *control_flow = ControlFlow::Poll;

        // NB: AFAIK the only event that isn't 'static is the window
        // scale change (for high DPI displays), as it returns a
        // reference
        // so until the corresponding support is added, those events
        // are simply ignored here
        let event = if let Some(ev) = event.to_static() {
            ev
        } else {
            return;
        };

        if let Event::WindowEvent { event, .. } = &event {
            if let WindowEvent::MouseInput { state, button, .. } = event {
                if *state == ElementState::Pressed &&
                    *button == MouseButton::Right {

                        let focus = &app.shared_state().gui_focus_state;

                        // this whole thing should be handled better
                        if !focus.mouse_over_gui() {
                            main_view.send_context(context_menu.tx());
                        }

                        open_context.store(true);
                        // context_menu.open_context_menu(&gui.ctx);
                        context_menu.set_position(app.shared_state().mouse_pos());
                }
            }
        }

        while let Ok(callback) = app.channels().modal_rx.try_recv() {
            let _ = modal_handler.set_prepared_active(callback);
        }

        if let Event::WindowEvent { event, .. } = &event {
            let ev = event.clone();
            winit_tx.send(ev).unwrap();
        }

        let screen_dims = app.dims();

        match event {
            Event::NewEvents(_) => {
                if initial_resize_timer.elapsed().as_millis() > 100 && !initialized_view {
                    main_view.reset_view();
                    initialized_view = true;
                }

                // hacky -- this should take place after mouse pos is updated
                // in egui but before input is sent to mainview
                input_manager.handle_events(&mut reactor, &gui_msg_tx);

                let mouse_pos = app.mouse_pos();

                gui.push_event(egui::Event::PointerMoved(mouse_pos.into()));

                let hover_node = main_view
                    .read_node_id_at(mouse_pos)
                    .map(|nid| NodeId::from(nid as u64));

                app.channels().app_tx.send(AppMsg::HoverNode(hover_node)).unwrap();

                gui.set_hover_node(hover_node);

                if app.selection_changed() {
                    if let Some(selected) = app.selected_nodes() {

                        log::warn!("sending selection");
                        context_menu
                            .tx()
                            .send(ContextEntry::Selection { nodes: selected.to_owned() })
                            .unwrap();

                        let mut nodes = selected.iter().copied().collect::<Vec<_>>();
                        nodes.sort();

                        gui.app_view_state()
                            .node_list()
                            .send(NodeListMsg::SetFiltered(nodes));

                        main_view.update_node_selection(selected).unwrap();


                    } else {
                        gui.app_view_state()
                            .node_list()
                            .send(NodeListMsg::SetFiltered(Vec::new()));

                        main_view.clear_node_selection().unwrap();
                    }
                }

                while let Ok((key_code, command)) = app.channels().binds_rx.try_recv() {
                    if let Some(cmd) = command {
                        input_manager.add_binding(key_code, cmd);
                        // input_manager.add_binding(key_code, Box::new(cmd));
                    } else {
                    }
                }

                while let Ok(app_in) = app_rx.try_recv() {
                    app.apply_input(app_in, &gui_msg_tx);
                }

                while let Ok(gui_in) = gui_rx.try_recv() {
                    gui.apply_input(&app.channels().app_tx, gui_in);
                }

                while let Ok(main_view_in) = main_view_rx.try_recv() {
                    main_view.apply_input(screen_dims, app.mouse_pos(), main_view_in);
                }

                while let Ok(app_msg) = app.channels().app_rx.try_recv() {


                    if let AppMsg::RectSelect(rect) = &app_msg {

                        if select_fence_id.is_none() && translate_fence_id.is_none() {
                            let fence_id = gpu_selection.rectangle_select(
                                &mut compute_manager,
                                &main_view.node_draw_system.vertices,
                                *rect
                            ).unwrap();

                            select_fence_id = Some(fence_id);
                        }

                    }

                    if let AppMsg::TranslateSelected(delta) = &app_msg {
                        if select_fence_id.is_none() && translate_fence_id.is_none() {

                            let fence_id = node_translation
                                .translate_nodes(
                                    &mut compute_manager,
                                    &main_view.node_draw_system.vertices,
                                    &main_view.selection_buffer,
                                    *delta
                                ).unwrap();


                            translate_fence_id = Some(fence_id);
                        }
                    }

                    app.apply_app_msg(
                        tree_bounding_box,
                        main_view.main_view_msg_tx(),
                        &gui_msg_tx,
                        universe.layout().nodes(),
                        app_msg,
                    );
                }

                gui.apply_received_gui_msgs();

                while let Ok(main_view_msg) = main_view.main_view_msg_rx().try_recv() {
                    main_view.apply_msg(main_view_msg);
                }

                while let Ok(new_overlay) = new_overlay_rx.try_recv() {
                    if let Ok(_) = handle_new_overlay(
                        &gfaestus,
                        &mut main_view,
                        graph_query.node_count(),
                        new_overlay
                    ) {
                        gui.populate_overlay_list(
                            main_view
                                .node_draw_system
                                .pipelines
                                .overlay_names()
                                .into_iter(),
                        );
                    }
                }
            }
            Event::MainEventsCleared => {
                let screen_dims = app.dims();
                let mouse_pos = app.mouse_pos();
                main_view.update_view_animation(screen_dims, mouse_pos);

                let edge_ubo = app.settings.edge_renderer().load();

                for er in edge_renderer.iter_mut() {
                    er.write_ubo(&edge_ubo).unwrap();
                }
            }
            Event::RedrawEventsCleared => {

                log::trace!("Event::RedrawEventsCleared");
                let edge_ubo = app.settings.edge_renderer().load();
                let edge_width = edge_ubo.edge_width;

                if let Some(fid) = translate_fence_id {
                    if compute_manager.is_fence_ready(fid).unwrap() {
                        log::trace!("Node translation fence ready");
                        compute_manager.block_on_fence(fid).unwrap();
                        compute_manager.free_fence(fid, false).unwrap();

                        log::trace!("Compute fence freed, updating CPU node positions");
                        universe.update_positions_from_gpu(&gfaestus,
                                                           &main_view.node_draw_system.vertices).unwrap();

                        translate_fence_id = None;
                    }
                }

                if let Some(fid) = select_fence_id {

                    if compute_manager.is_fence_ready(fid).unwrap() {
                        log::trace!("Node selection fence ready");
                        compute_manager.block_on_fence(fid).unwrap();
                        compute_manager.free_fence(fid, false).unwrap();

                        GfaestusVk::copy_buffer(gfaestus.vk_context().device(),
                                                gfaestus.transient_command_pool,
                                                gfaestus.graphics_queue,
                                                gpu_selection.selection_buffer.buffer,
                                                main_view.selection_buffer.buffer,
                                                main_view.selection_buffer.size);
                        log::trace!("Copied selection buffer to main view");


                        let t = std::time::Instant::now();
                        main_view
                            .selection_buffer
                            .fill_selection_set(gfaestus
                                                .vk_context()
                                                .device())
                            .unwrap();
                        log::trace!("Updated CPU selection buffer");
                        trace!("fill_selection_set took {} ns", t.elapsed().as_nanos());

                        app.channels().app_tx
                            .send(AppMsg::Selection(Select::Many {
                            nodes: main_view
                                .selection_buffer
                                .selection_set()
                                .clone(),
                            clear: true }))
                            .unwrap();


                        select_fence_id = None;
                    }
                }

                let frame_t = std::time::Instant::now();

                if dirty_swapchain {
                    let size = window.inner_size();
                    log::trace!("Dirty swapchain, reconstructing");
                    if size.width > 0 && size.height > 0 {
                        app.update_dims([size.width as f32, size.height as f32]);
                        gfaestus
                            .recreate_swapchain(Some([size.width, size.height]))
                            .unwrap();

                        selection_edge.write_descriptor_set(
                            gfaestus.vk_context().device(),
                            gfaestus.node_attachments.mask_resolve,
                        );

                        selection_blur.write_descriptor_set(
                            gfaestus.vk_context().device(),
                            gfaestus.offscreen_attachment.color,
                        );

                        main_view
                            .recreate_node_id_buffer(&gfaestus, size.width, size.height)
                            .unwrap();

                        let new_initial_view =
                            View::from_dims_and_target(app.dims(), top_left, bottom_right);
                        if initial_view.is_none()
                            && initial_resize_timer.elapsed().as_millis() > 100
                        {
                            main_view.set_view(new_initial_view);
                            initial_view = Some(new_initial_view);
                        }

                        main_view.set_initial_view(
                            Some(new_initial_view.center),
                            Some(new_initial_view.scale),
                        );
                    } else {
                        log::debug!("Can't recreate swapchain with a zero resolution");
                        return;
                    }
                }

                gui.begin_frame(
                    &mut reactor,
                    Some(app.dims().into()),
                    &graph_query,
                    &graph_query_worker,
                    app.annotations(),
                    context_menu.tx(),
                );

                modal_handler.show(&gui.ctx);

                {
                    let ctx = &gui.ctx;
                    let clipboard = &mut gui.clipboard_ctx;

                    if open_context.load() {
                        context_menu.recv_contexts();
                        context_menu.open_context_menu(&gui.ctx);
                        open_context.store(false);
                    }

                    context_menu.show(ctx, &reactor, clipboard);
                }



                {
                    let shared_state = app.shared_state();
                    let view = shared_state.view();
                    let labels = app.labels();
                    // log::debug!("Clustering label sets");
                    let cluster_tree = labels.cluster(tree_bounding_box,
                                                      app.settings.label_radius().load(),
                                                      view);
                    // log::debug!("Drawing label sets");
                    cluster_tree.draw_labels(&gui.ctx, shared_state);
                    // cluster_tree.draw_clusters(&gui.ctx, view);
                }


                /*
                let annotations = app.annotations();

                log::trace!("Drawing label sets");
                for label_set in annotations.visible_label_sets() {

                    if !step_caches.contains_key(&label_set.path_id) {
                        let steps = graph_query.path_pos_steps(label_set.path_id).unwrap();
                        step_caches.insert(label_set.path_id, steps);
                    }

                    let steps = step_caches.get(&label_set.path_id).unwrap();

                    let label_radius = app.settings.label_radius().load();

                    use gfaestus::annotations::AnnotationColumn;

                    let column = &label_set.column;

                    let records: &dyn std::any::Any = match column {
                        AnnotationColumn::Gff3(_) => {
                            let records: &Gff3Records = app
                                .annotations()
                                .get_gff3(&label_set.annotation_name)
                                .unwrap();

                            let records_any: &dyn std::any::Any = records as _;
                            records_any
                        }
                        AnnotationColumn::Bed(_) => {
                            let records: &BedRecords = app
                                .annotations()
                                .get_bed(&label_set.annotation_name)
                                .unwrap();

                            let records_any: &dyn std::any::Any = records as _;
                            records_any
                        }
                    };


                    if !cluster_caches.contains_key(label_set.name()) {
                        let cluster_cache = ClusterCache::new_cluster(
                            &steps,
                            universe.layout().nodes(),
                            label_set,
                            app.shared_state().view(),
                            label_radius
                        );

                        cluster_caches.insert(label_set.name().to_string(),
                                              cluster_cache);
                    }

                    let cluster_cache = cluster_caches
                        .get_mut(label_set.name())
                        .unwrap();

                    cluster_cache
                        .rebuild_cluster(
                            &steps,
                            universe.layout().nodes(),
                            app.shared_state().view(),
                            label_radius
                        );

                    for (node, cluster_indices) in cluster_cache.node_labels.iter() {
                        let mut y_offset = 20.0;
                        let mut count = 0;

                        let label_indices = &cluster_indices.label_indices;

                        for &label_ix in label_indices.iter() {

                            let label = &cluster_cache.label_set.label_strings()[label_ix];
                            let offset = &cluster_cache
                                .cluster_offsets[cluster_indices.offset_ix];

                            let anchor_dir = Point::new(-offset.x, -offset.y);
                            let offset = *offset * 20.0;

                            let rect = gfaestus::gui::text::draw_text_at_node_anchor(
                                &gui.ctx,
                                universe.layout().nodes(),
                                app.shared_state().view(),
                                *node,
                                offset + Point::new(0.0, y_offset),
                                anchor_dir,
                                label
                            );

                            if let Some(rect) = rect {
                                let rect = rect.resize(0.98);
                                if rect.contains(app.mouse_pos()) {
                                    gfaestus::gui::text::draw_rect(&gui.ctx, rect);

                                    // hacky way to check for a click
                                    // for now, because i can't figure
                                    // egui out
                                    if gui.ctx.input().pointer.any_click() {
                                        match column {
                                            AnnotationColumn::Gff3(col) => {
                                                if let Some(gff) = records.downcast_ref::<Gff3Records>() {
                                                    gui.scroll_to_gff_record(gff, col, label.as_bytes());
                                                }
                                            }
                                            AnnotationColumn::Bed(col) => {
                                                if let Some(bed) = records.downcast_ref::<BedRecords>() {
                                                    gui.scroll_to_bed_record(bed, col, label.as_bytes());
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            y_offset += 15.0;
                            count += 1;

                            if count > 10 {
                                let count = count.min(label_indices.len());
                                let rem = label_indices.len() - count;

                                if rem > 0 {
                                    let more_label = format!("and {} more", rem);

                                    gfaestus::gui::text::draw_text_at_node_anchor(
                                        &gui.ctx,
                                        universe.layout().nodes(),
                                        app.shared_state().view(),
                                        *node,
                                        offset + Point::new(0.0, y_offset),
                                        anchor_dir,
                                        &more_label
                                    );
                                }
                                break;
                            }
                        }
                    }
                }
                */


                let meshes = gui.end_frame();

                gui.upload_texture(&gfaestus).unwrap();

                if !meshes.is_empty() {
                    gui.upload_vertices(&gfaestus, &meshes).unwrap();
                }

                let node_pass = gfaestus.render_passes.nodes;
                let edges_pass = gfaestus.render_passes.edges;
                let edge_pass = gfaestus.render_passes.selection_edge_detect;
                let blur_pass = gfaestus.render_passes.selection_blur;
                let gui_pass = gfaestus.render_passes.gui;

                let node_id_image = gfaestus.node_attachments.id_resolve.image;

                let offscreen_image = gfaestus.offscreen_attachment.color.image;

                let overlay =
                    app.shared_state().overlay_state().current_overlay();

                let current_view = app.shared_state().view();

                let edges_enabled = app.shared_state().edges_enabled();

                // TODO this should also check tess. isoline support etc. i think
                let edges_enabled = edges_enabled &&
                    !matches!(renderer_config.edges, EdgeRendererType::Disabled);

                let debug_utils = gfaestus.vk_context().debug_utils().map(|u| u.to_owned());

                let debug_utils = debug_utils.as_ref();

                let swapchain_dims = gfaestus.swapchain_dims();

                let draw =
                    |device: &Device, cmd_buf: vk::CommandBuffer, framebuffers: &Framebuffers| {
                        log::trace!("In draw_frame_from callback");
                        let size = swapchain_dims;

                        debug::begin_cmd_buf_label(
                            debug_utils,
                            cmd_buf,
                            "Image transitions"
                        );

                        log::trace!("Pre-rendering image transitions");
                        unsafe {
                            let offscreen_image_barrier = vk::ImageMemoryBarrier::builder()
                                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                                .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                                .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                                .image(offscreen_image)
                                .subresource_range(vk::ImageSubresourceRange {
                                    aspect_mask: vk::ImageAspectFlags::COLOR,
                                    base_mip_level: 0,
                                    level_count: 1,
                                    base_array_layer: 0,
                                    layer_count: 1,
                                })
                                .build();

                            let memory_barriers = [];
                            let buffer_memory_barriers = [];
                            let image_memory_barriers = [offscreen_image_barrier];
                            device.cmd_pipeline_barrier(
                                cmd_buf,
                                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                                vk::PipelineStageFlags::FRAGMENT_SHADER,
                                vk::DependencyFlags::BY_REGION,
                                &memory_barriers,
                                &buffer_memory_barriers,
                                &image_memory_barriers,
                            );
                        }

                        debug::end_cmd_buf_label(debug_utils, cmd_buf);

                        debug::begin_cmd_buf_label(
                            debug_utils,
                            cmd_buf,
                            "Nodes",
                        );

                        let gradient_name = app.shared_state().overlay_state().gradient();
                        let gradient = gradients.gradient(gradient_name).unwrap();

                        log::trace!("Drawing nodes");
                        main_view.draw_nodes(
                            cmd_buf,
                            node_pass,
                            framebuffers,
                            size.into(),
                            Point::ZERO,
                            overlay,
                            gradient,
                        ).unwrap();


                        debug::end_cmd_buf_label(debug_utils, cmd_buf);

                        if edges_enabled {

                            log::trace!("Drawing edges");
                            debug::begin_cmd_buf_label(
                                debug_utils,
                                cmd_buf,
                                "Edges",
                            );

                            /*
                            edge_pipeline.preprocess_cmd(
                                cmd_buf,
                                current_view,
                                [size.width as f32, size.height as f32]
                            ).unwrap();

                            edge_pipeline.preprocess_memory_barrier(cmd_buf).unwrap();
                            */

                            for er in edge_renderer.iter_mut() {
                                er.draw(
                                    cmd_buf,
                                    edge_width,
                                    &main_view.node_draw_system.vertices,
                                    edges_pass,
                                    framebuffers,
                                    size.into(),
                                    2.0,
                                    current_view,
                                    Point::ZERO,
                                ).unwrap();
                            }

                            debug::end_cmd_buf_label(debug_utils, cmd_buf);
                        }


                        log::trace!("Post-edge image transitions");
                        unsafe {
                            let image_memory_barrier = vk::ImageMemoryBarrier::builder()
                                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                                .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                                .image(node_id_image)
                                .subresource_range(vk::ImageSubresourceRange {
                                    aspect_mask: vk::ImageAspectFlags::COLOR,
                                    base_mip_level: 0,
                                    level_count: 1,
                                    base_array_layer: 0,
                                    layer_count: 1,
                                })
                                .build();

                            let memory_barriers = [];
                            let buffer_memory_barriers = [];
                            let image_memory_barriers = [image_memory_barrier];
                            device.cmd_pipeline_barrier(
                                cmd_buf,
                                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                                vk::PipelineStageFlags::FRAGMENT_SHADER,
                                vk::DependencyFlags::BY_REGION,
                                &memory_barriers,
                                &buffer_memory_barriers,
                                &image_memory_barriers,
                            );
                        }

                        debug::begin_cmd_buf_label(
                            debug_utils,
                            cmd_buf,
                            "Node selection border",
                        );

                        log::trace!("Drawing selection border edge detection");
                        selection_edge
                            .draw(
                                &device,
                                cmd_buf,
                                edge_pass,
                                framebuffers,
                                [size.width as f32, size.height as f32],
                            )
                            .unwrap();

                        log::trace!("Selection border edge detection -- image transitions");
                        unsafe {
                            let image_memory_barrier = vk::ImageMemoryBarrier::builder()
                                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                                .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                                .image(offscreen_image)
                                .subresource_range(vk::ImageSubresourceRange {
                                    aspect_mask: vk::ImageAspectFlags::COLOR,
                                    base_mip_level: 0,
                                    level_count: 1,
                                    base_array_layer: 0,
                                    layer_count: 1,
                                })
                                .build();

                            let memory_barriers = [];
                            let buffer_memory_barriers = [];
                            let image_memory_barriers = [image_memory_barrier];
                            // let image_memory_barriers = [];
                            device.cmd_pipeline_barrier(
                                cmd_buf,
                                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                                vk::PipelineStageFlags::FRAGMENT_SHADER,
                                vk::DependencyFlags::BY_REGION,
                                &memory_barriers,
                                &buffer_memory_barriers,
                                &image_memory_barriers,
                            );
                        }

                        log::trace!("Drawing selection border blur");
                        selection_blur
                            .draw(
                                &device,
                                cmd_buf,
                                blur_pass,
                                framebuffers,
                                [size.width as f32, size.height as f32],
                            )
                            .unwrap();

                        debug::end_cmd_buf_label(debug_utils, cmd_buf);

                        debug::begin_cmd_buf_label(
                            debug_utils,
                            cmd_buf,
                            "GUI",
                        );

                        log::trace!("Drawing GUI");
                        gui.draw(
                            cmd_buf,
                            gui_pass,
                            framebuffers,
                            size.into(),
                            &gradients,
                        )
                        .unwrap();

                        debug::end_cmd_buf_label(debug_utils, cmd_buf);

                        log::trace!("End of draw_frame_from callback");
                    };

                let size = window.inner_size();
                dirty_swapchain = gfaestus.draw_frame_from([size.width, size.height], draw).unwrap();

                if !dirty_swapchain {
                    let screen_dims = app.dims();

                    log::trace!("Copying node ID image to buffer");
                    GfaestusVk::copy_image_to_buffer(
                        gfaestus.vk_context().device(),
                        gfaestus.transient_command_pool,
                        gfaestus.graphics_queue,
                        gfaestus.node_attachments.id_resolve.image,
                        main_view.node_id_buffer(),
                        vk::Extent2D {
                            width: screen_dims.width as u32,
                            height: screen_dims.height as u32,
                        },
                    ).unwrap();
                }

                log::trace!("Calculating FPS");
                let frame_time = frame_t.elapsed().as_secs_f32();
                frame_time_history[frame % frame_time_history.len()] = frame_time;

                if frame > FRAME_HISTORY_LEN && frame % FRAME_HISTORY_LEN == 0 {
                    let ft_sum: f32 = frame_time_history.iter().sum();
                    let avg = ft_sum / (FRAME_HISTORY_LEN as f32);
                    let fps = 1.0 / avg;
                    let avg_ms = avg * 1000.0;

                    gui.app_view_state().fps().send(FrameRateMsg(FrameRate {
                        fps,
                        frame_time: avg_ms,
                        frame,
                    }));
                }

                frame += 1;
            }
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => {
                    log::trace!("WindowEvent::CloseRequested");
                    *control_flow = ControlFlow::Exit;
                }
                WindowEvent::Resized { .. } => {
                    dirty_swapchain = true;
                }
                _ => (),
            },
            Event::LoopDestroyed => {
                log::trace!("Event::LoopDestroyed");

                gfaestus.wait_gpu_idle().unwrap();

                let device = gfaestus.vk_context().device();

                main_view.selection_buffer.destroy(device);
                main_view.node_id_buffer.destroy(device);
                main_view.node_draw_system.destroy(&gfaestus);

                gui.draw_system.destroy(&gfaestus.allocator);

                selection_edge.destroy(device);
                selection_blur.destroy(device);
            }
            _ => (),
        }
    });
}

fn handle_new_overlay(
    app: &GfaestusVk,
    main_view: &mut MainView,
    node_count: usize,
    msg: OverlayCreatorMsg,
) -> Result<()> {
    let OverlayCreatorMsg::NewOverlay { name, data } = msg;

    let overlay = match data {
        OverlayData::RGB(data) => {
            let mut overlay =
                Overlay::new_empty_rgb(&name, app, node_count).unwrap();

            overlay
                .update_rgb_overlay(
                    data.iter()
                        .enumerate()
                        .map(|(ix, col)| (NodeId::from((ix as u64) + 1), *col)),
                )
                .unwrap();

            overlay
        }
        OverlayData::Value(data) => {
            let mut overlay =
                Overlay::new_empty_value(&name, &app, node_count).unwrap();

            overlay
                .update_value_overlay(
                    data.iter()
                        .enumerate()
                        .map(|(ix, v)| (NodeId::from((ix as u64) + 1), *v)),
                )
                .unwrap();

            overlay
        }
    };

    main_view.node_draw_system.pipelines.create_overlay(overlay);

    Ok(())
}

fn create_overlay(
    app: &GfaestusVk,
    main_view: &mut MainView,
    reactor: &Reactor,
    name: &str,
    script: &str,
) -> Result<()> {
    let node_count = reactor.graph_query.graph.node_count();

    let script_config = gfaestus::script::ScriptConfig {
        default_color: rgb::RGBA::new(0.3, 0.3, 0.3, 0.3),
        target: gfaestus::script::ScriptTarget::Nodes,
    };

    if let Ok(data) = gfaestus::script::overlay_colors_tgt(
        &reactor.rayon_pool,
        &script_config,
        &reactor.graph_query,
        script,
    ) {
        let msg = OverlayCreatorMsg::NewOverlay {
            name: name.to_string(),
            data,
        };
        handle_new_overlay(app, main_view, node_count, msg)?;
    }

    Ok(())
}

fn draw_tree<T>(ctx: &egui::CtxRef, tree: &QuadTree<T>, app: &App)
where
    T: Clone + ToString,
{
    let view = app.shared_state().view();
    let s = app.shared_state().mouse_pos();
    let dims = app.dims();
    let w = view.screen_point_to_world(dims, s);

    for leaf in tree.leaves() {
        gfaestus::gui::text::draw_rect_world(ctx, view, leaf.boundary(), None);

        let points = leaf.points();
        let data = leaf.data();
        for (point, val) in points.into_iter().zip(data.into_iter()) {
            gfaestus::gui::text::draw_text_at_world_point(
                ctx,
                view,
                *point,
                &val.to_string(),
            );
        }
    }

    if let Some(closest) = tree.nearest_leaf(w) {
        let rect = closest.boundary();
        let color = rgb::RGBA::new(0.8, 0.1, 0.1, 1.0);
        gfaestus::gui::text::draw_rect_world(ctx, view, rect, Some(color));
    }
}

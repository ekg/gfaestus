use crossbeam::channel::{self, Receiver, Sender};
use winit::event::VirtualKeyCode;

use crate::app::mainview::MainViewMsg;
use crate::app::AppMsg;
use crate::gui::GuiMsg;

pub type BindMsg = (
    VirtualKeyCode,
    Option<Box<dyn Fn() + Send + Sync + 'static>>,
);

#[derive(Clone)]
pub struct AppChannels {
    pub app_tx: Sender<AppMsg>,
    pub app_rx: Receiver<AppMsg>,

    pub main_view_tx: Sender<MainViewMsg>,
    pub main_view_rx: Receiver<MainViewMsg>,

    pub gui_tx: Sender<GuiMsg>,
    pub gui_rx: Receiver<GuiMsg>,

    pub binds_tx: Sender<BindMsg>,
    pub binds_rx: Receiver<BindMsg>,
}

impl AppChannels {
    pub(super) fn new() -> Self {
        let (app_tx, app_rx) = channel::unbounded::<AppMsg>();
        let (main_view_tx, main_view_rx) = channel::unbounded::<MainViewMsg>();
        let (gui_tx, gui_rx) = channel::unbounded::<GuiMsg>();
        let (binds_tx, binds_rx) = channel::unbounded::<BindMsg>();

        Self {
            app_tx,
            app_rx,

            main_view_tx,
            main_view_rx,

            gui_tx,
            gui_rx,

            binds_tx,
            binds_rx,
        }
    }
}

use std::pin::Pin;
use std::sync::Arc;

use crossbeam::channel::{Receiver, Sender};
use futures::{future::RemoteHandle, task::SpawnExt, Future};

mod modal;
mod paired;

pub use modal::*;
pub use paired::{create_host_pair, Host, Inbox, Outbox, Processor};

use paired::*;

use crate::app::channels::OverlayCreatorMsg;
use crate::app::AppChannels;
use crate::graph_query::GraphQuery;

pub struct Reactor {
    thread_pool: futures::executor::ThreadPool,
    pub rayon_pool: Arc<rayon::ThreadPool>,

    pub graph_query: Arc<GraphQuery>,

    pub overlay_create_tx: Sender<OverlayCreatorMsg>,
    pub overlay_create_rx: Receiver<OverlayCreatorMsg>,

    pub future_tx:
        Sender<Pin<Box<dyn Future<Output = ()> + Send + Sync + 'static>>>,
    // pub future_tx: Sender<Box<dyn Future<Output = ()> + 'static>>,

    // pub future_tx: Sender<Box<dyn FnOnce() + Send + Sync + 'static>>,
    // pub task_rx: Receiver<Box<dyn FnOnce() + Send + Sync + 'static>>,
    _task_thread: std::thread::JoinHandle<()>,
}

impl Reactor {
    pub fn init(
        thread_pool: futures::executor::ThreadPool,
        rayon_pool: rayon::ThreadPool,
        graph_query: Arc<GraphQuery>,
        channels: &AppChannels,
    ) -> Self {
        let rayon_pool = Arc::new(rayon_pool);

        let (task_tx, task_rx) = crossbeam::channel::unbounded();

        let thread_pool_ = thread_pool.clone();

        let _task_thread = std::thread::spawn(move || {
            let thread_pool = thread_pool_;

            while let Ok(task) = task_rx.recv() {
                thread_pool.spawn(task).unwrap();
            }
        });

        Self {
            thread_pool,
            rayon_pool,

            graph_query,

            overlay_create_tx: channels.new_overlay_tx.clone(),
            overlay_create_rx: channels.new_overlay_rx.clone(),

            future_tx: task_tx,
            // task_rx,
            _task_thread,
        }
    }

    pub fn create_host<F, I, T>(&mut self, func: F) -> Host<I, T>
    where
        T: Send + Sync + 'static,
        I: Send + Sync + 'static,
        F: Fn(&Outbox<T>, I) -> T + Send + Sync + 'static,
    {
        let boxed_func = Box::new(func) as Box<_>;

        let (host, proc) = create_host_pair(boxed_func);

        let mut processor = Box::new(proc) as Box<dyn ProcTrait>;

        self.thread_pool
            .spawn(async move {
                log::debug!("spawning reactor task");

                loop {
                    let _result = processor.process().await;
                }
            })
            .expect("Error when spawning reactor task");

        host
    }

    /*
    pub fn spawn_interval<F>(
        &mut self,
        func: F,
        dur: std::time::Duration,
    ) -> anyhow::Result<RemoteHandle<()>>
    where
        F: Fn(f64) + Send + Sync + 'static,
    {
        use futures_timer::Delay;
        use std::time::{Duration, SystemTime};

        let result = self.thread_pool.spawn_with_handle(async move {
            let looper = || {
                let delay = Delay::new(dur);
                async {
                    delay.await;
                    let t = SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or(Duration::from_secs_f64(0.0))
                        .as_secs_f64();
                    func(t);
                }
            };

            loop {
                looper().await;
            }
        })?;
        Ok(result)
    }
    */

    pub fn spawn_interval<F>(
        &mut self,
        mut func: F,
        dur: std::time::Duration,
    ) -> anyhow::Result<RemoteHandle<()>>
    where
        F: FnMut() + Send + Sync + 'static,
    {
        use futures_timer::Delay;

        let result = self.thread_pool.spawn_with_handle(async move {
            /*
            let looper = || {
                let delay = Delay::new(dur);
                async {
                    delay.await;
                    func();
                }
            };
            */

            loop {
                let delay = Delay::new(dur);
                delay.await;
                func();
            }
        })?;
        Ok(result)
    }

    pub fn spawn<F, T>(&mut self, fut: F) -> anyhow::Result<RemoteHandle<T>>
    where
        F: Future<Output = T> + Send + Sync + 'static,
        T: Send + Sync + 'static,
    {
        let handle = self.thread_pool.spawn_with_handle(fut)?;
        Ok(handle)
    }

    pub fn spawn_forget<F>(&self, fut: F) -> anyhow::Result<()>
    where
        F: Future<Output = ()> + Send + Sync + 'static,
    {
        let fut = Box::pin(fut) as _;
        self.future_tx.send(fut)?;
        Ok(())
    }
}

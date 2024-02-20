// TODO does task really need to be wrapped in Arc?
//
use futures::{
    task::{waker_ref, ArcWake},
    Future,
};
use log::debug;
use std::{
    any::Any,
    cell::RefCell,
    pin::Pin,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

use crate::threadpool::{JoinHandle, ThreadPool};

thread_local! {
    static HANDLE: RefCell<Option<Handle>> = RefCell::new(None);
}

#[derive(Clone)]
pub struct Handle {
    task_sender: crossbeam_channel::Sender<Arc<Task<'static>>>,
    thread_pool: Arc<ThreadPool>,
    condvar: Arc<(Mutex<()>, Condvar)>,
}

impl Handle {
    fn new(
        task_sender: crossbeam_channel::Sender<Arc<Task<'static>>>,
        thread_pool: Arc<ThreadPool>,
        condvar: Arc<(Mutex<()>, Condvar)>,
    ) -> Self {
        Self {
            task_sender,
            thread_pool,
            condvar,
        }
    }

    /// Future is not needed to be Send since we're doing single threaded but
    /// the ArcWake trait requires it for more general use cases.
    pub fn spawn<R>(&self, future: impl Future<Output = R> + Send + 'static) -> JoinHandle<R>
    where
        R: Send + 'static,
    {
        let future = Box::pin(async {
            let boxed: Box<TaskResult> = Box::new(future.await);
            boxed
        });

        let (result_send, result_recv) = crossbeam_channel::bounded(1);

        let task = Arc::new(Task {
            future: Mutex::new(future),
            task_sender: self.task_sender.clone(),
            result_sender: Some(result_send),
            condvar: self.condvar.clone(),
        });

        self.task_sender.send(task).unwrap();

        self.condvar.1.notify_one();

        JoinHandle::new(result_recv)
    }

    pub fn spawn_blocking<F, R>(&self, task: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: std::any::Any + Send + 'static,
    {
        self.thread_pool.spawn_blocking(task)
    }

    pub fn block_on<R>(&self, future: impl Future<Output = R> + Send + 'static) -> R
    where
        R: Send + 'static,
    {
        self.spawn(future).join()
    }
}

pub fn current() -> Handle {
    HANDLE.with(|handle| {
        handle
            .borrow()
            .clone()
            .expect("The async runtime is None, maybe you forgot to make one")
    })
}

pub fn set_current(handle: Handle) {
    HANDLE.with(|h| {
        *h.borrow_mut() = Some(handle);
    });
}

pub fn new_runtime(num_worker: usize, max_blocking_threads: usize) -> Handle {
    let thread_pool = Arc::new(ThreadPool::new(max_blocking_threads + num_worker));

    let (global_send, global_recv) = crossbeam_channel::unbounded::<Arc<Task>>();

    let condvar = Arc::new((Mutex::new(()), Condvar::new()));

    let handle = Handle::new(global_send.clone(), thread_pool.clone(), condvar.clone());

    set_current(handle.clone());

    for _ in 0..num_worker {
        let executor = Worker::new(global_recv.clone(), condvar.clone());
        thread_pool.spawn_blocking(move || executor.run());
    }

    handle
}

struct Worker<'a> {
    local_queue: crossbeam_channel::Receiver<Arc<Task<'a>>>,
    global_queue: crossbeam_channel::Receiver<Arc<Task<'a>>>,
    // the task sender for this local queue
    task_sender: crossbeam_channel::Sender<Arc<Task<'a>>>,
    condvar: Arc<(Mutex<()>, Condvar)>,
}

// TODO implement lifetime correctly
impl Worker<'static> {
    fn new(
        global_queue: crossbeam_channel::Receiver<Arc<Task<'static>>>,
        condvar: Arc<(Mutex<()>, Condvar)>,
    ) -> Self {
        let (sender, queue) = crossbeam_channel::unbounded::<Arc<Task>>();

        Self {
            local_queue: queue,
            global_queue,
            task_sender: sender,
            condvar,
        }
    }

    fn run(&self) {
        // TODO since we're not using crossbeam channel's recv(), we don't get
        // the benefit of yielding the thread when the channel is empty.
        // Performance opportunities:
        // - [DONE] park the thread and use signal mechanism to wake up the thread when
        //   there's a new task
        // - implement or use crossbeam's Backoff to yield the thread or spin
        //   when the channel is empty, so that we don't have to park the thread
        //   prematurely.
        loop {
            let mut task: Option<Arc<Task<'static>>> = None;

            // TODO currently we're not spawning into the local queue so this
            // always returns err
            if let Ok(t) = self.local_queue.try_recv() {
                task = Some(t);
            } else if let Ok(t) = self.global_queue.try_recv() {
                // TODO consider changing the task_sender of the task to local
                // queue sender, so that any futures that this task spawns
                // get queued in the local queue.
                task = Some(t);
            } else {
                drop(task);
                let lock = self.condvar.0.lock().unwrap();
                drop(
                    self.condvar
                        .1
                        // We want the thread to wake up every 100ms to check if
                        // there are any tasks in the global queue. This is to
                        // prevent the thread from sleeping indefinitely when
                        // there are tasks in the global queue.
                        .wait_timeout(lock, Duration::from_millis(100))
                        .unwrap(),
                );
                continue;
            }

            if let Some(task) = task {
                debug!("got task from local queue, running it");
                let mut future = task.future.lock().unwrap();
                let waker = waker_ref(&task);
                let context = &mut std::task::Context::from_waker(&waker);

                match future.as_mut().poll(context) {
                    std::task::Poll::Pending => {
                        debug!("task not ready");
                    }
                    std::task::Poll::Ready(result) => {
                        debug!("task finished");
                        if let Some(result_sender) = &task.result_sender {
                            // ignore the error because there are cases
                            // where the caller doesn't need the JoinHandle
                            // thus it's dropped and the result channel is
                            // closed
                            let _ = result_sender.send(result);
                        }
                    }
                }
            }
        }
    }
}

type TaskResult = dyn Any + Send + 'static;

struct Task<'a> {
    future: Mutex<Pin<Box<dyn Future<Output = Box<TaskResult>> + Send>>>,
    task_sender: crossbeam_channel::Sender<Arc<Task<'a>>>,
    result_sender: Option<crossbeam_channel::Sender<Box<TaskResult>>>,
    condvar: Arc<(Mutex<()>, Condvar)>,
}

impl ArcWake for Task<'static> {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        debug!("waking task");
        let cloned = arc_self.to_owned();
        // TODO proper error handling
        arc_self.task_sender.send(cloned).unwrap();
        // TODO maybe implement a mechanism to check if there are idle threads
        // (i.e. num_idle_threads > 0) if not we don't need to notify. Also the
        // notify could be cheaper than manually checking the condition.
        arc_self.condvar.1.notify_one();
    }
}

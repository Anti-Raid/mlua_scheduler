use crate::{XRc, XRefCell};
use futures_util::StreamExt;
use mlua::IntoLua;
use std::collections::{BinaryHeap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

pub struct WaitingThread {
    thread: ThreadInfo,
    start: std::time::Instant,
    duration: std::time::Duration,
}

impl std::cmp::PartialEq for WaitingThread {
    fn eq(&self, other: &Self) -> bool {
        self.start + self.duration == other.start + other.duration
    }
}

impl std::cmp::Eq for WaitingThread {}

impl std::cmp::PartialOrd for WaitingThread {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::cmp::Ord for WaitingThread {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reverse order
        (other.start + other.duration).cmp(&(self.start + self.duration))
    }
}

pub struct DeferredThread {
    thread: ThreadInfo,
}

#[derive(Debug)]
pub struct ThreadInfo {
    pub thread: mlua::Thread,
    pub args: mlua::MultiValue,
}

pub struct AsyncThreadInfo {
    pub thread: mlua::Thread,
    #[cfg(feature = "send")]
    pub callback: Pin<Box<dyn (Future<Output = mlua::Result<mlua::MultiValue>>) + Send + Sync>>,
    #[cfg(not(feature = "send"))]
    pub callback: Pin<Box<dyn (Future<Output = mlua::Result<mlua::MultiValue>>)>>,
}

#[cfg(not(feature = "send"))]
pub trait SchedulerFeedback {
    /// Function that is called whenever a thread is added/known to the task manager
    ///
    /// Contains both the creator thread and the thread that was added
    fn on_thread_add(
        &self,
        _label: &str,
        _creator: &mlua::Thread,
        _thread: &mlua::Thread,
    ) -> mlua::Result<()> {
        // Do nothing, unless overridden
        Ok(())
    }

    /// Function that is called when any response, Ok or Error occurs
    fn on_response(
        &self,
        _label: &str,
        _tm: &TaskManager,
        _th: &mlua::Thread,
        _result: Option<mlua::Result<mlua::MultiValue>>,
    ) {
        // Do nothing, unless overridden
    }
}

#[cfg(feature = "send")]
pub trait SchedulerFeedback: Send + Sync {
    /// Function that is called whenever a thread is added/known to the task manager
    ///
    /// Contains both the creator thread and the thread that was added
    fn on_thread_add(
        &self,
        _label: &str,
        _creator: &mlua::Thread,
        _thread: &mlua::Thread,
    ) -> mlua::Result<()> {
        // Do nothing, unless overridden
        Ok(())
    }

    /// Function that is called when any response, Ok or Error occurs
    fn on_response(
        &self,
        _label: &str,
        _tm: &TaskManager,
        _th: &mlua::Thread,
        _result: Option<mlua::Result<mlua::MultiValue>>,
    ) {
        // Do nothing, unless overridden
    }
}

/// Inner task manager state
pub struct TaskManagerInner {
    pub lua: mlua::Lua,
    pub pending_resumes: XRc<AtomicU64>,
    pub pending_asyncs: XRc<AtomicU64>,
    pub waiting_queue: XRefCell<BinaryHeap<WaitingThread>>,
    pub deferred_queue: XRefCell<VecDeque<DeferredThread>>,
    pub is_running: AtomicBool,
    pub feedback: XRc<dyn SchedulerFeedback>,
    pub async_task_executor: XRefCell<tokio::task::JoinSet<()>>,
}

#[derive(Clone)]
/// Task Manager
pub struct TaskManager {
    pub inner: XRc<TaskManagerInner>,
}

impl TaskManager {
    /// Creates a new task manager
    pub fn new(lua: mlua::Lua, feedback: XRc<dyn SchedulerFeedback>) -> Self {
        Self {
            inner: TaskManagerInner {
                pending_resumes: XRc::new(AtomicU64::new(0)),
                pending_asyncs: XRc::new(AtomicU64::new(0)),
                waiting_queue: XRefCell::new(BinaryHeap::default()),
                deferred_queue: XRefCell::new(VecDeque::default()),
                is_running: AtomicBool::new(false),
                feedback,
                lua,
                async_task_executor: XRefCell::new(tokio::task::JoinSet::new()),
            }
            .into(),
        }
    }

    /// Attaches the task manager to the lua state
    pub fn attach(&self) {
        self.inner.lua.set_app_data(self.clone());

        // Also save error userdata
        let error_userdata = ErrorUserdata {}.into_lua(&self.inner.lua).unwrap();
        self.inner
            .lua
            .set_app_data(ErrorUserdataValue(error_userdata));
    }

    /// Returns whether the task manager is running
    pub fn is_running(&self) -> bool {
        self.inner.is_running.load(Ordering::Acquire)
    }

    /// Returns the feedback stored in the task manager
    pub fn feedback(&self) -> XRc<dyn SchedulerFeedback> {
        self.inner.feedback.clone()
    }

    /// Resumes a thread to next
    pub async fn resume_thread(
        &self,
        label: &str,
        thread: mlua::Thread,
        args: mlua::MultiValue,
    ) -> Option<mlua::Result<mlua::MultiValue>> {
        log::debug!("StartResumeThread: {}", label);

        #[cfg(feature = "send")]
        self.inner.pending_resumes.fetch_add(1, Ordering::AcqRel);
        #[cfg(not(feature = "send"))]
        self.inner.pending_resumes.fetch_add(1, Ordering::Relaxed);

        let mut async_thread = thread.into_async::<mlua::MultiValue>(args);

        let next = async_thread.next().await;

        #[cfg(feature = "send")]
        self.inner.pending_resumes.fetch_sub(1, Ordering::AcqRel);
        #[cfg(not(feature = "send"))]
        self.inner.pending_resumes.fetch_sub(1, Ordering::Relaxed);

        log::debug!("EndResumeThread {}", label);

        next
    }

    /// Resumes a thread to next and sends feedback through the scheduler feedback
    pub async fn resume_thread_and_send_feedback(
        &self,
        label: &str,
        thread: mlua::Thread,
        args: mlua::MultiValue,
    ) {
        let result = self.resume_thread(label, thread.clone(), args).await;
        self.inner
            .feedback
            .on_response(label, self, &thread, result);
    }

    /// Adds a waiting thread to the task manager
    pub fn add_waiting_thread(
        &self,
        thread: mlua::Thread,
        args: mlua::MultiValue,
        duration: std::time::Duration,
    ) {
        log::debug!("Trying to add thread to waiting queue");
        let tinfo = ThreadInfo { thread, args };
        let mut self_ref = self.inner.waiting_queue.borrow_mut();
        self_ref.push(WaitingThread {
            thread: tinfo,
            start: std::time::Instant::now(),
            duration,
        });
        log::debug!("Added thread to waiting queue");
    }

    /// Removes a waiting thread from the task manager returning the number of threads removed
    pub fn remove_waiting_thread(&self, thread: &mlua::Thread) -> u64 {
        let mut self_ref = self.inner.waiting_queue.borrow_mut();

        let mut removed = 0;
        self_ref.retain(|x| {
            if x.thread.thread != *thread {
                true
            } else {
                removed += 1;
                false
            }
        });

        removed
    }

    /// Adds a deferred thread to the task manager to the front of the queue
    pub fn add_deferred_thread_front(&self, thread: mlua::Thread, args: mlua::MultiValue) {
        log::debug!("Adding deferred thread to queue");
        let tinfo = ThreadInfo { thread, args };
        let mut self_ref = self.inner.deferred_queue.borrow_mut();
        self_ref.push_front(DeferredThread { thread: tinfo });
        log::debug!("Added deferred thread to queue");
    }

    /// Adds a deferred thread to the task manager to the front of the queue
    pub fn add_deferred_thread_back(&self, thread: mlua::Thread, args: mlua::MultiValue) {
        log::debug!("Adding deferred thread to queue");
        let tinfo = ThreadInfo { thread, args };
        let mut self_ref = self.inner.deferred_queue.borrow_mut();
        self_ref.push_back(DeferredThread { thread: tinfo });
        log::debug!("Added deferred thread to queue");
    }

    /// Removes a deferred thread from the task manager returning the number of threads removed
    pub fn remove_deferred_thread(&self, thread: &mlua::Thread) -> u64 {
        let mut self_ref = self.inner.deferred_queue.borrow_mut();

        let mut removed = 0;
        self_ref.retain(|x| {
            if x.thread.thread != *thread {
                true
            } else {
                removed += 1;
                false
            }
        });

        removed
    }

    /// Runs the task manager
    pub async fn run(&self, sleep_interval: Duration) -> Result<(), mlua::Error> {
        self.inner.is_running.store(true, Ordering::Relaxed);

        //log::debug!("Task Manager started");

        loop {
            if !self.is_running() {
                break;
            }

            //log::debug!("Processing task manager");
            self.process().await?;

            tokio::time::sleep(sleep_interval).await;
        }

        Ok(())
    }

    /// Processes the task manager
    pub async fn process(&self) -> Result<(), mlua::Error> {
        /*log::debug!(
            "Queue Length: {}, Running: {}",
            self.inner.len(),
            self.inner.is_running()
        );*/

        // Process all threads in the queue

        //log::debug!("Queue Length After Defer: {}", self.inner.len());
        {
            loop {
                // Pop element from self_ref
                let entry = {
                    let mut self_ref = self.inner.waiting_queue.borrow_mut();

                    let Some(entry) = self_ref.pop() else {
                        break;
                    };

                    entry
                };

                if let Some(entry) = self.process_waiting_thread(entry).await {
                    let mut self_ref = self.inner.waiting_queue.borrow_mut();
                    self_ref.push(entry);
                    break; // Because we have a binary heap, we can break here as order is maintained
                }
            }
        }

        {
            loop {
                // Pop element from self_ref
                let entry = {
                    let mut self_ref = self.inner.deferred_queue.borrow_mut();

                    let Some(entry) = self_ref.pop_back() else {
                        break;
                    };

                    entry
                };

                self.process_deferred_thread(entry).await;
            }
        }

        // Check all_threads, removing all finished threads and resuming all threads not in deferred_queue or waiting_queue

        Ok(())
    }

    /// Processes a deferred thread. Returns true if the thread is still running and should be readded to the list of deferred tasks
    async fn process_deferred_thread(&self, thread_info: DeferredThread) {
        /*
            if coroutine.status(data.thread) ~= "dead" then
               resume_with_error_check(data.thread, table.unpack(data.args))
            end
        */
        match thread_info.thread.thread.status() {
            mlua::ThreadStatus::Error | mlua::ThreadStatus::Finished => {}
            _ => {
                //log::debug!("Trying to resume deferred thread");
                let result = self
                    .resume_thread(
                        "DeferredThread",
                        thread_info.thread.thread.clone(),
                        thread_info.thread.args,
                    )
                    .await;

                self.inner.feedback.on_response(
                    "DeferredThread",
                    self,
                    &thread_info.thread.thread,
                    result,
                );
            }
        }
    }

    /// Processes a waiting thread
    async fn process_waiting_thread(&self, thread_info: WaitingThread) -> Option<WaitingThread> {
        /*
        if coroutine.status(thread) == "dead" then
        elseif type(data) == "table" and last_tick >= data.resume then
            if data.start then
                resume_with_error_check(thread, last_tick - data.start)
            else
                resume_with_error_check(thread, table.unpack(data, 1, data.n))
            end
        else
            waiting_threads[thread] = data
        end
                 */
        match thread_info.thread.thread.status() {
            mlua::ThreadStatus::Error | mlua::ThreadStatus::Finished => None,
            _ => {
                let start = thread_info.start;
                let duration = thread_info.duration;
                let current_time = std::time::Instant::now();

                if current_time - start >= duration {
                    log::debug!(
                        "Resuming waiting thread, start: {:?}, duration: {:?}, current_time: {:?}",
                        start,
                        duration,
                        current_time
                    );
                    // resume_with_error_check(thread, table.unpack(data, 1, data.n))
                    let mut args = thread_info.thread.args;
                    args.push_back(mlua::Value::Number((current_time - start).as_secs_f64()));
                    let result = self
                        .resume_thread("WaitingThread", thread_info.thread.thread.clone(), args)
                        .await;

                    self.inner.feedback.on_response(
                        "WaitingThread",
                        self,
                        &thread_info.thread.thread,
                        result,
                    );

                    None
                } else {
                    // Put thread back in queue
                    Some(thread_info)
                }
            }
        }
    }

    /// Stops the task manager
    pub fn stop(&self) {
        self.inner.is_running.store(false, Ordering::Relaxed);
    }

    /// Clears the task manager queues completely
    pub fn clear(&self) {
        self.inner.waiting_queue.borrow_mut().clear();
        self.inner.deferred_queue.borrow_mut().clear();
    }

    /// Returns the waiting queue length
    pub fn waiting_len(&self) -> usize {
        self.inner.waiting_queue.borrow().len()
    }

    /// Returns the deferred queue length
    pub fn deferred_len(&self) -> usize {
        self.inner.deferred_queue.borrow_mut().len()
    }

    /// Returns the pending resumes length
    pub fn pending_resumes_len(&self) -> usize {
        self.inner.pending_resumes.load(Ordering::Acquire) as usize
    }

    /// Returns the pending asyncs length
    pub fn pending_asyncs_len(&self) -> usize {
        self.inner.pending_asyncs.load(Ordering::Acquire) as usize
    }

    /// Returns the number of items in the whole queue
    pub fn len(&self) -> usize {
        self.waiting_len()
            + self.deferred_len()
            + self.pending_resumes_len()
            + self.pending_asyncs_len()
    }

    /// Returns if the queue is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Waits until the task manager is done
    pub async fn wait_till_done(&self, sleep_interval: Duration) {
        while self.is_running() {
            if self.is_empty() {
                break;
            }

            tokio::time::sleep(sleep_interval).await;
        }
    }
}

#[derive(Clone)]
pub struct ErrorUserdata {}

impl mlua::UserData for ErrorUserdata {}

pub struct ErrorUserdataValue(pub mlua::Value);

pub fn get(lua: &mlua::Lua) -> TaskManager {
    lua.app_data_ref::<TaskManager>()
        .expect("Failed to get task manager")
        .clone()
}

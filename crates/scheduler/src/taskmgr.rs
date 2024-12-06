use futures_util::StreamExt;

use crate::XRc;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

pub struct WaitingThread {
    thread: XRc<ThreadInfo>,
    start: u64,
    resume_ticks: u64,
}

pub struct DeferredThread {
    thread: XRc<ThreadInfo>,
}

#[derive(Debug)]
pub struct ThreadInfo {
    thread: mlua::Thread,
    args: mlua::MultiValue,
}

pub type OnError = fn(th: &ThreadInfo, mlua::Error) -> mlua::Result<()>;

/// Task Manager
pub struct TaskManager {
    pending_threads_count: XRc<AtomicU64>,
    waiting_queue: Mutex<VecDeque<XRc<WaitingThread>>>,
    deferred_queue: Mutex<VecDeque<XRc<DeferredThread>>>,
    is_running: AtomicBool,
    shutdown_requested: AtomicBool,
    processing: AtomicBool,
    ticks: AtomicU64,

    /// Function that is called when an error occurs
    ///
    /// If this function returns an error, the task manager will stop and error with the error
    on_error: OnError,
}

impl TaskManager {
    pub fn new(on_error: OnError) -> Self {
        Self {
            pending_threads_count: XRc::new(AtomicU64::new(0)),
            waiting_queue: Mutex::new(VecDeque::default()),
            deferred_queue: Mutex::new(VecDeque::default()),
            is_running: AtomicBool::new(false),
            shutdown_requested: AtomicBool::new(false),
            processing: AtomicBool::new(false),
            ticks: AtomicU64::new(0),
            on_error,
        }
    }

    /// Returns whether the task manager is running
    pub fn is_running(&self) -> bool {
        self.is_running.load(Ordering::Acquire)
    }

    /// Resumes a thread to next
    pub async fn resume_thread(
        &self,
        label: &str,
        thread: mlua::Thread,
        args: mlua::MultiValue,
    ) -> mlua::Result<mlua::MultiValue> {
        log::debug!("StartResumeThread: {}", label);
        let pending_count = self.pending_threads_count.clone();

        pending_count.fetch_add(1, Ordering::Relaxed);

        let mut async_thread = thread.clone().into_async::<mlua::MultiValue>(args.clone());

        let Some(next) = async_thread.next().await else {
            return Ok(mlua::MultiValue::new());
        };

        pending_count.fetch_sub(1, Ordering::Relaxed);

        tokio::task::yield_now().await;
        log::debug!("EndResumeThread {}", label);

        next
    }

    /// Resumes a thread to full
    pub async fn resume_thread_full(
        &self,
        thread: mlua::Thread,
        args: mlua::MultiValue,
    ) -> mlua::Result<mlua::MultiValue> {
        log::debug!("StartResumeThreadFull");
        let pending_count = self.pending_threads_count.clone();

        pending_count.fetch_add(1, Ordering::Relaxed);

        let mut async_thread = thread.into_async::<mlua::MultiValue>(args);

        let mut prev = Ok(mlua::MultiValue::new());
        loop {
            let Some(next) = async_thread.next().await else {
                break;
            };
            prev = next;
        }

        pending_count.fetch_sub(1, Ordering::Relaxed);

        tokio::task::yield_now().await;
        log::debug!("EndResumeThreadFull");
        prev
    }

    /// Adds a waiting thread to the task manager
    pub fn add_waiting_thread(
        &self,
        thread: mlua::Thread,
        args: mlua::MultiValue,
        resume_ticks: u64,
    ) {
        log::debug!("Trying to add thread to waiting queue");
        let tinfo = XRc::new(ThreadInfo { thread, args });
        let mut self_ref = self.waiting_queue.lock().unwrap();
        self_ref.push_front(XRc::new(WaitingThread {
            thread: tinfo,
            start: self.ticks.load(Ordering::Relaxed),
            resume_ticks,
        }));
        log::debug!("Added thread to waiting queue");
    }

    /// Adds a deferred thread to the task manager
    pub fn add_deferred_thread(&self, thread: mlua::Thread, args: mlua::MultiValue) {
        log::debug!("Adding deferred thread");
        let tinfo = XRc::new(ThreadInfo { thread, args });
        let mut self_ref = self.deferred_queue.lock().unwrap();
        self_ref.push_front(XRc::new(DeferredThread { thread: tinfo }));
    }

    /// Runs the task manager
    pub async fn run(&self, heartbeater: flume::Receiver<()>) -> Result<(), mlua::Error> {
        self.is_running.store(true, Ordering::Relaxed);

        //log::debug!("Task Manager started");

        loop {
            if !self.is_running() {
                break;
            }

            if self.shutdown_requested.load(Ordering::Relaxed) {
                self.is_running.store(false, Ordering::Relaxed);
                break;
            }

            //log::debug!("Processing task manager");
            self.processing.store(true, Ordering::Relaxed);
            self.process().await?;
            self.processing.store(false, Ordering::Relaxed);

            // Wait for next heartbeat to continue processing
            match heartbeater.recv_async().await {
                Ok(_) => {}
                Err(flume::RecvError::Disconnected) => {
                    break;
                }
            }

            // Increment tick count
            self.ticks.fetch_add(1, Ordering::Acquire);
        }

        Ok(())
    }

    pub async fn process(&self) -> Result<(), mlua::Error> {
        /*log::debug!(
            "Queue Length: {}, Running: {}",
            self.len(),
            self.is_running()
        );*/

        // Process all threads in the queue
        //
        // NOTE/DEVIATION FROM ROBLOX BEHAVIOUR: All threads are processed in a single event loop per heartbeat

        //log::debug!("Queue Length After Defer: {}", self.len());
        let mut readd_wait_list = Vec::new();
        {
            loop {
                // Pop element from self_ref
                let mut self_ref = self.waiting_queue.lock().unwrap();

                let Some(entry) = self_ref.pop_back() else {
                    break;
                };

                drop(self_ref);

                if self.process_waiting_thread(&entry).await? {
                    readd_wait_list.push(entry);
                }
            }
        }

        let mut readd_deferred_list = Vec::new();
        {
            loop {
                // Pop element from self_ref
                let mut self_ref = self.deferred_queue.lock().unwrap();

                let Some(entry) = self_ref.pop_back() else {
                    break;
                };

                drop(self_ref);

                if self.process_deferred_thread(&entry).await? {
                    readd_deferred_list.push(entry);
                }
            }
        }

        {
            // Readd threads that need to be re-added
            let mut self_ref = self.deferred_queue.lock().unwrap();
            for entry in readd_deferred_list {
                self_ref.push_back(entry);
            }

            drop(self_ref);

            // Readd threads that need to be re-added
            let mut self_ref = self.waiting_queue.lock().unwrap();
            for entry in readd_wait_list {
                self_ref.push_back(entry);
            }

            drop(self_ref);

            //log::debug!("Mutex unlocked");
        }

        // Check all_threads, removing all finished threads and resuming all threads not in deferred_queue or waiting_queue

        Ok(())
    }

    /// Processes a deferred thread. Returns true if the thread is still running and should be readded to the list of deferred tasks
    async fn process_deferred_thread(
        &self,
        thread_info: &XRc<DeferredThread>,
    ) -> mlua::Result<bool> {
        /*
            if coroutine.status(data.thread) ~= "dead" then
               resume_with_error_check(data.thread, table.unpack(data.args))
            end
        */
        match thread_info.thread.thread.status() {
            mlua::ThreadStatus::Error | mlua::ThreadStatus::Finished => Ok(false),
            _ => {
                //log::debug!("Trying to resume deferred thread");
                match self
                    .resume_thread(
                        "DeferredThread",
                        thread_info.thread.thread.clone(),
                        thread_info.thread.args.clone(),
                    )
                    .await
                {
                    Ok(_) => {
                        log::debug!("Deferred thread finished");
                        Ok(false)
                    }
                    Err(err) => {
                        (self.on_error)(&thread_info.thread, err)?;
                        Ok(false)
                    }
                }
            }
        }
    }

    /// Processes a waiting thread
    async fn process_waiting_thread(&self, thread_info: &XRc<WaitingThread>) -> mlua::Result<bool> {
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
        let start = thread_info.start;
        let resume_ticks = thread_info.resume_ticks;

        match thread_info.thread.thread.status() {
            mlua::ThreadStatus::Error | mlua::ThreadStatus::Finished => Ok(false),
            mlua::ThreadStatus::Running => Ok(true),
            mlua::ThreadStatus::Resumable => {
                let ticks = self.ticks.load(Ordering::Relaxed);
                if ticks > (resume_ticks + start) {
                    /*log::debug!(
                        "Resuming waiting thread, ticks: {}, resume_ticks: {}, start: {}",
                        ticks, resume_ticks, start
                    );*/
                    // resume_with_error_check(thread, table.unpack(data, 1, data.n))
                    match self
                        .resume_thread(
                            "WaitingThread",
                            thread_info.thread.thread.clone(),
                            thread_info.thread.args.clone(),
                        )
                        .await
                    {
                        Ok(_) => {
                            log::debug!("Waiting thread finished");
                            Ok(false)
                        }
                        Err(err) => {
                            (self.on_error)(&thread_info.thread, err)?;
                            Ok(false)
                        }
                    }
                } else {
                    // Put thread back in queue
                    Ok(true)
                }
            }
        }
    }

    /// Stops the task manager
    pub async fn stop(&self) {
        self.shutdown_requested.store(true, Ordering::Relaxed);

        while self.is_running() {
            tokio::task::yield_now().await;
        }
    }

    /// Returns the waiting queue length
    pub fn waiting_len(&self) -> usize {
        self.waiting_queue.lock().unwrap().len()
    }

    /// Returns the deferred queue length
    pub fn deferred_len(&self) -> usize {
        self.deferred_queue.lock().unwrap().len()
    }

    /// Returns the pending count length
    pub fn pending_len(&self) -> usize {
        self.pending_threads_count.load(Ordering::Relaxed) as usize
    }

    /// Returns the number of items in the whole queue
    pub fn len(&self) -> usize {
        self.waiting_len() + self.deferred_len() + self.pending_len()
    }

    /// Returns if the queue is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub async fn wait_till_done(&self) {
        while self.is_running() {
            if self.is_empty() && !self.processing.load(Ordering::Relaxed) {
                break;
            }
            tokio::task::yield_now().await;
        }
    }
}

pub fn add_scheduler(lua: &mlua::Lua, on_error: OnError) -> mlua::Result<XRc<TaskManager>> {
    let task_manager = XRc::new(TaskManager::new(on_error));
    lua.set_app_data(XRc::clone(&task_manager));
    Ok(task_manager)
}

pub fn get(lua: &mlua::Lua) -> XRc<TaskManager> {
    lua.app_data_ref::<XRc<TaskManager>>()
        .expect("Failed to get task manager")
        .clone()
}

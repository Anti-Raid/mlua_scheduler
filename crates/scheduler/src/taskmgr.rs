use crate::XRc;
use std::time::Duration;

pub trait SchedulerFeedback: crate::MaybeSend + crate::MaybeSync {
    /// Function that is called when any response, Ok or Error occurs
    fn on_response(
        &self,
        _label: &str,
        _th: &mlua::Thread,
        _result: mlua::Result<mlua::MultiValue>,
    ) {
        // Do nothing, unless overridden
    }
}

#[derive(Clone)]
/// Task Manager
pub struct TaskManager {
    #[cfg(feature = "v2_taskmgr")]
    pub(crate) inner: XRc<crate::taskmgr_v2::CoreScheduler>,
    #[cfg(not(feature = "v2_taskmgr"))]
    pub(crate) inner: XRc<crate::taskmgr_v1::TaskManagerInner>,
}

impl TaskManager {
    /// Creates a new task manager
    pub fn new(lua: &mlua::Lua, feedback: XRc<dyn SchedulerFeedback>) -> Self {
        #[cfg(feature = "v2_taskmgr")]
        {
            Self {
                inner: crate::taskmgr_v2::CoreScheduler::new(lua.weak(), feedback).into(),
            }
        }
        #[cfg(not(feature = "v2_taskmgr"))]
        {
            Self {
                inner: crate::taskmgr_v1::TaskManagerInner::new(lua, feedback).into(),
            }
        }
    }

    /// Tries to get strong ref to lua
    pub fn get_lua(&self) -> Option<mlua::Lua> {
        self.inner.lua.try_upgrade()
    }

    /// Attaches the task manager to the lua state. Note that run_in_task (etc.) must also be called
    pub fn attach(&self) -> Result<(), mlua::Error> {
        let Some(lua) = self.get_lua() else {
            return Err(mlua::Error::RuntimeError(
                "Failed to upgrade lua".to_string(),
            ));
        };
        lua.set_app_data(self.clone());
        Ok(())
    }

    /// Returns whether the task manager has been cancelled
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled.get()
    }

    /// Returns whether the task manager is running
    pub fn is_running(&self) -> bool {
        self.inner.is_running.get()
    }

    /// Returns the feedback stored in the task manager
    pub fn feedback(&self) -> &dyn SchedulerFeedback {
        &*self.inner.feedback
    }

    /// Adds a waiting thread to the task manager
    #[inline]
    pub fn add_waiting_thread(
        &self,
        thread: mlua::Thread,
        delay_args: Option<mlua::MultiValue>,
        duration: std::time::Duration,
    ) {
        #[cfg(feature = "v2_taskmgr")]
        {
            self.inner
                .push_event(crate::taskmgr_v2::SchedulerEvent::Wait {
                    delay_args,
                    thread,
                    duration,
                    start_at: std::time::Instant::now(),
                });
        }
        #[cfg(not(feature = "v2_taskmgr"))]
        {
            let op = match delay_args {
                Some(delay_args) => crate::taskmgr_v1::WaitOp::Delay { args: delay_args },
                None => crate::taskmgr_v1::WaitOp::Wait,
            };

            log::trace!("Trying to add thread to waiting queue");
            let mut self_ref = self.inner.waiting_queue.borrow_mut();
            let start = std::time::Instant::now();
            let wake_at = start + duration;
            self_ref.push(crate::taskmgr_v1::WaitingThread {
                thread,
                op,
                start,
                wake_at,
            });
            log::trace!("Added thread to waiting queue");
        }
    }

    /// Cancels a thread that is waiting in the task manager
    #[inline]
    pub fn cancel_task(&self, thread: &mlua::Thread) {
        #[cfg(feature = "v2_taskmgr")]
        {
            self.inner
                .cancel_task(crate::XId::from_ptr(thread.to_pointer()));
        }
        #[cfg(not(feature = "v2_taskmgr"))]
        {
            let mut self_ref = self.inner.waiting_queue.borrow_mut();

            self_ref.retain(|x| x.thread != *thread);

            drop(self_ref);

            let mut self_ref = self.inner.deferred_queue.borrow_mut();

            self_ref.retain(|x| x.thread != *thread);
        }
    }

    /// Adds a deferred thread to the task manager to the front of the queue
    #[inline]
    pub fn add_deferred_thread(&self, thread: mlua::Thread, args: mlua::MultiValue) {
        #[cfg(feature = "v2_taskmgr")]
        {
            self.inner
                .push_event(crate::taskmgr_v2::SchedulerEvent::DeferredThread { args, thread });
        }
        #[cfg(not(feature = "v2_taskmgr"))]
        {
            let mut self_ref = self.inner.deferred_queue.borrow_mut();
            self_ref.push_back(crate::taskmgr_v1::DeferredThread { thread, args });
        }
    }

    /// Runs the task manager
    ///
    /// Note that the scheduler will automatically schedule run to be called if needed
    #[inline]
    #[cfg(not(feature = "v2_taskmgr"))]
    async fn run(&self, ticker: tokio::sync::broadcast::Receiver<()>) {
        self.inner.run(ticker).await
    }

    /// Runs the task manager
    ///
    /// Note that the scheduler will automatically schedule run to be called if needed
    #[cfg(feature = "v2_taskmgr")]
    async fn run(&self) {
        self.inner.run().await
    }

    /// Helper method to start up the task manager
    /// from a synchronous context
    #[cfg(not(feature = "v2_taskmgr"))]
    pub fn run_in_task(&self, ticker: tokio::sync::broadcast::Receiver<()>) {
        if self.is_running() || self.is_cancelled() || !self.check_lua() {
            return;
        }

        log::debug!("Firing up task manager");

        let self_ref = self.clone();

        #[cfg(feature = "send")]
        tokio::task::spawn(async move {
            self_ref.run(ticker).await;
        });
        #[cfg(not(feature = "send"))]
        tokio::task::spawn_local(async move {
            self_ref.run(ticker).await;
        });
    }

    #[cfg(feature = "v2_taskmgr")]
    pub fn run_in_task(&self) {
        if self.is_running() || self.is_cancelled() || !self.check_lua() {
            return;
        }

        log::debug!("Firing up task manager");

        let self_ref = self.clone();

        #[cfg(feature = "send")]
        tokio::task::spawn(async move {
            self_ref.run().await;
        });
        #[cfg(not(feature = "send"))]
        tokio::task::spawn_local(async move {
            self_ref.run().await;
        });
    }

    /// Checks if the lua state is valid
    fn check_lua(&self) -> bool {
        self.inner.lua.try_upgrade().is_some()
    }

    /// Stops the task manager
    pub fn stop(&self) {
        self.inner.is_cancelled.set(true);
    }

    /// Unstops the task manager
    pub fn unstop(&self) {
        self.inner.is_cancelled.set(false);
    }

    /// Clears the task manager queues completely
    pub fn clear(&self) {
        #[cfg(feature = "v2_taskmgr")]
        {
            self.inner
                .push_event(crate::taskmgr_v2::SchedulerEvent::Clear {});
        }
        #[cfg(not(feature = "v2_taskmgr"))]
        {
            self.inner.waiting_queue.borrow_mut().clear();
            self.inner.deferred_queue.borrow_mut().clear();
        }
    }

    /// Waits until the task manager is done
    pub async fn wait_till_done(&self, sleep_interval: Duration) {
        #[cfg(feature = "v2_taskmgr")]
        {
            let _ = self.inner.wait_till_done().await;
        }
        #[cfg(not(feature = "v2_taskmgr"))]
        while !self.is_cancelled() {
            tokio::task::yield_now().await;
            println!("len: {}", self.inner.len());
            if self.inner.is_empty() {
                break;
            }

            tokio::time::sleep(sleep_interval).await;
        }
    }
}

pub fn get(lua: &mlua::Lua) -> mlua::AppDataRef<TaskManager> {
    lua.app_data_ref::<TaskManager>()
        .expect("Failed to get task manager")
}

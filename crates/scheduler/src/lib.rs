use smol::channel;

pub mod functions;

#[derive(Debug)]
struct ThreadInfo(mlua::Thread, mlua::MultiValue, SpawnProt);

#[derive(Debug)]
pub struct Scheduler {
    pub(crate) pool: (channel::Sender<ThreadInfo>, channel::Receiver<ThreadInfo>),

    // pub(crate) threads: Arc<Mutex<Vec<mlua::Thread>>>,
    pub errors: Vec<mlua::Error>,
}

pub fn setup_scheduler(lua: &mlua::Lua) {
    lua.set_app_data(Scheduler {
        pool: channel::unbounded(),
        errors: Default::default(),
    });
}

#[derive(Debug, Clone, Copy)]
pub enum SpawnProt {
    Spawn,
    Defer,
}

pub async fn spawn_local<A: mlua::IntoLuaMulti>(
    lua: &mlua::Lua,
    thread: mlua::Thread,
    prot: SpawnProt,
    args: A,
) -> mlua::Result<mlua::Thread> {
    let args = args.into_lua_multi(lua)?;

    let pool = {
        let scheduler = lua.app_data_ref::<Scheduler>().unwrap();
        scheduler.pool.0.clone()
    };

    let thread_inner = thread.clone();
    let args_inner = args.clone();

    pool.send(ThreadInfo(thread_inner, args_inner, prot))
        .await
        .expect("Failed to send thread to scheduler");

    if matches!(prot, SpawnProt::Spawn) {
        // poll immediately
        thread.resume::<()>(args)?;
    }

    Ok(thread)
}

async fn process_thread(thread_info: &ThreadInfo) {
    if let mlua::ThreadStatus::Resumable = thread_info.0.status() {
        // poll thread
        if let Err(err) = thread_info.0.resume::<()>(thread_info.1.clone()) {
            eprintln!("{err}");
        }

        smol::future::yield_now().await;
    };
}

pub async fn await_scheduler(lua: &mlua::Lua) -> Scheduler {
    let pool = {
        let scheduler = lua.app_data_ref::<Scheduler>().unwrap();
        scheduler.pool.1.clone()
    };

    let mut threads: Vec<ThreadInfo> = Vec::new();

    'main: loop {
        for thread_info in threads.iter().filter(|x| matches!(x.2, SpawnProt::Spawn)) {
            process_thread(thread_info).await;
        }

        for thread_info in threads.iter().filter(|x| matches!(x.2, SpawnProt::Defer)) {
            process_thread(thread_info).await;
        }

        while let Ok(thread_info) = pool.try_recv() {
            threads.push(thread_info);
        }

        smol::future::yield_now().await;

        for thread_info in &threads {
            if let mlua::ThreadStatus::Resumable = thread_info.0.status() {
                continue 'main;
            }
        }

        break 'main;
    }

    lua.remove_app_data::<Scheduler>().unwrap()
}
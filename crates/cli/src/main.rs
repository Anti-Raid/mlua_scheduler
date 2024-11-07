use clap::Parser;
use mlua_scheduler::{scheduler::Scheduler, traits::LuaSchedulerMethods};
use smol::fs;
use std::path::PathBuf;

#[derive(Debug, Parser)]
struct Cli {
    path: PathBuf,
}

async fn spawn_script(lua: mlua::Lua, path: PathBuf) -> mlua::Result<mlua::Thread> {
    let chunk = lua
        .load(fs::read_to_string(&path).await?)
        .set_name(fs::canonicalize(&path).await?.to_string_lossy());

    lua.spawn_thread(
        lua.create_thread(chunk.into_function()?)?,
        mlua_scheduler::SpawnProt::Spawn,
        (),
    )
}

fn main() {
    let cli = Cli::parse();
    let lua = mlua::Lua::new();
    let scheduler = Scheduler::new().setup(&lua);

    mlua_task_std::inject_globals(&lua).unwrap();

    let thread =
        smol::block_on(spawn_script(lua.clone(), cli.path)).expect("Failed to spawn script");

    smol::spawn(async move {
        scheduler.run().await.expect("Scheduler failed");
    })
    .detach();

    if smol::block_on(lua.await_thread(thread)).is_ok() {
        std::process::exit(0);
    } else {
        std::process::exit(1);
    }
}
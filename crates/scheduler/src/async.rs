use mlua::prelude::*;

use crate::MaybeSync;

pub fn create_async_lua_function<A, F, R, FR>(lua: &Lua, func: F) -> LuaResult<LuaFunction>
where
    A: FromLuaMulti + mlua::MaybeSend + MaybeSync + 'static,
    F: Fn(Lua, A) -> FR + mlua::MaybeSend + MaybeSync + Clone + 'static,
    R: mlua::IntoLuaMulti + mlua::MaybeSend + MaybeSync + 'static,
    FR: futures_util::Future<Output = LuaResult<R>> + mlua::MaybeSend + MaybeSync + 'static,
{
    let func = lua
        .load(
            r#"
local luacall = ...

local function callback(...)
    luacall(coroutine.running(), ...)
    return coroutine.yield()
end

return callback
            "#,
        )
        .set_name("__sched_yield")
        .call::<LuaFunction>(lua.create_function(
            move |lua, (th, args): (LuaThread, LuaMultiValue)| {
                let func_ref = func.clone();
                let lua_fut = lua.clone();
                let fut = async move {
                    let args = A::from_lua_multi(args, &lua_fut)?;
                    let fut = (func_ref)(lua_fut.clone(), args);
                    let res = fut.await;

                    match res {
                        Ok(res) => res.into_lua_multi(&lua_fut),
                        Err(err) => Err(err),
                    }
                };

                let lua = lua.clone();

                let taskmgr = super::taskmgr::get(&lua);

                *taskmgr.inner.pending_asyncs.borrow_mut() += 1;

                let inner = taskmgr.inner.clone();
                let mut async_executor = inner.async_task_executor.borrow_mut();

                let fut = async move {
                    let res = fut.await;

                    match res {
                        Ok(res) => {
                            *taskmgr.inner.pending_asyncs.borrow_mut() -= 1;

                            let result = th.resume(res);

                            taskmgr.inner.feedback.on_response(
                                "AsyncThread",
                                &taskmgr,
                                &th,
                                result,
                            );
                        }
                        Err(err) => {
                            *taskmgr.inner.pending_asyncs.borrow_mut() -= 1;

                            let result = th.resume_error::<LuaMultiValue>(err.to_string());

                            taskmgr.inner.feedback.on_response(
                                "AsyncThread.Resume",
                                &taskmgr,
                                &th,
                                result,
                            );
                        }
                    }
                };

                #[cfg(not(feature = "send"))]
                async_executor.spawn_local(fut);
                #[cfg(feature = "send")]
                async_executor.spawn(fut);

                Ok(())
            },
        )?)?;

    Ok(func)
}

pub trait LuaSchedulerAsync {
    /**
     * Creates a scheduler-handled async function
     *
     * Note that while `create_async_function` can still be used. Functions that need to have Lua scheduler aware
     * characteristics should use this function instead.
     *
     * # Panics
     *
     * Panics if called outside of a running [`mlua_scheduler::TaskManager`].
     */
    fn create_scheduler_async_function<A, F, R, FR>(&self, func: F) -> LuaResult<LuaFunction>
    where
        A: FromLuaMulti + mlua::MaybeSend + MaybeSync + 'static,
        F: Fn(Lua, A) -> FR + mlua::MaybeSend + MaybeSync + Clone + 'static,
        R: mlua::IntoLuaMulti + mlua::MaybeSend + MaybeSync + 'static,
        FR: futures_util::Future<Output = LuaResult<R>> + mlua::MaybeSend + MaybeSync + 'static;
}

impl LuaSchedulerAsync for Lua {
    fn create_scheduler_async_function<A, F, R, FR>(&self, func: F) -> LuaResult<LuaFunction>
    where
        A: FromLuaMulti + mlua::MaybeSend + MaybeSync + 'static,
        F: Fn(Lua, A) -> FR + mlua::MaybeSend + MaybeSync + Clone + 'static,
        R: mlua::IntoLuaMulti + mlua::MaybeSend + MaybeSync + 'static,
        FR: futures_util::Future<Output = LuaResult<R>> + mlua::MaybeSend + MaybeSync + 'static,
    {
        create_async_lua_function(self, func)
    }
}

/*pub trait LuaSchedulerAsyncUserData<T> {
    fn add_async_method<M, A, MR, R>(&mut self, name: impl ToString, method: M)
    where
        T: 'static,
        M: Fn(Lua, mlua::UserDataRef<T>, A) -> MR + mlua::MaybeSend + 'static,
        A: FromLuaMulti,
        MR: std::future::Future<Output = mlua::Result<R>> + mlua::MaybeSend + 'static,
        R: IntoLuaMulti;
}

impl<T> LuaSchedulerAsyncUserData<T> for mlua::UserDataRegistry<T> {
    fn add_async_method<M, A, MR, R>(&mut self, name: impl ToString, method: M)
    where
        T: 'static,
        M: Fn(Lua, mlua::UserDataRef<T>, A) -> MR + mlua::MaybeSend + 'static,
        A: FromLuaMulti,
        MR: std::future::Future<Output = mlua::Result<R>> + mlua::MaybeSend + 'static,
        R: IntoLuaMulti,
    {
        self.add_method(name, move |lua, this, args| {
            let coroutine = lua.globals().raw_get::<LuaFunction>("coroutine")?;
        });
    }
}
*/

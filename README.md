# mlua_scheduler

A Roblox-like scheduler for mlua

## Features

- Correctness and performance are king. mlua_scheduler is orders of magnitude faster than Lune 0.8's scheduler as of time of writing.
- Simple with simple primitives for getting results out.
- Properly working ``coroutine.yield`` and ``coroutine.resume`` functions that produce equivalent (mostly) results in Roblox's own Luau + Task Scheduling code
- Both mlua non-send and send features supported (thanks to rust async wizardry).
- Custom async function handling that works with Lua's coroutine design paradigm without breaking on edge-cases.
- Simple callback API for controlling the scheduler (`SchedulerFeedback` trait)

## Deviations from Roblox's scheduler

- Thread arguments are not preloaded to the stack due to mlua limitations. This is normally not a problem but may cause side-effects. For example, see ``tests/jacktest2.luau``.

```lua
local task = task or require("@lune/task")
local thread = task.delay(1, function(foo)
    print(foo)
    print(coroutine.yield())
    print("c")
    print(coroutine.yield())
    print("e")
end, "b")

task.spawn(thread, "a")

task.delay(2, function()
    coroutine.resume(thread, "d")
end)
```

## Inner Workings

- Waiting tasks are stored in a priority queue (binary heap) hence allowing the scheduler to only process ready tasks.
- Deferred tasks and async tasks are stored in a standard VecDeque.
- When processing, the scheduler runs in the following order: async -> waiting -> deferred.
- For proper async support, you should use the schedulers provided ``async`` module (see below). Thanks to recent scheduler reworks, the schedulers async system is as fast or faster than mlua's ``create_async_function``. The best way to avoid using these types of functions is to entirely disable mlua async altogether in your ``features`` for mlua [omit async from features = ['mlua']].

## Fast Mode

At the expense of fully breaking mlua's async support (``create_async_function`` and co.)
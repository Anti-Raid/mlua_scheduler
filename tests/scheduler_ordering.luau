--!nolint UnknownGlobal

local nums = {}
local function insert(n: number)
	table.insert(nums, n)
	print(n)
end

insert(1)

-- Defer will run at the end of the resumption cycle, but without yielding
task.defer(function()
	insert(5)
end)

-- Spawn will instantly run up until the first yield, and must then be resumed manually ...
task.spawn(function()
	insert(2)
	coroutine.yield()
	error("unreachable code")
end)

-- ... unless calling functions created using `lua.create_async_function(...)`,
-- which will resume their calling thread with their result automatically
task.spawn(function()
	insert(3)
	_TEST_ASYNC_WORK(1)
	insert(6)
end)

insert(4)

return nums
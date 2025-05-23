local task = task or require("@lune/task")

local process = {
    os = "windows"
}

local stdio = {
    format = function(value: any): string
        return tostring(value)
    end
}

local function assertA(condition: boolean, message: string)
    if not condition then
        print(message)
    end
end

print("ABC")

-- NOTE: For now we don't test accuracy of waiting, the only thing
-- we guarantee is that task.wait waits for _at least_ the amount
-- of time given. Windows sleep is extremely inaccurate.
local TEST_ACCURACY = false

local EPSILON = if process.os == "windows" then 1_000 / 1_000 else 8 / 1_000

local function test(expected: number)
	local start = os.clock()
	local returned = task.wait(expected)
	if typeof(returned) ~= "number" then
		error(
			string.format(
				"Expected task.wait to return a number, got %s %s",
				typeof(returned),
				stdio.format(returned)
			),
			2
		)
	end
	local elapsed = os.clock() - start
	if elapsed < expected then
		print(
			string.format(
				"Expected task.wait to yield for at least %.3f seconds, yielded for %.3f seconds",
				expected,
				elapsed
			)
		)
	end
	if not TEST_ACCURACY then
		return
	end
	local difference = math.abs(elapsed - expected)
	if difference > EPSILON then
		print(
			string.format(
				"Elapsed time diverged too much from argument!"
					.. "\nGot argument of %.3fms and elapsed time of %.3fms"
					.. "\nGot maximum difference of %.3fms and real difference of %.3fms",
				expected * 1_000,
				elapsed * 1_000,
				EPSILON * 1_000,
				difference * 1_000
			)
		)
	end
end

local function measure(duration: number)
	for _ = 1, 5 do
		test(duration)
	end
end

print("DEF")

-- About 20ms is the shortest safe sleep time on Windows, but
-- Linux and macOS can do down to about 10ms or less safely
measure(if process.os == "windows" then 15 / 1_000 else 5 / 1_000)

print("GHI")

measure(1 / 60)
measure(1 / 30)
measure(1 / 20)
measure(1 / 10)

-- Wait should work in other threads

local flag: boolean = false
task.spawn(function()
	task.wait(0.1)
	flag = true
end)
assertA(not flag, "Wait failed while inside task-spawned thread (1)")
task.wait(0.2)
assertA(flag, "Wait failed while inside task-spawned thread (2)")

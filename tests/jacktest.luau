local thread = task.spawn(function()
    print("1")
    print(_TEST_ASYNC_WORK(1))
    print("4")
    print(coroutine.yield())
    print("6")
end)

print("2")
coroutine.resume(thread, "3")
print("5")

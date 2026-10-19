-- A slow tool: reports progress each second and stops early if the client
-- cancels the request.
return {
  name = "countdown",
  description = "Count down from N seconds, reporting progress",
  input = {
    type = "object",
    properties = {
      seconds = { type = "integer", minimum = 1, maximum = 30 },
    },
    required = { "seconds" },
  },
  run = function(args, ctx)
    local n = args.seconds
    for i = 1, n do
      ctx:progress(i, n, "tick " .. i)
      -- `sleep` is provided by the host and yields to the runtime, so the
      -- server stays responsive and the request can be cancelled meanwhile.
      sleep(1)
      if ctx:cancelled() then
        error({ kind = "cancelled", message = "stopped at " .. i })
      end
    end
    return { done = n }
  end,
}

-- A tool file returns one table: name, description, input (a JSON Schema
-- written as a Lua table), and run(args, ctx).
return {
  name = "greet",
  description = "Greet someone by name",
  input = {
    type = "object",
    properties = {
      name = { type = "string", minLength = 1, description = "Who to greet" },
    },
    required = { "name" },
  },
  -- `args` is the call's arguments, already checked against `input`.
  run = function(args, ctx)
    return { greeting = "hello, " .. args.name }
  end,
}

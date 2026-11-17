-- The reference app's command handlers: each says what reached it in the
-- status line. `arg.values` holds every input component's value by node
-- key; `arg.event` is what ran the command, if a component did.

function greet(arg)
  local name = arg.values.name
  if name == "" then
    return "type a name first"
  end
  return "hello, " .. name
end

function pick(arg)
  return "skin: " .. arg.event.value
end

function notify(arg)
  if arg.values.notify then
    return "will notify"
  end
  return "will not notify"
end

function save(arg)
  return "saved " .. arg.values.skin .. (arg.values.notify and ", notifying" or "")
end

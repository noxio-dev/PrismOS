-- SPDX-License-Identifier: GPL-3.0-or-later
-- Copyright (c) 2026 noxio-dev
--
-- Interactive loader prompt (the "OK" prompt of the FreeBSD loader).

local config = require("config")
local cli = {}

local history = {}
local commands = {}
local order = {}

local function define(name, help, fn)
    commands[name] = { help = help, fn = fn }
    order[#order + 1] = name
end

local function out(s) loader.write(s) end

-- Line editor: edit in place, arrows for history and cursor, returns the line.
local function readline(prompt)
    local buf, pos, hist = "", 0, #history + 1
    local function redraw()
        out("\r" .. prompt .. buf .. "\27[K")
        local back = #buf - pos
        if back > 0 then out("\27[" .. back .. "D") end
    end
    out(prompt)
    while true do
        local k = loader.getkey()
        if k == "enter" then
            out("\n")
            return buf
        elseif k == "backspace" then
            if pos > 0 then
                buf = buf:sub(1, pos - 1) .. buf:sub(pos + 1)
                pos = pos - 1
                redraw()
            end
        elseif k == "delete" then
            if pos < #buf then buf = buf:sub(1, pos) .. buf:sub(pos + 2); redraw() end
        elseif k == "left" then
            if pos > 0 then pos = pos - 1; out("\27[D") end
        elseif k == "right" then
            if pos < #buf then pos = pos + 1; out("\27[C") end
        elseif k == "home" or k == "ctrl-a" then
            pos = 0; redraw()
        elseif k == "end" or k == "ctrl-e" then
            pos = #buf; redraw()
        elseif k == "up" then
            if hist > 1 then hist = hist - 1; buf = history[hist]; pos = #buf; redraw() end
        elseif k == "down" then
            if hist < #history then hist = hist + 1; buf = history[hist]
            else hist = #history + 1; buf = "" end
            pos = #buf; redraw()
        elseif k == "ctrl-u" then
            buf, pos = "", 0; redraw()
        elseif k == "ctrl-c" then
            out("^C\n")
            return ""
        elseif k == "tab" then
            -- nothing to complete yet
        elseif k and #k == 1 then
            buf = buf:sub(1, pos) .. k .. buf:sub(pos + 1)
            pos = pos + 1
            if pos == #buf then out(k) else redraw() end
        end
    end
end

local function split(line)
    local args = {}
    for w in line:gmatch("%S+") do args[#args + 1] = w end
    return args
end

local function human(n)
    if n >= 1 << 30 then return ("%.1f GiB"):format(n / (1 << 30)) end
    if n >= 1 << 20 then return ("%.1f MiB"):format(n / (1 << 20)) end
    if n >= 1 << 10 then return ("%.1f KiB"):format(n / (1 << 10)) end
    return n .. " B"
end

local function page(lines)
    local rows = loader.size()
    local n = 0
    for _, l in ipairs(lines) do
        out(l .. "\n")
        n = n + 1
        if n >= rows - 2 then
            out("-- more (q to stop) --")
            local k = loader.getkey()
            out("\r\27[K")
            if k == "q" or k == "esc" then return end
            n = 0
        end
    end
end

define("help", "show this help", function()
    local lines = {}
    local names = {}
    for _, n in ipairs(order) do names[#names + 1] = n end
    table.sort(names)
    for _, n in ipairs(names) do
        lines[#lines + 1] = ("  %-10s %s"):format(n, commands[n].help)
    end
    page(lines)
end)

define("boot", "boot the kernel: boot [kernel-path] [options]", function(a)
    local path, opts = nil, {}
    for i, w in ipairs(a) do
        if i == 1 and w:sub(1, 1) == "/" then path = w else opts[#opts + 1] = w end
    end
    if path then loader.setenv("kernel", path) end
    local ok, err = config.boot(table.concat(opts, " "))
    if not ok then out("boot failed: " .. tostring(err) .. "\n") end
end)

define("set", "set a variable: set name=value | set name value", function(a)
    if #a == 0 then
        out("usage: set name=value\n")
        return
    end
    local name, value = a[1]:match("^([^=]+)=(.*)$")
    if not name then
        name = a[1]
        value = table.concat(a, " ", 2)
    elseif #a > 1 then
        value = value .. " " .. table.concat(a, " ", 2)
    end
    value = value:gsub('^"(.*)"$', "%1")
    loader.setenv(name, value)
end)

define("unset", "remove a variable", function(a)
    if not a[1] then out("usage: unset name\n") return end
    loader.unsetenv(a[1])
end)

define("show", "show variables: show [name]", function(a)
    if a[1] then
        local v = loader.getenv(a[1])
        out(v and (a[1] .. "=" .. v .. "\n") or "variable not set\n")
        return
    end
    local names = {}
    for n in pairs(loader.environ()) do names[#names + 1] = n end
    table.sort(names)
    local env = loader.environ()
    local lines = {}
    for _, n in ipairs(names) do lines[#lines + 1] = n .. "=" .. env[n] end
    page(lines)
end)

define("ls", "list a directory: ls [path]", function(a)
    local path = a[1] or "/"
    local list, err = loader.readdir(path)
    if not list then out(path .. ": " .. tostring(err) .. "\n") return end
    table.sort(list, function(x, y) return x.name:lower() < y.name:lower() end)
    local lines = {}
    for _, e in ipairs(list) do
        lines[#lines + 1] = e.dir and ("  %-30s <dir>"):format(e.name)
                                   or ("  %-30s %d"):format(e.name, e.size)
    end
    page(lines)
end)

define("more", "show a text file: more path", function(a)
    if not a[1] then out("usage: more path\n") return end
    local text, err = loader.readfile(a[1])
    if not text then out(a[1] .. ": " .. tostring(err) .. "\n") return end
    local lines = {}
    for l in (text .. "\n"):gmatch("(.-)\r?\n") do lines[#lines + 1] = l end
    page(lines)
end)

define("lsmem", "show the firmware memory map", function()
    local lines = { ("  %-18s %-12s %s"):format("start", "size", "type") }
    for _, e in ipairs(loader.memmap()) do
        lines[#lines + 1] = ("  0x%016x %-12s %s"):format(e.base, human(e.length), e.type)
    end
    page(lines)
end)

define("load", "load the kernel: load path", function(a)
    if a[1] then loader.setenv("kernel", a[1]) end
    local ok, err = config.load_kernel()
    if not ok then out("load failed: " .. tostring(err) .. "\n") end
end)

define("reload", "re-read loader.conf", function()
    local errors = config.load()
    out(errors == 0 and "configuration reloaded\n" or (errors .. " error(s)\n"))
end)

define("lua", "run Lua code: lua <chunk>", function(a, line)
    local src = line:match("^%s*%S+%s+(.*)$")
    if not src then out("usage: lua <chunk>\n") return end
    local fn, err = load("return " .. src, "=lua")
    if not fn then fn, err = load(src, "=lua") end
    if not fn then out(err .. "\n") return end
    local res = table.pack(pcall(fn))
    if not res[1] then out(tostring(res[2]) .. "\n") return end
    for i = 2, res.n do out(tostring(res[i]) .. "\t") end
    if res.n > 1 then out("\n") end
end)

define("version", "show versions", function()
    out(loader.version .. " (" .. loader.platform .. ", " .. loader.lua_version .. ")\n")
end)

define("menu", "go back to the boot menu", function() return "menu" end)
define("reboot", "restart the machine", function() loader.reboot() end)

-- Run the prompt. Returns "menu" when the user asks for the menu.
function cli.run()
    out("\27[0m\27[2J\27[H")
    out(loader.version .. "\n")
    out("Type 'help' for a list of commands, 'boot' to start the kernel.\n\n")
    while true do
        local line = readline("OK ")
        local args = split(line)
        if #args > 0 then
            history[#history + 1] = line
            local cmd = table.remove(args, 1)
            local c = commands[cmd]
            if not c then
                out("unknown command '" .. cmd .. "' (try 'help')\n")
            else
                local ok, res = pcall(c.fn, args, line)
                if not ok then out("error: " .. tostring(res) .. "\n")
                elseif res == "menu" then return "menu" end
            end
        end
    end
end

return cli

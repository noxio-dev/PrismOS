-- SPDX-License-Identifier: GPL-3.0-or-later
-- Copyright (c) 2026 noxio-dev
--
-- Configuration: loader.conf parsing, module list and boot sequence.

local config = {}

config.files = { "/boot/defaults/loader.conf", "/boot/loader.conf" }
config.order = {}      -- variable names in order of first appearance
local seen = {}

local function trim(s)
    return (s:gsub("^%s+", ""):gsub("%s+$", ""))
end

-- Parse one loader.conf text. Returns the number of errors.
function config.parse(text, origin)
    local errors = 0
    local n = 0
    for line in (text .. "\n"):gmatch("(.-)\r?\n") do
        n = n + 1
        line = trim(line)
        if line ~= "" and line:sub(1, 1) ~= "#" then
            local name, value = line:match("^([%w_%.%-]+)%s*=%s*(.*)$")
            if not name then
                print(("%s:%d: syntax error"):format(origin, n))
                errors = errors + 1
            else
                local quoted = value:match('^"(.-)"')
                if quoted then
                    value = quoted
                else
                    value = trim((value:gsub("%s*#.*$", "")))
                end
                loader.setenv(name, value)
                if not seen[name] then
                    seen[name] = true
                    config.order[#config.order + 1] = name
                end
            end
        end
    end
    return errors
end

function config.load()
    local errors = 0
    for _, path in ipairs(config.files) do
        local text = loader.readfile(path)
        if text then
            errors = errors + config.parse(text, path)
        end
    end
    return errors
end

function config.get(name, default)
    local v = loader.getenv(name)
    if v == nil or v == "" then return default end
    return v
end

function config.yes(name)
    local v = loader.getenv(name)
    if not v then return false end
    v = v:lower()
    return v == "yes" or v == "true" or v == "on" or v == "1"
end

-- Seconds before autoboot, nil to wait forever.
function config.autoboot_delay()
    local v = (loader.getenv("autoboot_delay") or "3"):lower()
    if v == "no" then return nil end
    local n = tonumber(v)
    if not n or n < 0 then return nil end
    return math.floor(n)
end

-- Modules requested by <name>_load="YES", in configuration order.
function config.modules()
    local list = {}
    for _, var in ipairs(config.order) do
        local name = var:match("^(.+)_load$")
        if name and config.yes(var) then
            list[#list + 1] = {
                name = name,
                path = config.get(name .. "_name", "/boot/modules/" .. name),
                type = config.get(name .. "_type", "raw"),
            }
        end
    end
    return list
end

-- Load the kernel and its modules. Returns true, or nil and a message.
function config.load_kernel()
    local path = config.get("kernel", "/boot/kernel/kernel")
    loader.write(("Loading kernel %s... "):format(path))
    local ok, err = loader.load_kernel(path)
    if not ok then
        loader.write("failed\n")
        return nil, err
    end
    local k = loader.kernel()
    loader.write(("%d KiB at 0x%x\n"):format(k.size // 1024, k.phys))
    for _, m in ipairs(config.modules()) do
        loader.write(("Loading module %s (%s)... "):format(m.name, m.path))
        ok, err = loader.load_module(m.path, m.type, m.name)
        if not ok then
            loader.write("failed\n")
            return nil, err
        end
        loader.write("ok\n")
    end
    return true
end

-- Load everything and jump to the kernel. Only returns on failure.
function config.boot(extra_options)
    local ok, err = config.load_kernel()
    if not ok then return nil, err end
    local options = config.get("kernel_options", "")
    if extra_options and extra_options ~= "" then
        options = (options .. " " .. extra_options):gsub("^%s+", "")
    end
    loader.write("Booting...\n")
    return loader.boot(options)
end

return config

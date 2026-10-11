-- SPDX-License-Identifier: GPL-3.0-or-later
-- Copyright (c) 2026 noxio-dev
--
-- Boot menu.  Draws with ANSI escapes only, so it looks the same on the
-- VGA console, the UEFI console and a serial terminal.  ASCII only.

local config = require("config")
local menu = {}

local ESC = "\27["
local color = true

local function sgr(code) return color and (ESC .. code .. "m") or "" end
local function at(row, col) return ESC .. row .. ";" .. col .. "H" end
local function out(s) loader.write(s) end

local logo = {
    [[ ____  ____  ___ ____  __  __   ___  ____  ]],
    [[|  _ \|  _ \|_ _/ ___||  \/  | / _ \/ ___| ]],
    [[| |_) | |_) || |\___ \| |\/| || | | \___ \ ]],
    [[|  __/|  _ < | | ___) | |  | || |_| |___) |]],
    [[|_|   |_| \_\___|____/|_|  |_| \___/|____/ ]],
}

menu.options = { verbose = false, single = false }

local function boot()
    local extra = {}
    if menu.options.verbose then extra[#extra + 1] = "-v" end
    if menu.options.single then extra[#extra + 1] = "-s" end
    out(sgr("0") .. ESC .. "2J" .. ESC .. "H")
    local ok, err = config.boot(table.concat(extra, " "))
    out(sgr("1;31") .. "\nBoot failed: " .. tostring(err) .. sgr("0") .. "\n")
    out("Press any key to return to the menu.\n")
    loader.getkey()
    return "redraw"
end

local function memmap()
    out(sgr("0") .. ESC .. "2J" .. ESC .. "H")
    out(sgr("1;37") .. "Firmware memory map" .. sgr("0") .. "\n\n")
    local total = 0
    for _, e in ipairs(loader.memmap()) do
        out(("0x%012x  %10d KiB  %s\n"):format(e.base, e.length // 1024, e.type))
        if e.type == "usable" then total = total + e.length end
    end
    out(("\nUsable: %d MiB\n\nPress any key."):format(total // (1 << 20)))
    loader.getkey()
    return "redraw"
end

local items = {
    { label = "Boot PrismOS [Enter]", key = "1", run = boot },
    { label = function() return "Boot verbose: " .. (menu.options.verbose and "On" or "Off") end,
      key = "2", run = function() menu.options.verbose = not menu.options.verbose return "redraw" end },
    { label = function() return "Boot single user: " .. (menu.options.single and "On" or "Off") end,
      key = "3", run = function() menu.options.single = not menu.options.single return "redraw" end },
    { label = "Memory map", key = "4", run = memmap },
    { label = "Escape to loader prompt", key = "5", run = function() return "cli" end },
    { label = "Reboot", key = "6", run = function() loader.reboot() end },
}

local function label(item)
    return type(item.label) == "function" and item.label() or item.label
end

local function draw(sel)
    local rows, cols = loader.size()
    out(sgr("0") .. ESC .. "?25l" .. ESC .. "2J")
    local x = math.max(1, (cols - #logo[1]) // 2)
    local rainbow = {
        "1;31",
        "1;33",
        "1;32",
        "1;36",
        "1;34",
    }

    for i, l in ipairs(logo) do
        out(at(1 + i, x) .. sgr(rainbow[i]) .. l .. sgr("0"))
    end
    local title = config.get("loader_menu_title", "Welcome to PrismOS")
    out(at(8, math.max(1, (cols - #title) // 2)) .. sgr("1;37") .. title)

    local bw, bx = 44, math.max(1, (cols - 44) // 2)
    out(sgr("0;37") .. at(10, bx) .. "+" .. ("-"):rep(bw - 2) .. "+")
    for r = 1, #items + 2 do
        out(at(10 + r, bx) .. "|" .. (" "):rep(bw - 2) .. "|")
    end
    out(at(11 + #items + 2, bx) .. "+" .. ("-"):rep(bw - 2) .. "+")
    for i, item in ipairs(items) do
        local text = (" %s. %s"):format(item.key, label(item))
        text = text .. (" "):rep(bw - 4 - #text)
        out(at(11 + i, bx + 1))
        if i == sel then out(sgr("0;30;47") .. " " .. text:sub(1, bw - 3) .. sgr("0;37"))
        else out(" " .. text:sub(1, bw - 3)) end
    end
    out(sgr("0;37") .. at(rows - 2, 2) .. ("%s | %s | Lua %s"):format(loader.version, loader.platform:upper(), loader.lua_version:gsub("^Lua ", "")))
end

local function countdown_line(n)
    local rows = loader.size()
    out(at(rows - 4, 2) .. sgr("0;33") .. ESC .. "K")
    if n then
        out(("Autoboot in %d second%s: [Enter] boot now, [Esc] prompt, any other key stops."):format(n, n == 1 and "" or "s"))
    end
    out(sgr("0"))
end

-- Run the menu. Returns "cli" when the user wants the prompt.
function menu.run()
    color = config.yes("loader_color") or loader.getenv("loader_color") == nil
    local sel = 1
    draw(sel)

    local delay = config.autoboot_delay()
    if delay then
        local deadline = loader.time() + delay * 1000
        local last = -1
        while true do
            local left = math.ceil((deadline - loader.time()) / 1000)
            if left <= 0 then
                countdown_line(nil)
                boot()
                draw(sel)
                break
            end
            if left ~= last then countdown_line(left); last = left end
            local k = loader.getkey(100)
            if k == "enter" then boot(); draw(sel); break
            elseif k == "esc" then
                countdown_line(nil)
                out(ESC .. "?25h")
                return "cli"
            elseif k then countdown_line(nil); break end
        end
    end

    while true do
        local k = loader.getkey()
        local act
        if k == "up" then sel = (sel - 2) % #items + 1; draw(sel)
        elseif k == "down" then sel = sel % #items + 1; draw(sel)
        elseif k == "enter" then act = items[sel].run()
        elseif k == "esc" then act = "cli"
        elseif k then
            for i, item in ipairs(items) do
                if item.key == k then sel = i; act = item.run() end
            end
        end
        if act == "cli" then
            out(ESC .. "?25h")
            return "cli"
        elseif act == "redraw" then
            draw(sel)
        end
    end
end

return menu

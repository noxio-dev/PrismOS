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
    {
        label = "Shutdown",
        key = "7",
        run = function()
            out(sgr("0") .. ESC .. "2J" .. ESC .. "H")
            out(sgr("1;37") .. "Shutting down PrismOS..." .. sgr("0") .. "\n")

            if loader.shutdown then
                loader.shutdown()
                out("\nShutdown failed or returned.\n")
            else
                out("\nShutdown is not supported by this bootloader.\n")
            end

            out("Press any key to return to the menu.")
            loader.getkey()
            return "redraw"
        end
    },
}

local function label(item)
    return type(item.label) == "function" and item.label() or item.label
end

-- ---------------------------------------------------------------------------
-- Animated rainbow logo
--
-- Instead of swapping 5 block colors per line, every character gets its own
-- hue taken from a smooth HSL rainbow wave that flows across the logo.  Two
-- render modes are available through config:
--
--   loader_anim        "wave" (default) | "rainbow" (old 5-color shift) | "off"
--   loader_anim_mode   "16" (default: plain SGR, works on VGA/UEFI/serial) |
--                      "256" (xterm palette, terminals only) |
--                      "truecolor" (24-bit, smoothest, modern terminals only)
--
-- NOTE: the QEMU/SeaBIOS VGA console and the UEFI console silently ignore
-- 38;5 and 38;2 sequences, which leaves garbage colors behind.  That is why
-- the default mode is "16": those SGR codes are supported everywhere.  Only
-- opt into 256/truecolor if you boot over a serial terminal that supports it.
--
-- The frame rate is ~20 fps (50 ms); tune with loader_anim_fps (ms per frame).
-- ---------------------------------------------------------------------------

local FRAME_MS = 50          -- ms between animation frames
local WAVE_SPEED = 12        -- deg of hue advance per frame (time flow)
local WAVE_SPREAD = 140      -- deg of hue across one logo line (spatial gradient)

-- HSL (h in degrees, s/l in 0..1) -> r,g,b in 0..255.
local function hsl_to_rgb(h, s, l)
    h = h % 360
    local a = s * math.min(l, 1 - l)
    local function f(n)
        local k = (n + h / 30) % 12
        return l - a * math.max(-1, math.min(math.min(k - 3, 9 - k), 1))
    end
    return math.floor(f(0) * 255 + 0.5), math.floor(f(8) * 255 + 0.5), math.floor(f(4) * 255 + 0.5)
end

-- A hue in degrees -> the closest xterm-256 color index, using a 24-stop
-- saturated rainbow slice of the xterm cube (no muddy grays).
local xterm_rainbow = {
    196, 202, 208, 214, 220, 226, 190, 154, 118, 82, 46, 47,
    48,  49,  50,  51,  45,  39,  33,  27,  21, 57, 93, 129,
}
local function hue_to_256(h)
    return xterm_rainbow[(math.floor((h % 360) / 15) % 24) + 1]
end

-- A hue in degrees -> one of 12 bright SGR codes (the "16" fallback).
local sgr_rainbow = {
    "1;31", "1;31", "1;33", "1;33", "1;33", "1;32", "1;32", "1;36",
    "1;36", "1;34", "1;34", "1;35",
}
local function hue_to_16(h)
    return sgr_rainbow[(math.floor((h % 360) / 30) % 12) + 1]
end

local anim_mode  -- "truecolor" | "256" | "16"
local frame = 0
local last_anim = 0

-- Foreground escape for a given hue, in the active render mode.
local function hue_sgr(h)
    if anim_mode == "truecolor" then
        local r, g, b = hsl_to_rgb(h, 1.0, 0.55)
        return "38;2;" .. r .. ";" .. g .. ";" .. b
    elseif anim_mode == "256" then
        return "38;5;" .. hue_to_256(h)
    else
        return hue_to_16(h)
    end
end

-- Draw one logo line as a per-character color wave.  The hue combines a
-- spatial gradient (left -> right across the line) with a temporal offset
-- shared by all lines, so the whole logo shimmers as one flowing wave.
-- Consecutive characters that resolve to the same color share one SGR
-- sequence (big win on serial links, and the norm in "16" mode).
local function draw_logo_line(i, l, x)
    out(at(1 + i, x))
    local cur
    for col = 1, #l do
        local c = l:sub(col, col)
        if c == " " then
            out(" ")
        else
            local hue = frame * WAVE_SPEED
                + ((col - 1) / #l) * WAVE_SPREAD
                + (i - 1) * 18
            local code = hue_sgr(hue)
            if code ~= cur then
                cur = code
                out(sgr(code))
            end
            out(c)
        end
    end
    out(sgr("0"))
end

-- Old-style animation: 5 solid colors shifting down one line per frame.
local rainbow = { "1;31", "1;33", "1;32", "1;36", "1;34" }
local function draw_logo_step()
    local _, cols = loader.size()
    local x = math.max(1, (cols - #logo[1]) // 2)
    for i, l in ipairs(logo) do
        out(at(1 + i, x) .. sgr(rainbow[(i + frame - 1) % #rainbow + 1]) .. l .. sgr("0"))
    end
end

local function draw_logo()
    if anim_mode == "off" then return end
    if anim_mode == "rainbow" then draw_logo_step() return end
    local _, cols = loader.size()
    local x = math.max(1, (cols - #logo[1]) // 2)
    for i, l in ipairs(logo) do
        draw_logo_line(i, l, x)
    end
end

local function tick_logo(now)
    if anim_mode == "off" then return end
    if now - last_anim >= FRAME_MS then
        last_anim = now
        frame = frame + 1
        draw_logo()
    end
end

local function draw(sel)
    local rows, cols = loader.size()
    out(sgr("0") .. ESC .. "?25l" .. ESC .. "2J")
    frame = 0
    draw_logo()
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

-- Resolve animation settings from config (with sane defaults).
local function setup_anim()
    local mode = (config.get("loader_anim") or "wave"):lower()
    if not color or mode == "off" then
        anim_mode = "off"
        return
    end
    anim_mode = mode
    if anim_mode == "wave" then
        local m = (config.get("loader_anim_mode") or "16"):lower()
        if m == "truecolor" or m == "256" or m == "16" then anim_mode = m end
    end
    local fps = tonumber(config.get("loader_anim_fps"))
    if fps and fps >= 10 and fps <= 200 then FRAME_MS = 1000 // fps end
end

-- Run the menu. Returns "cli" when the user wants the prompt.
function menu.run()
    color = config.yes("loader_color") or loader.getenv("loader_color") == nil
    setup_anim()
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
            tick_logo(loader.time())
            local k = loader.getkey(FRAME_MS)
            if k == "enter" then boot(); draw(sel); break
            elseif k == "esc" then
                countdown_line(nil)
                out(ESC .. "?25h")
                return "cli"
            elseif k then countdown_line(nil); break end
        end
    end

    while true do
        -- Poll with a timeout so the logo keeps animating while idle.
        local k = loader.getkey(FRAME_MS)
        if k == nil then
            tick_logo(loader.time())
        else
            local act
            if k == "up" then sel = (sel - 2) % #items + 1; draw(sel)
            elseif k == "down" then sel = sel % #items + 1; draw(sel)
            elseif k == "enter" then act = items[sel].run()
            elseif k == "esc" then act = "cli"
            else
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
end

return menu 
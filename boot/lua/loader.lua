-- SPDX-License-Identifier: GPL-3.0-or-later
-- Copyright (c) 2026 noxio-dev
--
-- /boot/lua/loader.lua: entry point run by the C loader.
--
-- Reads /boot/defaults/loader.conf and /boot/loader.conf, then either boots
-- straight away (beastie_disable="YES"), shows the menu with its autoboot
-- countdown, or drops to the loader prompt.

local config = require("config")

local errors = config.load()
if errors > 0 then
    print(("%d configuration error(s), see above"):format(errors))
end

local function prompt()
    local cli = require("cli")
    return cli.run()
end

if config.yes("beastie_disable") then
    local delay = config.autoboot_delay()
    if delay and delay > 0 then
        loader.write(("Autoboot in %d s, press any key for the prompt... "):format(delay))
        if loader.getkey(delay * 1000) then
            loader.write("\n")
            prompt()
        end
    end
    local ok, err = config.boot()
    loader.write("boot failed: " .. tostring(err) .. "\n")
    prompt()
    return
end

local menu = require("menu")
while true do
    local where = menu.run()
    if where == "cli" then
        prompt()
    end
end

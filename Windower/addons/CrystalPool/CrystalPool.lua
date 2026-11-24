_addon.name = 'CrystalPool'
_addon.author = 'John Doe'
_addon.version = '1.0.0'
_addon.commands = {'crystalpool'}

package.cpath = package.cpath .. ';' .. windower.addon_path .. 'libs/?.dll'
local reader_ok, reader = pcall(require, '_CrystalPool')
local verified = false
local overlay_requested = false
local announced = false
local stopped = false
local attempts = 0
local login_generation = 0
local poll_generation = 1

local function log_ready()
    return (windower.ffxi.get_info() or {}).logged_in
end

local function announce()
    if not log_ready() or announced then return end
    if verified then
        windower.add_to_chat(207,
            '[CrystalPool] 1024 MiB verified (Crystal Launcher); XIPivot loading requested.')
        announced = true
    elseif stopped then
        local detail = reader_ok and 'Crystal memory verification was not received.'
            or 'libs/_CrystalPool.dll could not be loaded.'
        windower.add_to_chat(167,
            '[CrystalPool] ' .. detail .. ' XIPivot was not enabled by this addon.')
        announced = true
    end
end

local function inspect()
    if verified then return true end
    if not reader_ok or type(reader) ~= 'function' then return false end
    local ok, result = pcall(reader)
    if not ok or result ~= true then return false end
    verified = true
    if not overlay_requested then
        overlay_requested = true
        windower.send_command('lua load XIPivot')
    end
    return true
end

local function poll(generation)
    if generation ~= poll_generation or stopped then return end
    attempts = attempts + 1
    if inspect() then
        announce()
        return
    end
    if not reader_ok or type(reader) ~= 'function' or attempts >= 480 then
        stopped = true
        announce()
        return
    end
    coroutine.schedule(function() poll(generation) end, 0.25)
end

local function on_login()
    login_generation = login_generation + 1
    local generation = login_generation
    announced = false
    coroutine.schedule(function()
        if generation ~= login_generation then return end
        inspect()
        announce()
    end, 2)
end

windower.register_event('login', on_login)
windower.register_event('logout', function()
    login_generation = login_generation + 1
    announced = false
end)
windower.register_event('addon command', function()
    announced = false
    if not inspect() then
        stopped = false
        attempts = 0
        poll_generation = poll_generation + 1
        poll(poll_generation)
    end
    if verified or stopped then
        announce()
    else
        windower.add_to_chat(207, '[CrystalPool] Waiting for Crystal memory verification.')
    end
end)

poll(poll_generation)
if log_ready() then on_login() end

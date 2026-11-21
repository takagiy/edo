local M = {}

---@class Point
---@field x number
local Point = {}

---@param x number
---@return Point
function Point.new(x)
    return setmetatable({ x = x }, Point)
end

-- The length.
function Point:len() return self.x end

local function helper(a, b)
    return a + b
end

---@param event string
local handler = function (event)
    print("got", event)
end

M.zero = function ()
    return Point.new(0)
end

function M.noop() end

vim.keymap.set("n", "x", function ()
    handler(helper(1, 2))
end)

return M

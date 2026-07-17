---@diagnostic disable: undefined-global
-- Headless Neovim probe for Java library goto-definition in a Gradle project.
--
-- Drives the real ktlsp server against a project (arg[1]) whose Java source imports an external
-- library symbol, and asserts that goto on that symbol jumps into the indexed library source.
--
-- nvim -l dev/nvim_java_library.lua <project-dir> <file.java> <token> [occurrence]
local project = arg[1]
local file = arg[2]
local token = arg[3]
local occurrence = tonumber(arg[4] or 1)
assert(project and project ~= "", "usage: nvim -l dev/nvim_java_library.lua <project-dir> <file.java> <token> [occurrence]")
assert(file and file ~= "", "usage: nvim -l dev/nvim_java_library.lua <project-dir> <file.java> <token> [occurrence]")
assert(token and token ~= "", "usage: nvim -l dev/nvim_java_library.lua <project-dir> <file.java> <token> [occurrence]")

local bin = os.getenv("KTLSP_BIN")
if not bin or bin == "" then
    bin = vim.fn.getcwd() .. "/target/release/ktlsp"
    if vim.fn.filereadable(bin) == 0 then
        bin = vim.fn.getcwd() .. "/target/debug/ktlsp"
    end
end
assert(vim.fn.filereadable(bin) == 1, "ktlsp binary not found: " .. bin)
assert(vim.fn.filereadable(file) == 1, "probe file not readable: " .. file)

vim.cmd("edit " .. vim.fn.fnameescape(file))
local bufnr = vim.api.nvim_get_current_buf()
vim.bo[bufnr].filetype = "java"

local id = vim.lsp.start({ name = "ktlsp", cmd = { bin }, root_dir = project })
assert(id, "vim.lsp.start returned nil")

vim.wait(25000, function()
    local c = vim.lsp.get_client_by_id(id)
    return c ~= nil and c.server_capabilities ~= nil and c.server_capabilities.definitionProvider ~= nil
end, 500)

local client = vim.lsp.get_client_by_id(id)
assert(client, "client did not attach")
assert(client.server_capabilities.definitionProvider, "server does not advertise definitionProvider")

local function definition_for(needle, n)
    n = n or 1
    local lines = vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)
    local count = 0
    local row, col
    for r, line in ipairs(lines) do
        local start = 1
        while true do
            local s, e = line:find(needle, start, true)
            if not s then break end
            count = count + 1
            if count == n then
                row = r - 1
                col = s - 1
                break
            end
            start = e + 1
        end
        if row then break end
    end
    assert(row, "token " .. needle .. " occurrence " .. n .. " not found")
    local params = vim.lsp.util.make_position_params(0, client.offset_encoding)
    params.position = { line = row, character = col }
    local result = vim.lsp.buf_request_sync(bufnr, "textDocument/definition", params, 10000)
    if not result then return nil end
    for _, resp in pairs(result) do
        if resp.result and #resp.result > 0 then
            return resp.result[1]
        end
    end
    return nil
end

local loc = definition_for(token, occurrence)
if loc and loc.uri:match(token .. "%.java$") then
    local short = loc.uri:gsub("^.*/ktlsp%-harness/", ".../ktlsp-harness/")
    print("PASS goto " .. token .. " -> " .. short .. ":" .. loc.range.start.line)
    if client then
        pcall(function() client:stop(true) end)
    end
    vim.wait(300)
    os.exit(0)
else
    print("FAIL goto " .. token .. " -> " .. vim.inspect(loc))
    if client then
        pcall(function() client:stop(true) end)
    end
    vim.wait(300)
    os.exit(1)
end

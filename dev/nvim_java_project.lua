---@diagnostic disable: undefined-global
-- Headless Neovim probe for Java library goto in a real Gradle project.
--
--     nvim -l dev/nvim_java_project.lua <project-dir> <file.java> <token> [occurrence]

local project = arg[1]
local file = arg[2]
local token = arg[3] or "ConfigurationProperties"
local occurrence = tonumber(arg[4] or "1")

assert(project and project ~= "", "usage: nvim_java_project.lua <project-dir> <file.java> <token> [occurrence]")
assert(file and file ~= "", "usage: nvim_java_project.lua <project-dir> <file.java> <token> [occurrence]")

local bin = vim.fn.getcwd() .. "/target/release/ktlsp"
if vim.fn.filereadable(bin) == 0 then
  bin = vim.fn.getcwd() .. "/target/debug/ktlsp"
end
assert(vim.fn.filereadable(bin) == 1, "ktlsp binary not found")
assert(vim.fn.filereadable(file) == 1, "probe file not readable: " .. file)

vim.cmd("edit " .. vim.fn.fnameescape(file))
local bufnr = vim.api.nvim_get_current_buf()
local detected = vim.filetype.match({ filename = file, buf = bufnr })
if detected then
  vim.bo[bufnr].filetype = detected
end

local id = vim.lsp.start({ name = "ktlsp", cmd = { bin }, root_dir = project })
assert(id, "vim.lsp.start returned nil")
local ready = vim.wait(15000, function()
  local c = vim.lsp.get_client_by_id(id)
  return c ~= nil and (c.initialized == true or (c.server_capabilities and c.server_capabilities.definitionProvider))
end, 50)
assert(ready, "client did not initialize")

local function find_token()
  local lines = vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)
  local seen = 0
  for i, text in ipairs(lines) do
    local start = 1
    while true do
      local s = text:find(token, start, true)
      if not s then
        break
      end
      seen = seen + 1
      if seen == occurrence then
        return i - 1, s - 1
      end
      start = s + #token
    end
  end
  error("no `" .. token .. "` occurrence " .. occurrence .. " found in " .. file)
end

local line, col = find_token()
local params = {
  textDocument = { uri = vim.uri_from_bufnr(bufnr) },
  position = { line = line, character = col },
}

local loc
for i = 1, 300 do
  local r = vim.lsp.buf_request_sync(bufnr, "textDocument/definition", params, 1000) or {}
  for _, v in pairs(r) do
    if v.result then
      local result = v.result
      if result[1] then
        result = result[1]
      end
      if result.uri then
        loc = result
        break
      end
    end
  end
  if loc then
    break
  end
  vim.wait(200)
end

if loc then
  local short = loc.uri:gsub("^.*/ktlsp%-harness/", ".../ktlsp-harness/")
  print("PASS  goto " .. token .. " -> " .. short .. ":" .. loc.range.start.line)
else
  print("FAIL  goto " .. token .. " -> no result")
  os.exit(1)
end

local refs
for i = 1, 300 do
  local r = vim.lsp.buf_request_sync(bufnr, "textDocument/references", {
    textDocument = { uri = vim.uri_from_bufnr(bufnr) },
    position = { line = line, character = col },
    context = { includeDeclaration = true },
  }, 1000) or {}
  for _, v in pairs(r) do
    if v.result and #v.result >= 2 then
      refs = v.result
      break
    end
  end
  if refs then
    break
  end
  vim.wait(200)
end

if refs then
  print("PASS  references " .. token .. " -> " .. tostring(#refs) .. " results")
  os.exit(0)
else
  print("FAIL  references " .. token .. " -> no result")
  os.exit(1)
end

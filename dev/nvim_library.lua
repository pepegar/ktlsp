---@diagnostic disable: undefined-global
-- Headless Neovim test for *library* goto-definition.
--
-- Drives the real ktlsp server against a project (arg[1]) whose `gradle/libs.versions.toml`
-- declares kotlin-stdlib, and asserts that goto on a `listOf(...)` call jumps into the indexed
-- kotlin-stdlib source. Run via dev/smoke_library.sh (which sets up the temp project).
--
--     nvim -l dev/nvim_library.lua <project-dir>

local project = arg[1]
assert(project and project ~= "", "usage: nvim -l dev/nvim_library.lua <project-dir>")

local bin = vim.fn.getcwd() .. "/target/release/ktlsp"
if vim.fn.filereadable(bin) == 0 then
  bin = vim.fn.getcwd() .. "/target/debug/ktlsp"
end
assert(vim.fn.filereadable(bin) == 1, "ktlsp binary not found — run `cargo build` first")

vim.cmd("edit " .. vim.fn.fnameescape(project .. "/Main.kt"))
local bufnr = vim.api.nvim_get_current_buf()
vim.bo[bufnr].filetype = "kotlin"

local id = vim.lsp.start({ name = "ktlsp", cmd = { bin }, root_dir = project })
assert(id, "vim.lsp.start returned nil")
vim.wait(8000, function()
  local c = vim.lsp.get_client_by_id(id)
  return c ~= nil and (c.initialized == true or (c.server_capabilities and c.server_capabilities.definitionProvider))
end, 50)

-- Locate the `listOf` call site.
local lines = vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)
local line, col
for i, text in ipairs(lines) do
  local s = text:find("listOf", 1, true)
  if s then
    line, col = i - 1, s - 1
    break
  end
end
assert(line, "no `listOf` usage found in Main.kt")

-- Poll for the result: dependency indexing runs asynchronously after `initialize` (it may need to
-- download + extract + parse the stdlib sources), so retry until it warms up.
local params = { textDocument = { uri = vim.uri_from_bufnr(bufnr) }, position = { line = line, character = col } }
local loc
for _ = 1, 250 do
  local r = vim.lsp.buf_request_sync(bufnr, "textDocument/definition", params, 1000) or {}
  for _, v in pairs(r) do
    if v.result and (v.result.uri or v.result[1]) then
      loc = v.result
      if loc[1] then
        loc = loc[1]
      end
      break
    end
  end
  if loc then
    break
  end
  vim.wait(200)
end

if loc and loc.uri:match("kotlin%-stdlib") and loc.uri:match("%.kt$") then
  local short = loc.uri:gsub("^.*/%.cache/ktlsp/extracted/", "…/")
  print(("PASS  goto listOf -> %s:%d"):format(short, loc.range.start.line))
  os.exit(0)
else
  print("FAIL  goto listOf -> " .. vim.inspect(loc))
  print("(LSP log under: " .. vim.fn.stdpath("log") .. "/lsp.log)")
  os.exit(1)
end

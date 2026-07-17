---@diagnostic disable: undefined-global
-- Headless Neovim test for *library* goto-definition.
--
-- Drives the real ktlsp server against a project (arg[1]) whose `gradle/libs.versions.toml`
-- declares kotlin-stdlib, and asserts that goto on `listOf(...)` jumps into the JVM-specific
-- stdlib source set while `java.sql.Connection` jumps into indexed JDK source. Run via
-- dev/smoke_library.sh (which sets up the temp project).
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

local function find_token(token, occurrence)
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
  error(("no `%s` occurrence %d found in Main.kt"):format(token, occurrence))
end

local function definition_for(token, occurrence)
  local line, col = find_token(token, occurrence)
  local params = { textDocument = { uri = vim.uri_from_bufnr(bufnr) }, position = { line = line, character = col } }
  local loc
  -- Dependency and JDK indexing run asynchronously after `initialize`, so retry until warm.
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
  return loc
end

local function check_definition(desc, token, occurrence, predicate)
  local loc = definition_for(token, occurrence)
  if loc and predicate(loc) then
    local short = loc.uri:gsub("^.*/ktlsp%-harness/", ".../ktlsp-harness/")
    print(("PASS  %s -> %s:%d"):format(desc, short, loc.range.start.line))
    return true
  end
  print(("FAIL  %s -> %s"):format(desc, vim.inspect(loc)))
  return false
end

local ok = true
ok = check_definition("goto listOf", "listOf", 1, function(loc)
  return loc.uri:match("kotlin%-stdlib")
    and (loc.uri:match("jvmMain") or loc.uri:match("commonMain"))
    and loc.uri:match("%.kt$")
end) and ok
ok = check_definition("goto java.sql.Connection", "Connection", 3, function(loc)
  return loc.uri:match("Connection%.java$")
end) and ok

if ok then
  os.exit(0)
else
  print("(LSP log under: " .. vim.fn.stdpath("log") .. "/lsp.log)")
  os.exit(1)
end

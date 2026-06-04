---@diagnostic disable: undefined-global
-- Headless Neovim checks for S1 (incremental reparse via did_change) and S3 (references),
-- driven through Neovim's real LSP client against dev/sample. Run via dev/smoke_features.sh.
--
--     nvim -l dev/nvim_features.lua <project-dir>

local project = arg[1]
assert(project and project ~= "", "usage: nvim -l dev/nvim_features.lua <project-dir>")

local bin = vim.fn.getcwd() .. "/target/release/ktlsp"
if vim.fn.filereadable(bin) == 0 then
  bin = vim.fn.getcwd() .. "/target/debug/ktlsp"
end
assert(vim.fn.filereadable(bin) == 1, "ktlsp binary not found — run `cargo build` first")

local failures = {}
local function check(name, ok, detail)
  if ok then
    print(("PASS  %s"):format(name))
  else
    print(("FAIL  %s%s"):format(name, detail and ("  (" .. tostring(detail) .. ")") or ""))
    table.insert(failures, name)
  end
end

vim.cmd("edit " .. vim.fn.fnameescape(project .. "/Main.kt"))
local bufnr = vim.api.nvim_get_current_buf()
vim.bo[bufnr].filetype = "kotlin"

local id = vim.lsp.start({ name = "ktlsp", cmd = { bin }, root_dir = project })
assert(id, "vim.lsp.start returned nil")
vim.wait(8000, function()
  local c = vim.lsp.get_client_by_id(id)
  return c ~= nil and (c.initialized == true or (c.server_capabilities and c.server_capabilities.definitionProvider))
end, 50)

local client = vim.lsp.get_client_by_id(id)
check("advertises referencesProvider", client and client.server_capabilities.referencesProvider ~= nil)

-- locate (0-indexed) line/col of `token` on the buffer line containing `anchor`
local function find(anchor, token)
  local lines = vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)
  for i, line in ipairs(lines) do
    if line:find(anchor, 1, true) then
      local s = line:find(token, 1, true)
      if s then
        return i - 1, s - 1
      end
    end
  end
  return nil
end

local function request(method, params)
  local res
  vim.wait(4000, function()
    local r = vim.lsp.buf_request_sync(bufnr, method, params, 1000) or {}
    for _, v in pairs(r) do
      if v.result ~= nil then
        res = v.result
        return true
      end
    end
    return false
  end, 100)
  return res
end

local uri = vim.uri_from_bufnr(bufnr)

-- ---- S3: references on `helper` (declaration + the call in main) ----
do
  local l, c = find("println(helper())", "helper")
  check("found helper() usage", l ~= nil)
  if l then
    local res = request("textDocument/references", {
      textDocument = { uri = uri },
      position = { line = l, character = c },
      context = { includeDeclaration = true },
    })
    local n = res and #res or 0
    check("references(helper) returns >= 2 sites", n >= 2, "got " .. n)
    if res then
      check("  references are in Main.kt", vim.tbl_count(res) > 0 and res[1].uri:match("Main%.kt$") ~= nil, res[1] and res[1].uri)
    end
  end
end

-- ---- S1: edit the buffer (did_change -> incremental reparse), then goto on the new code ----
do
  local n = vim.api.nvim_buf_line_count(bufnr)
  -- append a fresh declaration + a call to it; this fires textDocument/didChange to ktlsp
  vim.api.nvim_buf_set_lines(bufnr, n, n, false, {
    "",
    "fun freshlyAdded(): Int = 7",
    "fun callsFresh() { freshlyAdded() }",
  })
  vim.wait(800) -- let change-tracking flush didChange and ktlsp reparse incrementally

  local l, c = find("fun callsFresh() { freshlyAdded() }", "freshlyAdded")
  check("found edited usage of freshlyAdded", l ~= nil)
  if l then
    local res = request("textDocument/definition", {
      textDocument = { uri = uri },
      position = { line = l, character = c },
    })
    local loc = res
    if type(res) == "table" and res[1] then
      loc = res[1]
    end
    check("goto after edit resolves freshlyAdded", loc ~= nil and loc.uri ~= nil)
    if loc and loc.range then
      -- the definition line is where `fun freshlyAdded` was inserted
      local dl = find("fun freshlyAdded(): Int = 7", "freshlyAdded")
      check("  resolves to the edited definition line", loc.range.start.line == dl, ("got %s want %s"):format(loc.range.start.line, dl))
    end
  end
end

pcall(function() client:stop(true) end)
vim.wait(300)

if #failures == 0 then
  print("\nALL PASS")
  os.exit(0)
else
  print("\nFAILED: " .. table.concat(failures, ", "))
  os.exit(1)
end

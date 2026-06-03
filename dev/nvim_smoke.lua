---@diagnostic disable: undefined-global
-- Headless Neovim integration smoke test for ktlsp.
--
-- Drives the REAL Neovim built-in LSP client against the ktlsp binary over stdio and asserts
-- goto-definition (local + cross-file). Run from the repo root:
--
--     nvim -l dev/nvim_smoke.lua
--
-- Exits 0 if all checks pass, 1 otherwise. No plugins, no user config involved.

local script = debug.getinfo(1, "S").source:gsub("^@", "")
local dev_dir = vim.fn.fnamemodify(script, ":p:h")
local root = vim.fn.fnamemodify(dev_dir, ":h")
local sample = dev_dir .. "/sample"

local bin = root .. "/target/release/ktlsp"
if vim.fn.filereadable(bin) == 0 then
  bin = root .. "/target/debug/ktlsp"
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

-- Open the entry file (sets the buffer name/uri and `kotlin` filetype).
local main = sample .. "/Main.kt"
vim.cmd("edit " .. vim.fn.fnameescape(main))
local bufnr = vim.api.nvim_get_current_buf()
-- In `nvim -l` script mode the FileType autocmd doesn't fire on :edit, so assert the actual
-- detection rule (what a real editor uses to auto-start the server) and set it on the buffer.
local detected = vim.filetype.match({ filename = main, buf = bufnr })
check("filetype detection: .kt -> kotlin", detected == "kotlin", detected)
if detected then
  vim.bo[bufnr].filetype = detected
end

-- Start ktlsp and attach to the current buffer.
local client_id = vim.lsp.start({ name = "ktlsp", cmd = { bin }, root_dir = sample })
assert(client_id, "vim.lsp.start returned nil")

-- Wait for the initialize handshake to actually complete (Neovim creates an empty
-- server_capabilities table before the response lands, so don't check it too early).
local ready = vim.wait(8000, function()
  local c = vim.lsp.get_client_by_id(client_id)
  if not c then
    return false
  end
  if c.initialized == true then
    return true
  end
  local caps = c.server_capabilities
  return caps ~= nil and caps.definitionProvider ~= nil
end, 50)
check("client attached & initialized", ready)

local client = vim.lsp.get_client_by_id(client_id)
if client then
  local caps = client.server_capabilities or {}
  local dp = caps.definitionProvider
  check("advertises definitionProvider", dp ~= nil and dp ~= false, vim.inspect(dp))
end

-- 0-indexed (line, col) of `token` on the buffer line containing `anchor`.
local function pos(anchor, token)
  local lines = vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)
  for i, line in ipairs(lines) do
    if line:find(anchor, 1, true) then
      local s = line:find(token, 1, true)
      if s then
        return i - 1, s - 1
      end
    end
  end
  error("anchor/token not found: " .. anchor .. " / " .. token)
end

-- Request definition, retrying so the server's async workspace scan can warm up (cross-file).
local function definition(line, col)
  local params = {
    textDocument = { uri = vim.uri_from_bufnr(bufnr) },
    position = { line = line, character = col },
  }
  for _ = 1, 25 do
    local r = vim.lsp.buf_request_sync(bufnr, "textDocument/definition", params, 1000) or {}
    for _, v in pairs(r) do
      if v.result and (v.result.uri or v.result[1]) then
        local loc = v.result
        if loc[1] then
          loc = loc[1]
        end
        return loc
      end
    end
    vim.wait(120)
  end
  return nil
end

-- 1) Local goto: helper() -> `fun helper` in Main.kt (line 2).
do
  local l, c = pos("println(helper())", "helper")
  local loc = definition(l, c)
  check("local goto: helper() resolves", loc ~= nil)
  if loc then
    check("  -> Main.kt", loc.uri:match("Main%.kt$") ~= nil, loc.uri)
    check("  -> line 2", loc.range.start.line == 2, loc.range and loc.range.start.line)
  end
end

-- 2) Cross-file goto: Greeter(...) -> `class Greeter` in Greeter.kt (line 2).
do
  local l, c = pos("val g = Greeter", "Greeter")
  local loc = definition(l, c)
  check("cross-file goto: Greeter resolves", loc ~= nil)
  if loc then
    check("  -> Greeter.kt", loc.uri:match("Greeter%.kt$") ~= nil, loc.uri)
    check("  -> line 2", loc.range.start.line == 2, loc.range and loc.range.start.line)
  end
end

if client then
  pcall(function() client:stop(true) end)
end
vim.wait(300)

if #failures == 0 then
  print("\nALL PASS")
  os.exit(0)
else
  print("\nFAILED: " .. table.concat(failures, ", "))
  print("(LSP log under: " .. vim.fn.stdpath("log") .. "/lsp.log)")
  os.exit(1)
end

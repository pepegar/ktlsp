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
check("advertises completionProvider", client and client.server_capabilities.completionProvider ~= nil)

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

-- ---- Stage A: scope/name completion offers a visible top-level name at an unqualified prefix ----
do
  -- Position the cursor one char into the `helper` token of the `println(helper())` call, so the
  -- completion prefix is `h` (an unqualified scope-name position).
  local l, c = find("println(helper())", "helper")
  check("found helper() call for completion", l ~= nil)
  if l then
    local res = request("textDocument/completion", {
      textDocument = { uri = uri },
      position = { line = l, character = c + 1 },
    })
    -- The result may be a CompletionList ({ items = {...} }) or a bare item array.
    local items = res and (res.items or res) or {}
    local has_helper = false
    for _, item in ipairs(items) do
      if item.label == "helper" then
        has_helper = true
        break
      end
    end
    check("completion offers `helper`", has_helper, "got " .. #items .. " items")
  end
end

-- ---- Stage C: member completion after `g.` offers Greeter members with a function snippet ----
do
  -- Position the cursor right after the `.` in `g.greet()` (the start of the `greet` selector).
  local l, c = find("g.greet()", "greet")
  check("found g.greet() for member completion", l ~= nil)
  if l then
    local res = request("textDocument/completion", {
      textDocument = { uri = uri },
      position = { line = l, character = c },
    })
    local items = res and (res.items or res) or {}
    local has_greet, has_potato = false, false
    local potato_item = nil
    for _, item in ipairs(items) do
      if item.label == "greet" then has_greet = true end
      if item.label == "potato" then
        has_potato = true
        potato_item = item
      end
    end
    check("member completion offers `greet`", has_greet, "got " .. #items .. " items")
    check("member completion offers `potato`", has_potato, "got " .. #items .. " items")
    if potato_item then
      -- `potato` is a zero-arg function: SNIPPET format (2), insertText `potato()$0`, kind FUNCTION (3).
      check("  potato insertTextFormat == Snippet (2)", potato_item.insertTextFormat == 2, tostring(potato_item.insertTextFormat))
      check("  potato insertText == potato()$0", potato_item.insertText ~= nil and potato_item.insertText:match("potato%(%)%$0") ~= nil, potato_item.insertText)
      check("  potato kind == Function (3)", potato_item.kind == 3, tostring(potato_item.kind))
    end
  end
end

-- ---- Stage C: an unimported, indexed type is offered WITH an auto-import additionalTextEdits ----
do
  -- `Decorator` is declared in dev/sample/Decor.kt under package `widgets` (no import in Main.kt),
  -- and referenced unimported below; completion must carry an `import widgets.Decorator` edit.
  local n0 = vim.api.nvim_buf_line_count(bufnr)
  vim.api.nvim_buf_set_lines(bufnr, n0, n0, false, {
    "",
    "fun tryAutoImport() { Decorat }",
  })
  vim.wait(800)
  local l, c = find("fun tryAutoImport() { Decorat }", "Decorat")
  check("found Decorat reference for auto-import", l ~= nil)
  if l then
    -- The cross-file `Decorator` symbol comes from the background project scan, which races with
    -- this request; poll the completion until it warms up (or time out).
    local decorator_item = nil
    local last_count = 0
    vim.wait(8000, function()
      local res = request("textDocument/completion", {
        textDocument = { uri = uri },
        position = { line = l, character = c + #"Decorat" },
      })
      local items = res and (res.items or res) or {}
      last_count = #items
      for _, item in ipairs(items) do
        if item.label == "Decorator" then
          decorator_item = item
          return true
        end
      end
      return false
    end, 200)
    check("completion offers `Decorator`", decorator_item ~= nil, "got " .. last_count .. " items")
    if decorator_item then
      local edits = decorator_item.additionalTextEdits
      local ok = edits ~= nil and edits[1] ~= nil and edits[1].newText ~= nil and edits[1].newText:match("^import ") ~= nil
      check("  Decorator carries an `import ` additionalTextEdit", ok, edits and edits[1] and edits[1].newText)
    end
  end
end

-- ---- S6: member access `g.greet()` resolves into Greeter.kt via the receiver's inferred type ----
do
  local l, c = find("g.greet()", "greet")
  check("found g.greet() member usage", l ~= nil)
  if l then
    local res = request("textDocument/definition", { textDocument = { uri = uri }, position = { line = l, character = c } })
    local loc = res
    if type(res) == "table" and res[1] then
      loc = res[1]
    end
    check("member goto g.greet -> Greeter.kt", loc ~= nil and loc.uri ~= nil and loc.uri:match("Greeter%.kt$") ~= nil, loc and loc.uri)
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

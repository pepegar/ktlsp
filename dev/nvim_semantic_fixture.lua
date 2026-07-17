---@diagnostic disable: undefined-global
-- Headless Neovim probe for representative Kotlin semantic shapes.
--
-- Checks:
-- - Result/onFailure/getOrThrow chain completion
-- - `this` receiver typing inside `apply`
-- - KMP source-set narrowing for goto-definition
-- - Constructor-property goto when the class header and `(` are split across lines
-- - Alias-backed nested implicit receivers for definition, hover, and completion

local root = arg[1]
assert(root and root ~= "", "usage: nvim -l dev/nvim_semantic_fixture.lua <project-dir> [ktlsp-bin]")

local bin = arg[2] or os.getenv("KTLSP_BIN")
if not bin or bin == "" then
  bin = vim.fn.getcwd() .. "/target/release/ktlsp"
  if vim.fn.filereadable(bin) == 0 then
    bin = vim.fn.getcwd() .. "/target/debug/ktlsp"
  end
end
assert(vim.fn.filereadable(bin) == 1, "ktlsp binary not found — run `cargo build` first")

local files = {
  accounts = root .. "/probe/accounts/AccountsProbe.kt",
  cleaner = root .. "/probe/cleaner/CleanerProbe.kt",
  invitations = root .. "/probe/invitations/InvitationsApi.kt",
  kmp = root .. "/feature/src/jvmMain/kotlin/probe/kmp/KmpProbe.kt",
}

local failures = {}
local function check(name, ok, detail)
  if ok then
    print(("PASS  %s"):format(name))
  else
    print(("FAIL  %s%s"):format(name, detail and ("  (" .. tostring(detail) .. ")") or ""))
    table.insert(failures, name)
  end
end

local id

local function open_file(path)
  vim.cmd("edit " .. vim.fn.fnameescape(path))
  local bufnr = vim.api.nvim_get_current_buf()
  vim.bo[bufnr].filetype = "kotlin"
  id = vim.lsp.start({ name = "ktlsp", cmd = { bin }, root_dir = root })
  return bufnr
end

local bufs = {}
for key, path in pairs(files) do
  bufs[key] = open_file(path)
end

assert(id, "vim.lsp.start returned nil")
vim.wait(10000, function()
  local c = vim.lsp.get_client_by_id(id)
  return c ~= nil and (c.initialized == true or (c.server_capabilities and c.server_capabilities.definitionProvider))
end, 50)

local client = vim.lsp.get_client_by_id(id)
check("advertises definitionProvider", client and client.server_capabilities.definitionProvider ~= nil)
check("advertises completionProvider", client and client.server_capabilities.completionProvider ~= nil)

local function find(bufnr, needle, token, use_last)
  local lines = vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)
  for i, line in ipairs(lines) do
    if line:find(needle, 1, true) then
      local s
      if use_last then
        local from = 1
        while true do
          local hit = line:find(token, from, true)
          if not hit then
            break
          end
          s = hit
          from = hit + 1
        end
      else
        s = line:find(token, 1, true)
      end
      if s then
        return i - 1, s - 1
      end
    end
  end
  return nil
end

local function request(bufnr, method, params)
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

local function completion_labels(bufnr, line, character)
  local res = request(bufnr, "textDocument/completion", {
    textDocument = { uri = vim.uri_from_bufnr(bufnr) },
    position = { line = line, character = character },
    context = { triggerKind = 1 },
  })
  local items = res and (res.items or res) or {}
  local labels = {}
  for _, item in ipairs(items) do
    labels[item.label] = true
  end
  return labels
end

local ready = vim.wait(10000, function()
  local b = open_file(files.accounts)
  local l, c = find(b, ".ema", "ema")
  if not l then
    return false
  end
  local labels = completion_labels(b, l, c + #"ema")
  return labels.email == true
end, 200)
check("semantic probe warmed", ready)

do
  local b = open_file(files.invitations)
  local l, c = find(b, "createInvitationUseCase.execute", "createInvitationUseCase")
  check("found split-header constructor property goto anchor", l ~= nil)
  if l then
    local res = request(b, "textDocument/definition", {
      textDocument = { uri = vim.uri_from_bufnr(b) },
      position = { line = l, character = c },
    })
    local loc = res
    if type(res) == "table" and res[1] then
      loc = res[1]
    end
    check(
      "split-header constructor property resolves to declaration",
      loc ~= nil
        and loc.uri ~= nil
        and loc.uri:match("probe/invitations/InvitationsApi%.kt$") ~= nil
        and loc.range.start.line == 15,
      vim.inspect(loc)
    )
  end
end

do
  local b = open_file(files.invitations)
  local l, c = find(b, "val resolvedCall = call", "call", true)
  check("found alias-backed implicit receiver call anchor", l ~= nil)
  if l then
    local params = {
      textDocument = { uri = vim.uri_from_bufnr(b) },
      position = { line = l, character = c },
    }
    local res = request(b, "textDocument/definition", params)
    local loc = res
    if type(res) == "table" and res[1] then
      loc = res[1]
    end
    check(
      "alias-backed outer receiver call resolves to Routing.kt",
      loc ~= nil and loc.uri ~= nil and loc.uri:match("probe/invitations/Routing%.kt$") ~= nil,
      vim.inspect(loc)
    )

    local hover = request(b, "textDocument/hover", params)
    check(
      "alias-backed outer receiver call has hover",
      hover ~= nil and vim.inspect(hover):match("call") ~= nil,
      vim.inspect(hover)
    )
  end
end

do
  local b = open_file(files.invitations)
  local l, c = find(b, "                ca", "ca")
  check("found alias-backed implicit receiver completion anchor", l ~= nil)
  if l then
    local labels = completion_labels(b, l, c + #"ca")
    check("alias-backed outer receiver completion offers call", labels.call == true, vim.inspect(labels))
  end
end

do
  local b = open_file(files.accounts)
  local l, c = find(b, ".ema", "ema")
  check("found AccountsProbe completion anchor", l ~= nil)
  if l then
    local labels = completion_labels(b, l, c + #"ema")
    check("Result chain completion offers email", labels.email == true, vim.inspect(labels))
  end
end

do
  local b = open_file(files.cleaner)
  local l, c = find(b, "this.s", "s", true)
  check("found CleanerProbe completion anchor", l ~= nil)
  if l then
    local labels = completion_labels(b, l, c + #"s")
    check("apply-this completion offers s", labels.s == true, vim.inspect(labels))
  end
end

do
  local b = open_file(files.accounts)
  local l, c = find(b, ".onFailure", "onFailure")
  check("found AccountsProbe goto anchor", l ~= nil)
  if l then
    local res = request(b, "textDocument/definition", {
      textDocument = { uri = vim.uri_from_bufnr(b) },
      position = { line = l, character = c },
    })
    local loc = res
    if type(res) == "table" and res[1] then
      loc = res[1]
    end
    check("goto onFailure -> Stdlib.kt", loc ~= nil and loc.uri ~= nil and loc.uri:match("Stdlib%.kt$") ~= nil, loc and loc.uri)
  end
end

do
  local b = open_file(files.kmp)
  local l, c = find(b, "Thing = Thing()", "Thing", true)
  check("found KmpProbe goto anchor", l ~= nil)
  if l then
    local res = request(b, "textDocument/definition", {
      textDocument = { uri = vim.uri_from_bufnr(b) },
      position = { line = l, character = c },
    })
    local loc = res
    if type(res) == "table" and res[1] then
      loc = res[1]
    end
    check(
      "KMP goto prefers jvmMain",
      loc ~= nil and loc.uri ~= nil and loc.uri:match("feature/src/jvmMain/kotlin/lib/Thing%.kt$") ~= nil,
      loc and loc.uri
    )
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

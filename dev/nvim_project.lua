---@diagnostic disable: undefined-global
-- Generic headless Neovim probe for an existing Kotlin project.
--
--     nvim -l dev/nvim_project.lua <project-dir> <file.kt> [ktlsp-bin]
--       [implementation-token] [implementation-occurrence] [implementation-target]
--       [definition-token] [definition-occurrence] [definition-target]
--
-- This intentionally performs only broad LSP health checks. Feature-specific assertions live in
-- nvim_smoke.lua, nvim_features.lua, nvim_library.lua, and the Gradle probes.

local project = arg[1]
local file = arg[2]
local bin = arg[3] or os.getenv("KTLSP_BIN")
local implementation_token = arg[4]
local implementation_occurrence = tonumber(arg[5]) or 2
local implementation_target = arg[6]
local definition_token = arg[7]
local definition_occurrence = tonumber(arg[8]) or 1
local definition_target = arg[9]

assert(project and project ~= "", "usage: nvim -l dev/nvim_project.lua <project-dir> <file.kt> [ktlsp-bin]")
assert(file and file ~= "", "usage: nvim -l dev/nvim_project.lua <project-dir> <file.kt> [ktlsp-bin]")

if not bin or bin == "" then
  local script = debug.getinfo(1, "S").source:gsub("^@", "")
  local dev_dir = vim.fn.fnamemodify(script, ":p:h")
  local repo = vim.fn.fnamemodify(dev_dir, ":h")
  bin = repo .. "/target/release/ktlsp"
  if vim.fn.filereadable(bin) == 0 then
    bin = repo .. "/target/debug/ktlsp"
  end
end
assert(vim.fn.filereadable(bin) == 1, "ktlsp binary not found")
assert(vim.fn.filereadable(file) == 1, "probe file not readable: " .. file)

local failures = {}
local function check(name, ok, detail)
  if ok then
    print(("PASS  %s"):format(name))
  else
    print(("FAIL  %s%s"):format(name, detail and ("  (" .. tostring(detail) .. ")") or ""))
    table.insert(failures, name)
  end
end

vim.cmd("edit " .. vim.fn.fnameescape(file))
local bufnr = vim.api.nvim_get_current_buf()
local detected = vim.filetype.match({ filename = file, buf = bufnr })
local expected = file:match("%.java$") and "java" or "kotlin"
check("filetype detection: " .. expected, detected == expected, detected)
vim.bo[bufnr].filetype = detected or expected

local client_id = vim.lsp.start({ name = "ktlsp", cmd = { bin }, root_dir = project })
assert(client_id, "vim.lsp.start returned nil")

local ready = vim.wait(10000, function()
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
  check("advertises definitionProvider", caps.definitionProvider ~= nil and caps.definitionProvider ~= false, vim.inspect(caps.definitionProvider))
  check("advertises referencesProvider", caps.referencesProvider ~= nil and caps.referencesProvider ~= false, vim.inspect(caps.referencesProvider))
  check("advertises completionProvider", caps.completionProvider ~= nil, vim.inspect(caps.completionProvider))
  check("advertises implementationProvider", caps.implementationProvider ~= nil and caps.implementationProvider ~= false, vim.inspect(caps.implementationProvider))
end

vim.wait(500)
print(("diagnostics: %d"):format(#vim.diagnostic.get(bufnr)))

local function find_token(token, occurrence)
  local seen = 0
  for index, text in ipairs(vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)) do
    local from = 1
    while true do
      local start = text:find(token, from, true)
      if not start then
        break
      end
      seen = seen + 1
      if seen == occurrence then
        return index - 1, start - 1
      end
      from = start + #token
    end
  end
end

if client and implementation_token and implementation_token ~= "" then
  local line, column = find_token(implementation_token, implementation_occurrence)
  check(("found implementation token %s occurrence %d"):format(implementation_token, implementation_occurrence), line ~= nil)

  if line then
    local result
    local resolved = vim.wait(120000, function()
      local responses = vim.lsp.buf_request_sync(bufnr, "textDocument/implementation", {
        textDocument = { uri = vim.uri_from_bufnr(bufnr) },
        position = { line = line, character = column },
      }, 5000) or {}
      for _, response in pairs(responses) do
        if response.result ~= nil then
          local first = response.result[1] or response.result
          if first and first.uri then
            result = response.result
            return true
          end
        end
      end
      return false
    end, 250)
    check(("implementation(%s) returns a location"):format(implementation_token), resolved, vim.inspect(result))

    if resolved and implementation_target and implementation_target ~= "" then
      local locations = result.uri and { result } or result
      local found_target = false
      for _, location in ipairs(locations) do
        if location.uri and location.uri:find(implementation_target, 1, true) then
          found_target = true
          break
        end
      end
      check(("implementation(%s) includes %s"):format(implementation_token, implementation_target), found_target, vim.inspect(result))
    end
  end
end

if client and definition_token and definition_token ~= "" then
  local line, column = find_token(definition_token, definition_occurrence)
  check(("found definition token %s occurrence %d"):format(definition_token, definition_occurrence), line ~= nil)

  if line then
    local hover_result
    local hover_resolved = vim.wait(120000, function()
      local responses = vim.lsp.buf_request_sync(bufnr, "textDocument/hover", {
        textDocument = { uri = vim.uri_from_bufnr(bufnr) },
        position = { line = line, character = column },
      }, 5000) or {}
      for _, response in pairs(responses) do
        if response.result and response.result.contents then
          hover_result = response.result
          return true
        end
      end
      return false
    end, 250)
    check(("hover(%s) returns content without prior goto"):format(definition_token), hover_resolved, vim.inspect(hover_result))

    local result
    local resolved = vim.wait(120000, function()
      local responses = vim.lsp.buf_request_sync(bufnr, "textDocument/definition", {
        textDocument = { uri = vim.uri_from_bufnr(bufnr) },
        position = { line = line, character = column },
      }, 5000) or {}
      for _, response in pairs(responses) do
        if response.result ~= nil then
          local first = response.result[1] or response.result
          if first and first.uri then
            result = response.result
            return true
          end
        end
      end
      return false
    end, 250)
    check(("definition(%s) returns a location"):format(definition_token), resolved, vim.inspect(result))

    if resolved and definition_target and definition_target ~= "" then
      local locations = result.uri and { result } or result
      local found_target = false
      for _, location in ipairs(locations) do
        if location.uri and location.uri:find(definition_target, 1, true) then
          found_target = true
          break
        end
      end
      check(("definition(%s) includes %s"):format(definition_token, definition_target), found_target, vim.inspect(result))
    end

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

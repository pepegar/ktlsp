---@diagnostic disable: undefined-global
-- Generic headless Neovim probe for an existing Kotlin project.
--
--     nvim -l dev/nvim_project.lua <project-dir> <file.kt> [ktlsp-bin]
--
-- This intentionally performs only broad LSP health checks. Feature-specific assertions live in
-- nvim_smoke.lua, nvim_features.lua, nvim_library.lua, and the Gradle probes.

local project = arg[1]
local file = arg[2]
local bin = arg[3] or os.getenv("KTLSP_BIN")

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
check("filetype detection: Kotlin", detected == "kotlin", detected)
vim.bo[bufnr].filetype = detected or "kotlin"

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
end

vim.wait(500)
print(("diagnostics: %d"):format(#vim.diagnostic.get(bufnr)))

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

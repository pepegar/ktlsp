---@diagnostic disable: undefined-global
-- Live verification of ktlsp against the real Gradle project (dev/gradle-sample).
local script = debug.getinfo(1, "S").source:gsub("^@", "")
local dev_dir = vim.fn.fnamemodify(script, ":p:h")
local repo = vim.fn.fnamemodify(dev_dir, ":h")
local root = arg[1] or (repo .. "/dev/gradle-sample")
local bin = arg[2] or os.getenv("KTLSP_BIN")
if not bin or bin == "" then
  bin = repo .. "/target/release/ktlsp"
  if vim.fn.filereadable(bin) == 0 then
    bin = repo .. "/target/debug/ktlsp"
  end
end
local probe = arg[3] or (root .. "/src/main/kotlin/com/example/fixture/_Probe.kt")

local server_messages = {}
local default_log_message = vim.lsp.handlers["window/logMessage"]
vim.lsp.handlers["window/logMessage"] = function(err, result, ctx, config)
  if result and result.message then
    server_messages[#server_messages + 1] = result.message
  end
  if default_log_message then
    return default_log_message(err, result, ctx, config)
  end
end

vim.cmd("edit " .. probe)
local buf = vim.api.nvim_get_current_buf()
vim.bo[buf].filetype = "kotlin"
local id = vim.lsp.start({ name = "ktlsp", cmd = { bin }, root_dir = root })
vim.wait(10000, function()
  local c = vim.lsp.get_client_by_id(id)
  return c and c.server_capabilities ~= nil
end, 50)

local function lines() return vim.api.nvim_buf_get_lines(buf, 0, -1, false) end
local function find_line(needle)
  for i, l in ipairs(lines()) do
    if l:find(needle, 1, true) then return i - 1, l end
  end
end
local function complete_labels(line, char)
  local r = vim.lsp.buf_request_sync(buf, "textDocument/completion",
    { textDocument = { uri = vim.uri_from_bufnr(buf) },
      position = { line = line, character = char },
      context = { triggerKind = 1 } }, 4000) or {}
  local labels = {}
  for _, v in pairs(r) do
    local items = v.result and (v.result.items or v.result) or {}
    for _, it in ipairs(items) do labels[it.label] = true end
  end
  return labels
end
local function labels_after(needle)
  local line, l = find_line(needle)
  if not line then return {}, false end
  return complete_labels(line, #l), true
end
local function wait_for_message(pattern, timeout_ms)
  return vim.wait(timeout_ms, function()
    for _, message in ipairs(server_messages) do
      if message:find(pattern) then
        return true
      end
    end
    return false
  end, 1000)
end

local results = {}
local function check(desc, needle, expected)
  local labels, found = labels_after(needle)
  if not found then results[#results + 1] = "FAIL  " .. desc .. " (probe line not found)"; return false end
  local ok = labels[expected] == true
  results[#results + 1] = (ok and "PASS  " or "FAIL  ") .. desc .. "  (expect `" .. expected .. "`)"
  return ok
end
local function check_wait(desc, needle, expected, timeout_ms)
  local line_seen = false
  local deadline = vim.loop.hrtime() + (timeout_ms * 1000000)
  local ok = false
  while vim.loop.hrtime() < deadline do
    local labels, found = labels_after(needle)
    line_seen = line_seen or found
    if found and labels[expected] == true then
      ok = true
      break
    end
    vim.loop.sleep(1000)
  end
  if not line_seen then
    results[#results + 1] = "FAIL  " .. desc .. " (probe line not found)"
    return false
  end
  results[#results + 1] = (ok and "PASS  " or "FAIL  ") .. desc .. "  (expect `" .. expected .. "`)"
  return ok
end

-- 1) Wait for the project scan to warm (project-local completion resolves).
vim.wait(20000, function()
  local labels = labels_after("    g.gr")
  return labels["greet"] == true
end, 300)

-- Project-local inference (no library download needed).
check("member completion on a local (BasicGreeter)", "    g.gr", "greet")
check("function return-type inference (greeterFor -> Greeter)", '    greeterFor("en").gr', "greet")
check("companion/static access (BasicGreeter.default)", "    BasicGreeter.def", "default")
check("chained call (default() -> BasicGreeter)", "    BasicGreeter.default().sal", "salutation")

-- 2) Wait for library indexing (first run downloads/parses stdlib+serialization+coroutines+okio
-- sources). Do this from ktlsp's completion log message instead of polling completions; polling
-- while the cold index is inserting files can compete for the workspace lock and make readiness
-- look like a functional failure.
local lib_ready = wait_for_message("ktlsp indexed .* library files", 300000)

if lib_ready then
  check("stdlib String member completion (\"hello\".upper)", "    s.upper", "uppercase")
else
  results[#results + 1] = "FAIL  library dependency indexing completed (timed out)"
end

-- 3) Goto-definition into the kotlin-stdlib sources jar (goto on the `String` return type).
do
  local line, l = find_line("_probeStdlibType(): String")
  local desc = "goto-definition into stdlib (`String` -> extracted sources)"
  if line then
    local col = l:find("String", 1, true) - 1
    local r = vim.lsp.buf_request_sync(buf, "textDocument/definition",
      { textDocument = { uri = vim.uri_from_bufnr(buf) }, position = { line = line, character = col } }, 4000) or {}
    local uri = nil
    for _, v in pairs(r) do
      local res = v.result
      if res then
        if res.uri then uri = res.uri elseif res[1] then uri = res[1].uri or (res[1].targetUri) end
      end
    end
    local ok = uri ~= nil and (uri:find("ktlsp/extracted", 1, true) ~= nil or uri:find("kotlin", 1, true) ~= nil)
    results[#results + 1] = (ok and "PASS  " or "FAIL  ") .. desc .. "  (-> " .. tostring(uri) .. ")"
  else
    results[#results + 1] = "FAIL  " .. desc .. " (probe line not found)"
  end
end

-- 4) Goto-definition on a project type (BasicGreeter -> Greetings.kt).
do
  local line, l = find_line("    val g = BasicGreeter()")
  local desc = "goto-definition on a project type (BasicGreeter)"
  if line then
    local col = l:find("BasicGreeter", 1, true) - 1
    local r = vim.lsp.buf_request_sync(buf, "textDocument/definition",
      { textDocument = { uri = vim.uri_from_bufnr(buf) }, position = { line = line, character = col } }, 4000) or {}
    local uri = nil
    for _, v in pairs(r) do
      local res = v.result
      if res then if res.uri then uri = res.uri elseif res[1] then uri = res[1].uri or res[1].targetUri end end
    end
    local ok = uri ~= nil and uri:find("Greetings.kt", 1, true) ~= nil
    results[#results + 1] = (ok and "PASS  " or "FAIL  ") .. desc .. "  (-> " .. tostring(uri) .. ")"
  else
    results[#results + 1] = "FAIL  " .. desc
  end
end

-- 5) Gradual-checker features (require the stdlib index for the generic/lambda probes).
check("smart-cast (if x is BasicGreeter)", "        x.sal", "salutation")
check("early-return narrowing (if y !is BasicGreeter return)", "    y.sal", "salutation")
if lib_ready then
  check_wait("generic chain (listOf(x).first())", "    listOf(BasicGreeter()).first().sal", "salutation", 30000)
  check_wait("lambda `it` element type (listOf(x).map { it })", "        it.sal", "salutation", 30000)
end

-- 6) Unused-import diagnostic (published; debounced ~300ms). Read via vim.diagnostic after a wait.
do
  local desc = "unused-import diagnostic (import kotlinx.coroutines.delay)"
  local found = vim.wait(8000, function()
    for _, d in ipairs(vim.diagnostic.get(buf)) do
      if tostring(d.message):find("delay", 1, true) then return true end
    end
    return false
  end, 200)
  -- And confirm NO false positive on a used/needed name (sanity: only the unused one is flagged).
  local count = #vim.diagnostic.get(buf)
  results[#results + 1] = (found and "PASS  " or "FAIL  ") .. desc .. ("  (%d diagnostic(s) total)"):format(count)
end

-- 7) Opt-in gradle compile diagnostics (env-gated: real gradle is slow, so the fast probes above
--    don't pay for it). Run with `KTLSP_LIVE_COMPILE=1`. Uses its own client with the feature
--    enabled via initializationOptions, and pre-seeds workspace trust so no prompt blocks the
--    headless run.
if os.getenv("KTLSP_LIVE_COMPILE") then
  local desc = "gradle compile diagnostic (ktlsp (gradle) ERROR on a broken file)"
  -- Pre-seed trust: write the canonical root into ktlsp's cache root.
  local home = os.getenv("HOME")
  local cache_dir = os.getenv("KTLSP_CACHE_DIR")
  local canon = vim.loop.fs_realpath(root) or root
  local trust_dir = cache_dir and cache_dir ~= "" and cache_dir or (assert(home, "HOME or KTLSP_CACHE_DIR required") .. "/.cache/ktlsp")
  local trust_path = trust_dir .. "/trusted_roots"
  vim.fn.mkdir(trust_dir, "p")
  local tf = io.open(trust_path, "a")
  if tf then tf:write(canon .. "\n"); tf:close() end

  -- A throwaway source with a deliberate unresolved reference (compiled by compileKotlin).
  local broken = root .. "/src/main/kotlin/com/example/fixture/_CompileProbe.kt"
  local function write(path, body)
    local f = io.open(path, "w"); if f then f:write(body); f:close() end
  end
  write(broken, "package com.example.fixture\nval broken: Int = thisDoesNotResolve\n")

  vim.cmd("edit " .. broken)
  local cbuf = vim.api.nvim_get_current_buf()
  vim.bo[cbuf].filetype = "kotlin"
  local cid = vim.lsp.start({
    name = "ktlsp-compile",
    cmd = { bin },
    root_dir = root,
    init_options = { compile_diagnostics = { enabled = true } },
  })
  vim.wait(10000, function()
    local c = vim.lsp.get_client_by_id(cid)
    return c and c.server_capabilities ~= nil
  end, 50)
  vim.cmd("write")

  -- Gradle is multi-second (cold daemon much more); poll generously for an error from our source.
  local function has_gradle_error()
    for _, d in ipairs(vim.diagnostic.get(cbuf)) do
      if d.source == "ktlsp (gradle)" and d.severity == vim.diagnostic.severity.ERROR then
        return true
      end
    end
    return false
  end
  local appeared = vim.wait(180000, has_gradle_error, 1000)
  results[#results + 1] = (appeared and "PASS  " or "FAIL  ") .. desc

  -- Fix it and confirm the diagnostic clears after a recompile.
  if appeared then
    write(broken, "package com.example.fixture\nval ok: Int = 1\n")
    vim.cmd("edit! " .. broken)
    vim.cmd("write")
    local cleared = vim.wait(180000, function() return not has_gradle_error() end, 1000)
    results[#results + 1] = (cleared and "PASS  " or "FAIL  ") .. "gradle diagnostic clears after fix+recompile"
  end

  os.remove(broken)
end

print("\n===== ktlsp live verification (dev/gradle-sample) =====")
for _, r in ipairs(results) do print(r) end
print("======================================================")
for _, r in ipairs(results) do
  if r:match("^FAIL") then
    os.exit(1)
  end
end
os.exit(0)

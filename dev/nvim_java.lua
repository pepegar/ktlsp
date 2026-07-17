---@diagnostic disable: undefined-global
-- Headless Neovim check for Java project support: open a .java file, goto definition into
-- another .java file in the same project.

local project = arg[1]
assert(project and project ~= "", "usage: nvim -l dev/nvim_java.lua <project-dir> [ktlsp-bin]")

local bin = arg[2] or os.getenv("KTLSP_BIN")
if not bin or bin == "" then
  bin = vim.fn.getcwd() .. "/target/release/ktlsp"
  if vim.fn.filereadable(bin) == 0 then
    bin = vim.fn.getcwd() .. "/target/debug/ktlsp"
  end
end
assert(vim.fn.filereadable(bin) == 1, "ktlsp binary not found — run `cargo build` first")

local failures = {}
local function check(name, ok, detail)
  if ok then
    print("PASS: " .. name)
  else
    print("FAIL: " .. name)
    if detail then
      print("  " .. detail)
    end
    table.insert(failures, name)
  end
end

vim.cmd("edit " .. vim.fn.fnameescape(project .. "/app/Main.java"))
local bufnr = vim.api.nvim_get_current_buf()
vim.bo[bufnr].filetype = "java"

local id = vim.lsp.start({ name = "ktlsp", cmd = { bin }, root_dir = project })
assert(id, "vim.lsp.start returned nil")
vim.wait(8000, function()
  local c = vim.lsp.get_client_by_id(id)
  return c ~= nil and (c.initialized == true or (c.server_capabilities and c.server_capabilities.definitionProvider))
end, 50)

local client = vim.lsp.get_client_by_id(id)
check("advertises definitionProvider", client and client.server_capabilities.definitionProvider ~= nil)
check("advertises referencesProvider", client and client.server_capabilities.referencesProvider ~= nil)
check("advertises documentSymbolProvider", client and client.server_capabilities.documentSymbolProvider ~= nil)
check("advertises workspaceSymbolProvider", client and client.server_capabilities.workspaceSymbolProvider ~= nil)
check("advertises completionProvider", client and client.server_capabilities.completionProvider ~= nil)
check("advertises renameProvider", client and client.server_capabilities.renameProvider ~= nil)
check("advertises signatureHelpProvider", client and client.server_capabilities.signatureHelpProvider ~= nil)
check("advertises implementationProvider", client and client.server_capabilities.implementationProvider ~= nil)
check("advertises typeDefinitionProvider", client and client.server_capabilities.typeDefinitionProvider ~= nil)
check("advertises semanticTokensProvider", client and client.server_capabilities.semanticTokensProvider ~= nil)
check("advertises inlayHintProvider", client and client.server_capabilities.inlayHintProvider ~= nil)
check("advertises callHierarchyProvider", client and client.server_capabilities.callHierarchyProvider ~= nil)
check("advertises typeHierarchyProvider", client and client.server_capabilities.typeHierarchyProvider ~= nil)
check("advertises foldingRangeProvider", client and client.server_capabilities.foldingRangeProvider ~= nil)
check("advertises selectionRangeProvider", client and client.server_capabilities.selectionRangeProvider ~= nil)

local function find(anchor, token)
  local lines = vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)
  for l, line in ipairs(lines) do
    if line:find(anchor, 1, true) then
      local c = line:find(token, 1, true)
      if c then
        return l - 1, c - 1
      end
    end
  end
  return nil
end

local function request(method, params)
  local res
  local ok = vim.lsp.buf_request_sync(bufnr, method, params, 3000)
  if ok then
    for _, v in pairs(ok) do
      if v.result ~= nil then
        res = v.result
        break
      end
    end
  end
  return res
end

local function client_request(method, params)
  if not client then
    return nil
  end
  local result = nil
  local done = false
  client:request(method, params, function(_, res)
    result = res
    done = true
  end, bufnr)
  vim.wait(3000, function()
    return done
  end, 50)
  if done then
    return result
  end
  return nil
end

local uri = vim.uri_from_bufnr(bufnr)

local function result_mentions_uri(res, needle)
  if type(res) ~= "table" then
    return false, "empty"
  end
  if res.uri and res.uri:find(needle, 1, true) then
    return true, res.uri
  end
  if res.targetUri and res.targetUri:find(needle, 1, true) then
    return true, res.targetUri
  end
  for _, item in ipairs(res) do
    if type(item) == "table" then
      if item.uri and item.uri:find(needle, 1, true) then
        return true, item.uri
      end
      if item.targetUri and item.targetUri:find(needle, 1, true) then
        return true, item.targetUri
      end
    end
  end
  return false, vim.inspect(res)
end

-- documentSymbol should list the Main class and its run method.
do
  local res = request("textDocument/documentSymbol", { textDocument = { uri = uri } })
  local has_main = false
  local has_run = false
  if type(res) == "table" then
    for _, s in ipairs(res) do
      if s.name == "Main" then
        has_main = true
      end
      if s.name == "run" then
        has_run = true
      end
    end
  end
  check("documentSymbol includes Main and run", has_main and has_run, vim.inspect(res))
end

-- workspace/symbol should find Helper from the other file.
do
  local res = request("workspace/symbol", { query = "Helper" })
  local has_helper = false
  if type(res) == "table" then
    for _, s in ipairs(res) do
      if s.name == "Helper" then
        has_helper = true
      end
    end
  end
  check("workspace/symbol finds Helper", has_helper, vim.inspect(res))
end

-- Goto definition from Main.java to Helper.java.
do
  local l, c = find("new Helper()", "Helper")
  check("found Helper reference in Main.java", l ~= nil, "line/col")
  if l then
    local res = request("textDocument/definition", {
      textDocument = { uri = uri },
      position = { line = l, character = c },
    })
    local ok = false
    local detail = "empty"
    if type(res) == "table" then
      -- LSP may return a single Location {uri, range} or an array of LocationLink {targetUri, ...}.
      if res.uri and res.uri:find("Helper.java", 1, true) then
        ok = true
        detail = res.uri
      elseif res.targetUri and res.targetUri:find("Helper.java", 1, true) then
        ok = true
        detail = res.targetUri
      elseif #res > 0 then
        local helper_count = 0
        for _, d in ipairs(res) do
          if d.uri and d.uri:find("Helper.java", 1, true) then
            helper_count = helper_count + 1
            detail = d.uri
          end
          if d.targetUri and d.targetUri:find("Helper.java", 1, true) then
            helper_count = helper_count + 1
            detail = d.targetUri
          end
        end
        ok = helper_count == 1 and #res == 1
        if not ok then
          detail = "expected exactly one Helper.java definition, got " .. vim.inspect(res)
        end
      end
    end
    check("goto-definition lands in Helper.java", ok, detail)
  end
end

-- Goto definition on an overloaded Java method call should use the known argument count.
do
  local l, c = find("helper.waitFor(1);", "waitFor")
  check("found overloaded waitFor call in Main.java", l ~= nil, "line/col")
  if l then
    local res = request("textDocument/definition", {
      textDocument = { uri = uri },
      position = { line = l, character = c },
    })
    local locations = {}
    if type(res) == "table" then
      if res.uri or res.targetUri then
        locations = { res }
      else
        locations = res
      end
    end
    local expected_line
    local helper_lines = vim.fn.readfile(project .. "/app/Helper.java")
    for idx, line in ipairs(helper_lines) do
      if line:find("void waitFor(int seconds)", 1, true) then
        expected_line = idx - 1
        break
      end
    end
    local loc = locations[1]
    local loc_uri = loc and (loc.uri or loc.targetUri) or nil
    local loc_range = loc and (loc.range or loc.targetSelectionRange) or nil
    local ok = #locations == 1
      and loc_uri
      and loc_uri:find("Helper.java", 1, true)
      and loc_range
      and expected_line ~= nil
      and loc_range.start.line == expected_line
    check("goto-definition narrows overloaded Java method by arity", ok, vim.inspect(res))
  end
end

-- Type definition from a Java value usage should resolve to the value's declared type.
do
  local l, c = find("helper.assist();", "helper")
  check("found helper value for typeDefinition", l ~= nil, "line/col")
  if l then
    local res = request("textDocument/typeDefinition", {
      textDocument = { uri = uri },
      position = { line = l, character = c },
    })
    local ok, detail = result_mentions_uri(res, "Helper.java")
    check("typeDefinition(helper) returns Helper", ok, detail)
  end
end

-- Find references on Helper: declaration + usage.
do
  local l, c = find("new Helper()", "Helper")
  if l then
    local res = request("textDocument/references", {
      textDocument = { uri = uri },
      position = { line = l, character = c },
      context = { includeDeclaration = true },
    })
    local ok = false
    local detail = "empty"
    if type(res) == "table" and #res >= 2 then
      ok = true
      detail = tostring(#res) .. " refs"
    end
    check("references returns Helper declaration and usage", ok, detail)
  else
    check("references returns Helper declaration and usage", false, "could not find Helper")
  end
end

-- Folding ranges should expose Java class/method blocks.
do
  local res = request("textDocument/foldingRange", { textDocument = { uri = uri } })
  local ok = type(res) == "table" and #res >= 2
  check("foldingRange returns Java block folds", ok, vim.inspect(res))
end

-- Selection range should expand from a Java identifier through its method invocation.
do
  local l, c = find("helper.assist();", "assist")
  check("found selectionRange target in Main.java", l ~= nil, "line/col")
  if l then
    local res = request("textDocument/selectionRange", {
      textDocument = { uri = uri },
      positions = { { line = l, character = c } },
    })
    local item = type(res) == "table" and res[1] or nil
    check("selectionRange returns Java parent chain", item ~= nil and item.parent ~= nil, vim.inspect(res))
  end
end

-- Semantic tokens should classify Java-specific declarations, not just Kotlin-shaped identifiers.
do
  local res = request("textDocument/semanticTokens/full", { textDocument = { uri = uri } })
  local has_function = false
  local has_parameter = false
  if type(res) == "table" and type(res.data) == "table" then
    for i = 4, #res.data, 5 do
      if res.data[i] == 5 then
        has_function = true
      end
      if res.data[i] == 8 then
        has_parameter = true
      end
    end
  end
  check("semanticTokens/full includes Java function tokens", has_function, vim.inspect(res))
  check("semanticTokens/full includes Java parameter tokens", has_parameter, vim.inspect(res))
end

-- Inlay hints should report Java `var` inferred local types.
do
  local res = request("textDocument/inlayHint", {
    textDocument = { uri = uri },
    range = {
      start = { line = 0, character = 0 },
      ["end"] = { line = vim.api.nvim_buf_line_count(bufnr), character = 0 },
    },
  })
  local has_helper_hint = false
  if type(res) == "table" then
    for _, hint in ipairs(res) do
      local label = hint.label
      if type(label) == "table" then
        label = vim.inspect(label)
      end
      if label == ": Helper" then
        has_helper_hint = true
        break
      end
    end
  end
  check("inlayHint returns Java var type", has_helper_hint, vim.inspect(res))
end

-- Call hierarchy should report Java method callers and callees.
do
  local al, ac = find("helper.assist();", "assist")
  check("found callHierarchy assist target in Main.java", al ~= nil, "line/col")
  if al then
    local prepared = request("textDocument/prepareCallHierarchy", {
      textDocument = { uri = uri },
      position = { line = al, character = ac },
    })
    local item = type(prepared) == "table" and prepared[1] or nil
    check("prepareCallHierarchy(assist) returns item", item ~= nil and item.name == "assist", vim.inspect(prepared))
    if item then
      local incoming = request("callHierarchy/incomingCalls", { item = item })
      local has_run = false
      if type(incoming) == "table" then
        for _, call in ipairs(incoming) do
          if call.from and call.from.name == "run" then
            has_run = true
            break
          end
        end
      end
      check("callHierarchy/incomingCalls(assist) returns run", has_run, vim.inspect(incoming))
    end
  end

  local rl, rc = find("public void run", "run")
  check("found callHierarchy run target in Main.java", rl ~= nil, "line/col")
  if rl then
    local prepared = request("textDocument/prepareCallHierarchy", {
      textDocument = { uri = uri },
      position = { line = rl, character = rc },
    })
    local item = type(prepared) == "table" and prepared[1] or nil
    check("prepareCallHierarchy(run) returns item", item ~= nil and item.name == "run", vim.inspect(prepared))
    if item then
      local outgoing = request("callHierarchy/outgoingCalls", { item = item })
      local has_assist = false
      if type(outgoing) == "table" then
        for _, call in ipairs(outgoing) do
          if call.to and call.to.name == "assist" then
            has_assist = true
            break
          end
        end
      end
      check("callHierarchy/outgoingCalls(run) returns assist", has_assist, vim.inspect(outgoing))
    end
  end
end

-- Java semantic diagnostics should include unused explicit imports.
do
  local ok = vim.wait(5000, function()
    local diagnostics = vim.diagnostic.get(bufnr)
    for _, diagnostic in ipairs(diagnostics) do
      local message = diagnostic.message or ""
      if message:find("Unused import: List", 1, true) then
        return true
      end
    end
    return false
  end, 50)
  check("publishes Java unused-import diagnostic", ok, vim.inspect(vim.diagnostic.get(bufnr)))
end

-- Code actions should offer the Java unused-import quickfix as an LSP WorkspaceEdit.
do
  local l, c = find("import java.util.List;", "List")
  check("found Java unused import for codeAction", l ~= nil, "line/col")
  if l then
    local actions = request("textDocument/codeAction", {
      textDocument = { uri = uri },
      range = {
        start = { line = l, character = c },
        ["end"] = { line = l, character = c + 4 },
      },
      context = { diagnostics = {} },
    })
    local found = false
    if type(actions) == "table" then
      for _, action in ipairs(actions) do
        if action.title == "Remove unused import `List`" and action.edit ~= nil then
          found = true
          break
        end
      end
    end
    check("codeAction removes Java unused import", found, vim.inspect(actions))
  end
end

-- Java diagnostics should include conservative unresolved simple calls once the project scan is complete.
do
  local ok = vim.wait(5000, function()
    local diagnostics = vim.diagnostic.get(bufnr)
    for _, diagnostic in ipairs(diagnostics) do
      local message = diagnostic.message or ""
      if message:find("Unresolved reference: missingCall", 1, true) then
        return true
      end
    end
    return false
  end, 50)
  check("publishes Java unresolved-reference diagnostic", ok, vim.inspect(vim.diagnostic.get(bufnr)))
end

-- Java diagnostics should include conservative wrong-arity call-shape errors.
do
  local ok = vim.wait(5000, function()
    local diagnostics = vim.diagnostic.get(bufnr)
    for _, diagnostic in ipairs(diagnostics) do
      local message = diagnostic.message or ""
      if message:find("No overload of combine accepts 1 argument", 1, true) then
        return true
      end
    end
    return false
  end, 50)
  check("publishes Java call-shape diagnostic", ok, vim.inspect(vim.diagnostic.get(bufnr)))
end

-- Java diagnostics should include conservative argument-type call-shape errors.
do
  local ok = vim.wait(5000, function()
    local diagnostics = vim.diagnostic.get(bufnr)
    for _, diagnostic in ipairs(diagnostics) do
      local message = diagnostic.message or ""
      if message:find("No overload of adopt accepts argument type (Dog)", 1, true) then
        return true
      end
    end
    return false
  end, 50)
  check("publishes Java argument-type diagnostic", ok, vim.inspect(vim.diagnostic.get(bufnr)))
end

-- Implementation should use Java extends/implements edges.
do
  local l, c = find("Worker worker", "Worker")
  check("found Worker type in Main.java", l ~= nil, "line/col")
  if l then
    local impls = request("textDocument/implementation", {
      textDocument = { uri = uri },
      position = { line = l, character = c },
    })
    local ok, detail = result_mentions_uri(impls, "Helper.java")
    check("implementation(Worker) returns Helper", ok, detail)
  end
end

-- Implementation on an interface member should return the overriding member.
do
  local l, c = find("worker.assist()", "assist")
  check("found Worker.assist member usage in Main.java", l ~= nil, "line/col")
  if l then
    local impls = request("textDocument/implementation", {
      textDocument = { uri = uri },
      position = { line = l, character = c },
    })
    local ok, detail = result_mentions_uri(impls, "Helper.java")
    check("implementation(Worker.assist) returns Helper.assist", ok, detail)
  end
end

-- Type hierarchy should expose Java extends/implements edges.
do
  local l, c = find("Worker worker", "Worker")
  check("found Worker typeHierarchy target in Main.java", l ~= nil, "line/col")
  if l then
    local prepared = request("textDocument/prepareTypeHierarchy", {
      textDocument = { uri = uri },
      position = { line = l, character = c },
    })
    local item = type(prepared) == "table" and prepared[1] or nil
    check("prepareTypeHierarchy(Worker) returns item", item ~= nil and item.name == "Worker", vim.inspect(prepared))
    if item then
      local subtypes = client_request("typeHierarchy/subtypes", { item = item })
      local has_helper = false
      if type(subtypes) == "table" then
        for _, subtype in ipairs(subtypes) do
          if subtype.name == "Helper" then
            has_helper = true
            break
          end
        end
      end
      check("typeHierarchy/subtypes(Worker) returns Helper", has_helper, vim.inspect(subtypes))
    end
  end
end

-- Completion at the first Helper reference should offer Helper.
do
  local l, c = find("Helper helper", "Helper")
  check("found completion target in Main.java", l ~= nil, "line/col")
  if l then
    local res = request("textDocument/completion", {
      textDocument = { uri = uri },
      position = { line = l, character = c },
    })
    local ok = false
    local detail = "empty"
    if type(res) == "table" then
      local items = res.items or res
      if type(items) == "table" then
        for _, item in ipairs(items) do
          if item.label == "Helper" then
            ok = true
            detail = item.label
            break
          end
        end
      end
    end
    check("completion offers Helper", ok, detail)
  end
end

-- Signature help on a Java method call should include parameter and return types.
do
  local l, c = find('helper.combine("Ada", 2)', "2")
  check("found signatureHelp target in Main.java", l ~= nil, "line/col")
  if l then
    local res = request("textDocument/signatureHelp", {
      textDocument = { uri = uri },
      position = { line = l, character = c },
    })
    local ok = false
    local detail = "empty"
    if type(res) == "table" and type(res.signatures) == "table" then
      for _, sig in ipairs(res.signatures) do
        if sig.label == "combine(p1: String, p2: int): String" and res.activeParameter == 1 then
          ok = true
          detail = sig.label
          break
        end
      end
      if not ok then
        detail = vim.inspect(res)
      end
    end
    check("signatureHelp(combine) returns Java signature", ok, detail)
  end
end

-- prepareRename on Helper should return the identifier range.
do
  local l, c = find("new Helper()", "Helper")
  if l then
    local res = request("textDocument/prepareRename", {
      textDocument = { uri = uri },
      position = { line = l, character = c },
    })
    local ok = false
    local detail = "empty"
    if type(res) == "table" and (res.range or res.start) then
      ok = true
      detail = "range"
    end
    check("prepareRename returns range for Helper", ok, detail)
  else
    check("prepareRename returns range for Helper", false, "could not find Helper")
  end
end

-- Rename Helper to Renamed should produce edits.
do
  local l, c = find("new Helper()", "Helper")
  if l then
    local res = request("textDocument/rename", {
      textDocument = { uri = uri },
      position = { line = l, character = c },
      newName = "Renamed",
    })
    local ok = false
    local detail = "empty"
    if type(res) == "table" and res.changes then
      local count = 0
      for _, _ in pairs(res.changes) do
        count = count + 1
      end
      if count >= 1 then
        ok = true
        detail = tostring(count) .. " files"
      end
    end
    check("rename returns edits for Helper", ok, detail)
  else
    check("rename returns edits for Helper", false, "could not find Helper")
  end
end

-- Java parser diagnostics should be published for syntax errors.
do
  local lines = vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)
  local edited = false
  for i, line in ipairs(lines) do
    if line:find('helper.combine("Ada", 2);', 1, true) then
      lines[i] = '        helper.combine("Ada", );'
      edited = true
      break
    end
  end
  if edited then
    vim.api.nvim_buf_set_lines(bufnr, 0, -1, false, lines)
    local ok = vim.wait(5000, function()
      local diagnostics = vim.diagnostic.get(bufnr)
      for _, diagnostic in ipairs(diagnostics) do
        if diagnostic.severity == vim.diagnostic.severity.ERROR then
          return true
        end
      end
      return false
    end, 50)
    check("publishes Java syntax diagnostics", ok, vim.inspect(vim.diagnostic.get(bufnr)))
  else
    check("publishes Java syntax diagnostics", false, "could not edit combine call")
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

---@diagnostic disable: undefined-global, need-check-nil
-- Comprehensive single-session verification of ktlsp against the real Gradle project
-- (dev/gradle-sample). Consolidates the probe files under com/example/probe/ authored by the
-- verification workflow. One nvim, one ktlsp client; opens each probe file and asserts completion /
-- goto / diagnostics. Prints a per-category summary + every FAIL with the raw observed labels.
local repo = "/Users/pepe/projects/github.com/pepegar/ktlsp"
local root = repo .. "/dev/gradle-sample"
local bin = repo .. "/target/release/ktlsp"
local probe = root .. "/src/main/kotlin/com/example/probe/"

local files = {
  S = probe .. "StdlibProbe.kt",
  R = probe .. "SerializationProbe.kt",
  C = probe .. "CoroutinesProbe.kt",
  O = probe .. "OkioProbe.kt",
  P = probe .. "ProjectTypesProbe.kt",
}

-- completion checks: {file_key, unique-line-substring, expected_label}
local COMPL = {
  -- stdlib: String members/extensions
  {"S","sx1.upper","uppercase"},{"S","sx2.tri","trim"},{"S","sx3.isBla","isBlank"},
  {"S","sx4.leng","length"},{"S","sx5.split","uppercase"},{"S","sx6.repla","replace"},
  {"S","sx7.subs","substring"},
  -- stdlib: collections + element types
  {"S","sx8.first().ema","email"},{"S","sx9.firstOrNull","email"},{"S","ititem.ema","email"},
  {"S","sxB.filter","email"},{"S","sxC.associateBy","keys"},{"S","sxD.groupBy","values"},
  {"S","sxE.joinToSt","joinToString"},{"S","sxF.sumO","sumOf"},
  -- stdlib: numbers / nullability
  {"S","sxG.toLo","toLong"},{"S","sxH.coerc","coerceIn"},{"S","sxJ.upper","uppercase"},
  {"S","sxK!!.ema","email"},{"S","sxL.address?.city?.upper","uppercase"},
  -- stdlib: project-type elements + plain/data/ctor-prop discriminators
  {"S","sxM.first().ema","email"},{"S","sxN.first().sal","salutation"},{"S","sxO.ema","email"},
  {"S","sxP.ema","email"},{"S","sxQ.pna","pname"},{"S","sxS.pmeth","pmethod"},
  {"S","sxT.xpr","xprop"},{"S","sxU.loc","locale"},{"S","sxR.cme","cmethod"},
  -- serialization
  {"R","Json.encodeToStr","encodeToString"},{"R","Json.decodeFromStr","decodeFromString"},
  {"R","Json.parseToJsonEl","parseToJsonElement"},{"R","prettyPr","prettyPrint"},
  {"R","ignoreUnknownK","ignoreUnknownKeys"},{"R","cfg.encodeToJsonEl","encodeToJsonElement"},
  {"R","cfgTwo.configura","configuration"},{"R","userVal.ema","email"},{"R","userRole.rol","role"},
  {"R","userCopy.cop","copy"},{"R","addr.postal","postalCode"},{"R","Role.ADM","ADMIN"},
  {"R","el.jsonObj","jsonObject"},{"R","prim.conten","content"},{"R","obj.toStr","toString"},
  {"R","cfg3.encodeToJsonElement(sampleUser()).jsonOb","jsonObject"},
  {"R","JsonPrimitive(7).int.toLo","toLong"},{"R","cityVal.upperc","uppercase"},
  -- coroutines
  {"C","this.lau","launch"},{"C","this.asy","async"},{"C","this.coroutineCon","coroutineContext"},
  {"C","jobValue.isAct","isActive"},{"C","jobValue.canc","cancel"},{"C","jobValue.joi","join"},
  {"C","deferredUser.awa","await"},{"C","awaitedUser.nam","name"},
  {"C","Dispatchers.Defau","Default"},{"C","Dispatchers.I","IO"},{"C","Dispatchers.Mai","Main"},
  {"C","numberFlow.ma","map"},{"C","numberFlow.filt","filter"},{"C","numberFlow.collec","collect"},
  {"C","flowedUser.ema","email"},{"C","contextResult.upper","uppercase"},
  {"C","suspendProduced.rol","role"},{"C","diagScope.lau","launch"},{"C","typedAwaited.nam","name"},
  {"C","mappedInt.toLo","toLong"},{"C","typedSuspend.act","active"},{"C","plainUser.ema","email"},
  {"C","plainInSuspend.tag","tags"},{"C","collectedInt.toBy","toByte"},{"C","listInt.toSh","toShort"},
  {"C","ctxString.repe","repeat"},
  -- okio
  {"O","bufW.writeUtf","writeUtf8"},{"O","bufR.readUtf","readUtf8"},{"O","bufS.snaps","snapshot"},
  {"O","bufZ.siz","size"},{"O","bsU.utf","utf8"},{"O","bsH.he","hex"},{"O","bsS.sha2","sha256"},
  {"O","bsB.base","base64"},{"O","pthR.resol","resolve"},{"O","pthP.paren","parent"},
  {"O","pthS.segme","segments"},{"O",'encodeUtf8().sha256().he',"hex"},{"O","raw.toByteString().he","hex"},
  {"O",'decodeBase64()?.utf',"utf8"},{"O",'toPath().resolve("log").paren',"parent"},
  {"O","Buffer().snapshot().he","hex"},{"O","buf2.snapshot().sha2","sha256"},
  -- project types + inference
  {"P","basic.salu","salutation"},{"P","basicInh.loca","locale"},{"P","basicGreet.gree","greet"},
  {"P","shouter.greetLou","greetLoudly"},{"P","shouterBase.salu","salutation"},
  {"P","shouterIface.loca","locale"},{"P","BasicGreeter.defa","default"},
  {"P","BasicGreeter.DEFAULT_LOC","DEFAULT_LOCALE"},{"P","BasicGreeter.default().salu","salutation"},
  {"P",'greeterFor("en").gree',"greet"},{"P","plain.sho","shout"},{"P","word.trim().sho","shout"},
  {"P","anyShout.greetLou","greetLoudly"},{"P","anyWhen.greetLou","greetLoudly"},
  {"P","casted.greetLou","greetLoudly"},{"P","anyEarly.greetLou","greetLoudly"},
  {"P","itGreeter.gree","greet"},{"P","it.loca","locale"},{"P","itAlso.salu","salutation"},
  {"P","this.salu","salutation"},
}

-- goto checks: {file_key, line-substring, token, expected_uri_substring}
local GOTO = {
  {"S","_sxGotoReturnType(): String","String","kotlin-stdlib"},
  {"S","val gotoTargetVar","User","Model.kt"},
  {"S","val sxGotoListOf = listOf","listOf","kotlin-stdlib"},
  {"R","val gotoJsonAnchor","Json","serialization-json"},
  {"C","val gotoFlow","Flow","coroutines"},
  {"O","fun _okGotoBuffer(): Buffer","Buffer","okio"},
  {"P","probeUsesSpanishGreeter","SpanishGreeter","Greetings.kt"},
}

-- diagnostic checks: {file_key, expected unused-import name}
local DIAG = {
  {"O","Flow"},{"P","normalizeName"},{"R","coreEncodeToString"},
}

-- ---- harness ----
local id
local function ensure_open(path)
  vim.cmd("edit " .. path)
  local b = vim.api.nvim_get_current_buf()
  vim.bo[b].filetype = "kotlin"
  id = id or vim.lsp.start({ name = "ktlsp", cmd = { bin }, root_dir = root })
  vim.lsp.buf_attach_client(b, id)
  return b
end

vim.wait(10000, function()
  local first = ensure_open(files.S)
  return first ~= nil
end, 100)
vim.wait(10000, function()
  local c = vim.lsp.get_client_by_id(id); return c and c.server_capabilities ~= nil
end, 50)

local function lines(b) return vim.api.nvim_buf_get_lines(b, 0, -1, false) end
local function find_line(b, needle)
  for i, l in ipairs(lines(b)) do
    local c = l:find(needle, 1, true)
    if c then return i - 1, l, c end
  end
end
local function complete_labels(b, line, char)
  local r = vim.lsp.buf_request_sync(b, "textDocument/completion",
    { textDocument = { uri = vim.uri_from_bufnr(b) }, position = { line = line, character = char },
      context = { triggerKind = 1 } }, 5000) or {}
  local labs = {}
  for _, v in pairs(r) do
    local items = v.result and (v.result.items or v.result) or {}
    for _, it in ipairs(items) do labs[it.label] = true end
  end
  return labs
end

-- Warm up: wait for the library index (poll a known stdlib completion).
local sbuf = ensure_open(files.S)
vim.wait(150000, function()
  local ln, l = find_line(sbuf, "sx1.upper")
  if not ln then return false end
  return complete_labels(sbuf, ln, #l)["uppercase"] == true
end, 2000)

local stats = {} -- per category {pass, fail}
local fails = {}
local function bump(cat, ok) stats[cat] = stats[cat] or {0,0}; if ok then stats[cat][1]=stats[cat][1]+1 else stats[cat][2]=stats[cat][2]+1 end end

-- completion
for _, chk in ipairs(COMPL) do
  local b = ensure_open(files[chk[1]])
  local ln, l = find_line(b, chk[2])
  local ok = false
  local observed = "line-not-found"
  if ln then
    local labs = complete_labels(b, ln, #l)
    ok = labs[chk[3]] == true
    if not ok then
      local s = {}
      for k, _ in pairs(labs) do if #s < 10 then s[#s+1]=k end end
      table.sort(s)
      observed = (#s == 0) and "<none>" or table.concat(s, ",")
    end
  end
  bump(chk[1], ok)
  if not ok then fails[#fails+1] = string.format("  [%s] compl `%s` expect `%s`  observed: %s", chk[1], chk[2], chk[3], observed) end
end

-- goto
for _, g in ipairs(GOTO) do
  local b = ensure_open(files[g[1]])
  local ln, l, _ = find_line(b, g[2])
  local ok = false
  local uri = "line-not-found"
  if ln then
    local col = l:find(g[3], 1, true)
    if col then
      col = col - 1
      local r = vim.lsp.buf_request_sync(b, "textDocument/definition",
        { textDocument = { uri = vim.uri_from_bufnr(b) }, position = { line = ln, character = col } }, 5000) or {}
      uri = "<none>"
      for _, v in pairs(r) do
        local res = v.result
        if res then if res.uri then uri = res.uri elseif res[1] then uri = res[1].uri or res[1].targetUri end end
      end
      ok = uri ~= nil and uri ~= "<none>" and uri:find(g[4], 1, true) ~= nil
    end
  end
  bump("goto", ok)
  if not ok then fails[#fails+1] = string.format("  [goto] %s on `%s` expect uri~%s  observed: %s", g[1], g[3], g[4], tostring(uri)) end
end

-- diagnostics (published, debounced) — open file, wait, read vim.diagnostic
for _, d in ipairs(DIAG) do
  local b = ensure_open(files[d[1]])
  local got = vim.wait(8000, function()
    for _, di in ipairs(vim.diagnostic.get(b)) do
      if tostring(di.message):find(d[2], 1, true) then return true end
    end
    return false
  end, 200)
  bump("diag", got)
  if not got then
    local msgs = {}
    for _, di in ipairs(vim.diagnostic.get(b)) do msgs[#msgs+1] = di.message end
    fails[#fails+1] = string.format("  [diag] %s expect unused `%s`  observed: [%s]", d[1], d[2], table.concat(msgs, " | "))
  end
end

print("\n===== ktlsp COMPREHENSIVE verification (dev/gradle-sample) =====")
local order = {"S","R","C","O","P","goto","diag"}
local labelmap = {S="stdlib",R="serialization",C="coroutines",O="okio",P="project+inference",goto="goto-definition",diag="diagnostics"}
local tp, tf = 0, 0
for _, k in ipairs(order) do
  local s = stats[k] or {0,0}
  tp = tp + s[1]; tf = tf + s[2]
  print(string.format("  %-18s  %2d/%-2d pass", labelmap[k], s[1], s[1]+s[2]))
end
print(string.format("  %-18s  %2d/%-2d pass", "TOTAL", tp, tp+tf))
if #fails > 0 then
  print("\n--- FAILURES (with observed completion labels) ---")
  for _, f in ipairs(fails) do print(f) end
end
print("================================================================")
os.exit(0)

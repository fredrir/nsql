-- nsql: buffer-local wiring, loaded AFTER your own config via `-c luafile`, so
-- these win without disturbing your global setup. Everything is pcall-guarded so
-- a failure can never brick the editor.
--
--   :w             run the statement under the cursor (write = execute; :wq runs + quits)
--   q              toggle between the editor and the results window
--   ,a             run uncapped (all rows)        ,R   run on a prod profile (force past the guard)
--   ,j  /  ,c      copy the last result as JSON / CSV (OSC 52)
--   <C-x><C-o>     schema-aware completion (live tables & columns from the DB)
--   :q :wq :q! ZZ  quit, the native way (your buffer is saved for next time)
--   (in the results window: hjkl to move, y to copy clean values, q to come back)
--
-- We do NOT hijack <CR>; running is an explicit :w so a stray key never fires a query.
-- Custom (`,`) keys are reserved for FEATURES (run-variants, exports) — plain run,
-- copy and quit are all the native vim verbs you already use.

local o = { buffer = true, silent = true, desc = "nsql" }

-- SQL line-comment string for `gcc`/commentary plugins on the scratch buffer.
vim.bo.commentstring = "-- %s"

pcall(function()
  vim.g.nsql_ebuf = vim.api.nvim_get_current_buf()
  vim.g.nsql_ewin = vim.api.nvim_get_current_win()

  -- nsql sets g:nsql_chan to its RPC channel right after attaching (the channel
  -- can't be discovered reliably from inside a startup luafile, so we read it
  -- lazily at fire time).
  local function notify(method, payload)
    local ch = vim.g.nsql_chan
    if ch then vim.rpcnotify(ch, method, payload) end
  end

  -- The statement under the cursor: split the buffer on top-level ';' (naive — good
  -- enough for the sidetrack case; a single-statement buffer returns whole).
  local function stmt_under_cursor()
    local lines = vim.api.nvim_buf_get_lines(0, 0, -1, false)
    local text = table.concat(lines, "\n")
    local cur = vim.api.nvim_win_get_cursor(0)
    local off = cur[2] -- 0-indexed byte column
    for i = 1, cur[1] - 1 do
      off = off + #lines[i] + 1
    end
    local s = 1
    for i = 1, #text do
      if text:byte(i) == 59 then -- ';'
        if i - 1 >= off then
          return text:sub(s, i)
        end
        s = i + 1
      end
    end
    return text:sub(s)
  end

  local function run(opts)
    opts = opts or {}
    local sql = opts.sql or stmt_under_cursor()
    if sql == nil or sql:match("^%s*$") then
      -- cursor past the last statement / empty pick: fall back to whole buffer
      sql = table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), "\n")
    end
    notify("nsql_run", {
      sql = sql,
      force = opts.force or false,
      all = opts.all or false,
    })
  end

  -- Run is the native :w (write = execute). `:wq` runs then quits.
  vim.api.nvim_create_autocmd("BufWritePost", {
    buffer = 0,
    callback = function()
      if not vim.g.nsql_tearing then
        run()
      end
    end,
  })

  -- Custom keys are reserved for FEATURES: run-variants and exports.
  vim.keymap.set("n", ",a", function() run({ all = true }) end, o) -- run all rows
  vim.keymap.set("n", ",R", function() run({ force = true }) end, o) -- force past the prod guard
  vim.keymap.set("x", ",r", function() -- run exactly the selection
    vim.cmd('normal! "vy')
    run({ sql = vim.fn.getreg("v") })
  end, o)
  vim.keymap.set("n", ",j", function() notify("nsql_export", "json") end, o)
  vim.keymap.set("n", ",c", function() notify("nsql_export", "csv") end, o)

  -- q toggles into the results window (a real buffer: hjkl, visual-select, y to
  -- copy clean values; q there comes back). Native <C-w>j / <C-w>k work too.
  vim.keymap.set("n", "q", function()
    local w = vim.g.nsql_rwin
    if w and vim.api.nvim_win_is_valid(w) then
      pcall(vim.api.nvim_set_current_win, w)
    end
  end, o)

  -- Schema-aware completion. nsql fills _G.nsql_schema from the LIVE database in
  -- the background. One shared context analyser feeds three consumers below:
  -- (1) a blink.cmp source, (2) the omnifunc, (3) a vanilla auto-popup. It completes
  -- tables after FROM/JOIN/INTO/UPDATE, a table's columns after `tbl.`, and both
  -- otherwise — so `select name from cat` completes `cat` (table) and `name` (column).
  -- kind = "table" | "column"; the caller maps that to its own UI.
  function _G.nsql_complete_words()
    local schema = _G.nsql_schema
    if not schema then return {} end
    local col = vim.api.nvim_win_get_cursor(0)[2]
    local before = vim.api.nvim_get_current_line():sub(1, col):lower()
    local out = {}
    local dot = before:match("([%w_]+)%.[%w_]*$")
    if dot and schema.by_table and schema.by_table[dot] then
      for _, c in ipairs(schema.by_table[dot]) do out[#out + 1] = { word = c, kind = "column" } end
    elseif before:match("%f[%w]from%s+[%w_]*$")
      or before:match("%f[%w]join%s+[%w_]*$")
      or before:match("%f[%w]into%s+[%w_]*$")
      or before:match("%f[%w]update%s+[%w_]*$")
    then
      for _, t in ipairs(schema.tables or {}) do out[#out + 1] = { word = t, kind = "table" } end
    else
      for _, t in ipairs(schema.tables or {}) do out[#out + 1] = { word = t, kind = "table" } end
      for _, c in ipairs(schema.columns or {}) do out[#out + 1] = { word = c, kind = "column" } end
    end
    return out
  end

  -- (2) omnifunc — for <C-x><C-o> and any engine using the 'omni' source.
  function _G.NsqlOmni(findstart, base)
    if findstart == 1 then
      local line = vim.api.nvim_get_current_line()
      local col = vim.api.nvim_win_get_cursor(0)[2]
      local s = col
      while s > 0 and line:sub(s, s):match("[%w_]") do
        s = s - 1
      end
      return s
    end
    local b, out, seen = (base or ""):lower(), {}, {}
    for _, it in ipairs(_G.nsql_complete_words()) do
      if not seen[it.word] and (b == "" or it.word:lower():sub(1, #b) == b) then
        seen[it.word] = true
        out[#out + 1] = { word = it.word, menu = "[" .. it.kind .. "]" }
      end
    end
    return out
  end
  vim.bo.omnifunc = "v:lua.NsqlOmni"

  -- (1) blink.cmp source — registered after startup (blink loads on VimEnter), so
  -- tables/columns auto-pop in blink's own UI. blink fuzzy-matches, so we hand it
  -- the full context-appropriate list and let it filter.
  vim.schedule(function()
    local ok, blink = pcall(require, "blink.cmp")
    if not ok or type(blink.add_source_provider) ~= "function" then return end
    local KIND = { table = 7, column = 5 } -- LSP CompletionItemKind: Class / Field
    local src = {}
    function src.new() return setmetatable({}, { __index = src }) end
    function src:get_trigger_characters() return { "." } end
    function src:get_completions(_, callback)
      local items = {}
      for _, it in ipairs(_G.nsql_complete_words()) do
        items[#items + 1] = {
          label = it.word,
          kind = KIND[it.kind] or 1,
          labelDetails = { description = it.kind },
        }
      end
      callback({ is_incomplete_forward = false, is_incomplete_backward = false, items = items })
      return function() end
    end
    package.loaded["nsql._blink"] = src
    pcall(blink.add_source_provider, "nsql", { name = "nsql", module = "nsql._blink", score_offset = 5 })
    pcall(function()
      require("blink.cmp.sources.lib").add_filetype_provider_id("sql", "nsql")
    end)
  end)

  -- :NsqlSchema — diagnostics: did the background introspection load anything?
  pcall(vim.api.nvim_create_user_command, "NsqlSchema", function()
    local s = _G.nsql_schema
    if not s then
      vim.notify("nsql: schema not loaded yet (introspection pending or failed)", vim.log.levels.WARN)
    else
      vim.notify(("nsql: %d tables, %d columns loaded"):format(#(s.tables or {}), #(s.columns or {})))
    end
  end, {})

  -- (No auto-firing of completion: feeding <C-x><C-o> on every keystroke fights the
  -- way you type whole identifiers and can double characters. Completion is on
  -- DEMAND — <C-x><C-o> — or automatic through your engine's own UI via the source
  -- above. A vanilla-nvim auto-popup would need careful completeopt handling; out
  -- for now so typing is never disturbed.)

  -- Native quit (:q / :wq / :q! / ZZ) ends the WHOLE session. The results split
  -- auto-closes when it becomes the last window, so quitting the editor leaves
  -- only the results window — which then quits itself and nvim exits. (The
  -- quickfix / NERDTree pattern: WinEnter after the close, no textlock, no
  -- scheduling.) The scratch resumes from your last :w (so :wq / ZZ persist
  -- exactly what you ran).
  vim.api.nvim_create_autocmd("WinEnter", {
    callback = function()
      if vim.g.nsql_rbuf
        and vim.api.nvim_get_current_buf() == vim.g.nsql_rbuf
        and #vim.api.nvim_list_wins() == 1
      then
        pcall(vim.cmd, "quit")
      end
    end,
  })
end)

-- The MAIN HEADER (coloured badges: db name · SAFE · PROD · ,h help · ,i info) lives
-- in nvim's statusline. There are at most TWO bars and the main header never changes
-- content — it just MOVES: it's the editor statusline when there's no output, and
-- moves to the bottom (results) statusline once a table shows (the editor statusline
-- then becomes the sticky column header). No bar above the editor.
pcall(function()
  vim.opt.shortmess:append("WFI") -- no "written", no file-info intro, no intro
  vim.o.laststatus = 2 -- the main header is always shown (1 window) → bottom (2 windows)
  vim.o.ruler = false

  local db = vim.env.NSQL_DB or "db"
  vim.g.nsql_db = db
  vim.g.nsql_url = vim.env.NSQL_URL or ""
  local prod = vim.env.NSQL_PROD == "1"
  local safe = vim.env.NSQL_SAFE == "1"
  vim.g.nsql_prod = prod and 1 or 0
  vim.g.nsql_safe = safe and 1 or 0

  -- theme-independent badge colours (override NsqlDb/NsqlSafe/NsqlProd to taste).
  pcall(vim.api.nvim_set_hl, 0, "NsqlDb", { fg = "#1a1b26", bg = "#7aa2f7", bold = true, default = true })
  pcall(vim.api.nvim_set_hl, 0, "NsqlSafe", { fg = "#1a1b26", bg = "#9ece6a", bold = true, default = true })
  pcall(vim.api.nvim_set_hl, 0, "NsqlProd", { fg = "#1a1b26", bg = "#f7768e", bold = true, default = true })

  local function esc(s) return (s:gsub("%%", "%%%%")) end
  local bar = "%#NsqlDb# " .. esc(db) .. " %*"
  if safe then bar = bar .. " %#NsqlSafe# SAFE %*" end
  if prod then bar = bar .. " %#NsqlProd# PROD %*" end
  bar = bar .. "%=,h help · ,i info "
  vim.g.nsql_mainbar = bar
  vim.wo.statusline = bar -- the editor statusline starts as the main header
end)

-- ,h / ,i menus: keep the bar clean; surface keys + connection info on demand in a
-- small float (plain nvim, works anywhere). q / <Esc> dismiss.
pcall(function()
  local function float(title, lines)
    local w = #title + 2
    for _, l in ipairs(lines) do w = math.max(w, vim.fn.strdisplaywidth(l) + 2) end
    local buf = vim.api.nvim_create_buf(false, true)
    vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
    vim.bo[buf].modifiable = false
    vim.bo[buf].bufhidden = "wipe"
    local win = vim.api.nvim_open_win(buf, true, {
      relative = "editor",
      width = w,
      height = #lines,
      row = math.max(0, math.floor((vim.o.lines - #lines) / 2)),
      col = math.max(0, math.floor((vim.o.columns - w) / 2)),
      style = "minimal",
      border = "rounded",
      title = " " .. title .. " ",
      title_pos = "center",
    })
    pcall(function() vim.wo[win].winhl = "Normal:NormalFloat,FloatBorder:FloatBorder" end)
    local bo = { buffer = buf, silent = true }
    vim.keymap.set("n", "q", "<cmd>close<cr>", bo)
    vim.keymap.set("n", "<Esc>", "<cmd>close<cr>", bo)
  end

  vim.keymap.set("n", ",h", function()
    float("nsql · keys", {
      "  :w            run the statement under the cursor (:wq runs + quits)",
      "  q             toggle between the editor and the results window",
      "  <C-x><C-o>    schema-aware completion (tables & columns)",
      "  ,a            run uncapped (all rows)",
      "  ,R            run on a prod profile (force past the guard)",
      "  ,j  /  ,c     copy the last result as JSON / CSV",
      "  ,h  /  ,i     this help  /  connection info",
      "  :q :wq :q! ZZ quit (your buffer is saved for next time)",
    })
  end, o)

  vim.keymap.set("n", ",i", function()
    local s = _G.nsql_schema
    local schema = s and (("%d tables, %d columns"):format(#(s.tables or {}), #(s.columns or {})))
      or "loading…"
    local mode = (vim.g.nsql_safe == 1) and "read-only (SAFE)" or "read / write"
    float("nsql · connection", {
      "  database   " .. (vim.g.nsql_db or "?"),
      "  url        " .. (vim.g.nsql_url or "?"),
      "  mode       " .. mode .. ((vim.g.nsql_prod == 1) and "   ⚠ PROD" or ""),
      "  schema     " .. schema,
    })
  end, o)
end)

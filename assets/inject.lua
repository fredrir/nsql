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

  -- Schema-aware omni-completion (<C-x><C-o>). nsql fills _G.nsql_schema from the
  -- LIVE database in the background; this function reads it and completes tables
  -- after FROM/JOIN/INTO/UPDATE, a table's columns after `tbl.`, and both
  -- otherwise — so `select name from cat` completes `cat` (table) and `name`
  -- (column). Plays nice with completion engines that use the 'omni' source.
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
    local schema = _G.nsql_schema
    if not schema then
      return {}
    end
    local col = vim.api.nvim_win_get_cursor(0)[2]
    local before = vim.api.nvim_get_current_line():sub(1, col):lower()
    local b = (base or ""):lower()
    local out, seen = {}, {}
    local function add(word, menu)
      if word and not seen[word] and (b == "" or word:lower():sub(1, #b) == b) then
        seen[word] = true
        table.insert(out, { word = word, menu = menu })
      end
    end
    local dot = before:match("([%w_]+)%.[%w_]*$")
    if dot and schema.by_table and schema.by_table[dot] then
      for _, c in ipairs(schema.by_table[dot]) do add(c, "[col]") end
    elseif before:match("%f[%w]from%s+[%w_]*$")
      or before:match("%f[%w]join%s+[%w_]*$")
      or before:match("%f[%w]into%s+[%w_]*$")
      or before:match("%f[%w]update%s+[%w_]*$")
    then
      for _, t in ipairs(schema.tables or {}) do add(t, "[table]") end
    else
      for _, t in ipairs(schema.tables or {}) do add(t, "[table]") end
      for _, c in ipairs(schema.columns or {}) do add(c, "[col]") end
    end
    return out
  end
  vim.bo.omnifunc = "v:lua.NsqlOmni"

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

-- Status bars: nsql's NATIVE statuslines. Initially the editor statusline is the
-- main bar (connection; prod in red). Once a result is shown, nsql flips the roles
-- (editor bar → sticky column header, bottom bar → connection). We also hide the
-- temp-file path and the "N lines written" noise.
pcall(function()
  vim.opt.shortmess:append("WFI") -- no "written", no file-info intro, no intro
  vim.o.laststatus = 2 -- ensure the statusline (divider) is always shown
  vim.o.ruler = false

  local status = vim.env.NSQL_STATUS or "nsql"
  local prod = vim.env.NSQL_PROD == "1"
  vim.g.nsql_conn = status
  vim.g.nsql_prod = prod and 1 or 0
  if prod then
    pcall(vim.api.nvim_set_hl, 0, "NsqlProd", { fg = "#ff5555", bold = true })
  end

  local function esc(s)
    return (s:gsub("%%", "%%%%")) -- % is special in 'statusline'
  end
  local connbar = prod and ("%#NsqlProd# PROD %* " .. esc(status)) or (" " .. esc(status))
  vim.wo.statusline = connbar .. "%= :w run · q results"
end)

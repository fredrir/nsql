-- nsql: buffer-local wiring, loaded AFTER your own config via `-c luafile`, so
-- these win without disturbing your global setup. Everything is pcall-guarded so
-- a failure can never brick the editor.
--
--   ,r  = run the statement under the cursor (results in the bottom pane)
--   ,R  = run it even on a prod profile (force past the safety guard)
--   ,a  = run uncapped (all rows)
--   ,y  = copy the last result to the clipboard (TSV, via OSC 52)
--   ,,  = quit (saves your buffer for next time)   ,q = quit
--   :w  also previews (runs the statement under the cursor) without quitting
--
-- We do NOT hijack <CR>. Run/quit are explicit so a stray key never fires a query.

local o = { buffer = true, silent = true, desc = "nsql" }

-- SQL line-comment string for `gcc`/commentary plugins on the scratch buffer.
vim.bo.commentstring = "-- %s"

pcall(function()
  -- nsql sets g:nsql_chan to its RPC channel right after attaching (the channel
  -- can't be discovered reliably from inside a startup luafile, so we read it
  -- lazily at fire time).
  local function notify(method, payload)
    local ch = vim.g.nsql_chan
    if ch then
      vim.rpcnotify(ch, method, payload)
    end
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

  vim.keymap.set("n", ",r", function() run() end, o)
  vim.keymap.set("n", ",R", function() run({ force = true }) end, o)
  vim.keymap.set("n", ",a", function() run({ all = true }) end, o)
  vim.keymap.set("n", ",y", function() notify("nsql_copy") end, o)

  -- Visual ,r: run exactly the selection.
  vim.keymap.set("x", ",r", function()
    vim.cmd('normal! "vy')
    run({ sql = vim.fn.getreg("v") })
  end, o)

  -- :w previews (runs) without quitting — unless we're on our way out.
  vim.api.nvim_create_autocmd("BufWritePost", {
    buffer = 0,
    callback = function()
      if not vim.b.nsql_quitting then
        run()
      end
    end,
  })

  -- Quit: persist the buffer (write) but suppress the run-on-write, then leave.
  local function quit(cmd)
    vim.b.nsql_quitting = true
    pcall(vim.cmd, "silent! write")
    vim.cmd(cmd)
  end
  vim.keymap.set("n", ",,", function() quit("quit") end, o)
  vim.keymap.set("n", ",q", function() quit("quit") end, o)
end)

-- Show the active connection + key hints in nvim's NATIVE statusline (the bar at
-- the bottom of the editor window — which also serves as the divider above the
-- results pane). Prod connections are red. Also hide the temp-file path and the
-- "N lines written" noise (where the scratch lives is not useful info).
pcall(function()
  vim.opt.shortmess:append("WFI") -- no "written", no file-info intro, no intro
  vim.o.laststatus = 2 -- ensure the statusline (divider) is always shown
  vim.o.ruler = false

  local status = vim.env.NSQL_STATUS or "nsql"
  local prod = vim.env.NSQL_PROD == "1"
  local function esc(s)
    return (s:gsub("%%", "%%%%")) -- % is special in 'statusline'
  end
  local keys = ",r run  ,y copy  ,, quit "
  if prod then
    pcall(vim.api.nvim_set_hl, 0, "NsqlProd", { fg = "#ff5555", bold = true })
    vim.wo.statusline = "%#NsqlProd# PROD %* " .. esc(status) .. "%=" .. esc(keys)
  else
    vim.wo.statusline = " " .. esc(status) .. "%=" .. esc(keys)
  end
end)

-- nsql: buffer-local keymaps, loaded AFTER your own config via `-c luafile`,
-- so these win without disturbing your global setup.
--
--   ,,  = write + run     (saves the buffer and exits 0 -> nsql runs it)
--   ,q  = cancel          (exits non-zero -> nsql runs nothing)
--
-- We deliberately do NOT hijack <CR> (Enter is reflexive cursor movement and a
-- stray Enter must never fire a query at prod). `:wq` also runs and `:cq`
-- cancels, so this works even if these maps are unavailable.

local o = { buffer = true, silent = true, desc = "nsql" }

vim.keymap.set("n", ",,", "<Cmd>write<CR><Cmd>quit<CR>", o)
vim.keymap.set("n", ",q", "<Cmd>cquit<CR>", o)

-- SQL line-comment string for `gcc`/commentary plugins on the scratch buffer.
vim.bo.commentstring = "-- %s"

-- Show the active connection + key hints as VIRTUAL TEXT above the first line.
-- It is not buffer content, so the buffer opens fully clean and the hint is
-- never saved or executed. Wrapped in pcall so it can never break the editor.
pcall(function()
  local status = vim.env.NSQL_STATUS or "nsql"
  local ns = vim.api.nvim_create_namespace("nsql")
  vim.api.nvim_buf_set_extmark(0, ns, 0, 0, {
    virt_lines_above = true,
    virt_lines = { { { status .. "   ·   ,, run · ,q cancel", "Comment" } } },
  })
end)

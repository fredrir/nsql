-- nsql portable nvim config — a minimal, self-contained setup used over SSH or with
-- --clean (passed via `nvim -u`), so nsql behaves IDENTICALLY regardless of the box's
-- own nvim config. It loads NONE of your plugins/init; nsql's own inject.lua (added
-- via `-c luafile`) supplies every feature (completion, schema highlighting, keymaps,
-- the ,h/,i menus) — all of which have config-independent baselines.

vim.opt.swapfile = false
vim.opt.shadafile = "NONE"
vim.opt.termguicolors = true -- 24-bit colour for syntax + nsql's type/schema highlights
vim.opt.number = false
vim.opt.signcolumn = "no"
vim.opt.mouse = "a"

-- Enable nvim's BUNDLED filetype + syntax (gives SQL keyword/string highlighting
-- without any plugin). Treesitter is used if a parser happens to be present.
pcall(function()
  vim.cmd("filetype plugin indent on")
  vim.cmd("syntax enable")
end)

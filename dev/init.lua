---@diagnostic disable: undefined-global
-- Throwaway Neovim config to try ktlsp interactively (no plugins, ignores your real config).
--
--   nvim -u /Users/pepe/projects/github.com/pepegar/ktlsp/dev/init.lua  path/to/SomeFile.kt
--
-- In a Kotlin buffer:  gd = goto-definition,  gR = find-references  (grr also works on nvim 0.11+).

-- Find the ktlsp binary relative to this file (repo root = dev/..).
local this = debug.getinfo(1, "S").source:gsub("^@", "")
local repo = vim.fn.fnamemodify(this, ":p:h:h")
local bin = repo .. "/target/release/ktlsp"
if vim.fn.filereadable(bin) == 0 then
  bin = repo .. "/target/debug/ktlsp"
end

vim.filetype.add({ extension = { kt = "kotlin", kts = "kotlin" } })

-- Start ktlsp for each Kotlin buffer, rooted at the project (so its version catalog is indexed).
vim.api.nvim_create_autocmd("FileType", {
  pattern = "kotlin",
  callback = function(args)
    if vim.fn.filereadable(bin) == 0 then
      vim.notify("ktlsp not built — run `cargo build --release` in " .. repo, vim.log.levels.ERROR)
      return
    end
    local fname = vim.api.nvim_buf_get_name(args.buf)
    local marker = vim.fs.find(
      { "settings.gradle.kts", "settings.gradle", "build.gradle.kts", "build.gradle", ".git" },
      { upward = true, path = fname }
    )[1]
    local root = marker and vim.fs.dirname(marker) or vim.fs.dirname(fname)
    vim.lsp.start({ name = "ktlsp", cmd = { bin }, root_dir = root })
  end,
})

-- Convenient keymaps once attached.
vim.api.nvim_create_autocmd("LspAttach", {
  callback = function(args)
    local opts = { buffer = args.buf, silent = true }
    vim.keymap.set("n", "gd", vim.lsp.buf.definition, opts)
    vim.keymap.set("n", "gR", vim.lsp.buf.references, opts)
    vim.notify("ktlsp attached — gd = definition, gR = references")
  end,
})

vim.notify("ktlsp init loaded (binary: " .. bin .. ")")

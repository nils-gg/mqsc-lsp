-- Example Neovim setup for mqsc-lsp (Neovim >= 0.10; vim.filetype.add
-- arrived in 0.10, vim.lsp.start already existed in 0.8).
-- Drop this into your init.lua / lazy.nvim config and adjust the binary path.

local mqsc_bin = vim.fn.expand("~/projects/mqsc-lsp/server/target/release/mqsc-lsp") -- EDIT ME

-- 1. Teach Neovim what an MQSC file is.
vim.filetype.add({
	extension = { mqsc = "mqsc" },
	pattern = { [".*/mqsc/.*"] = "mqsc" }, -- e.g. mqsc/ scripts in a queue-manager folder
})

-- 2. Start the server for every MQSC buffer.
vim.api.nvim_create_autocmd("FileType", {
	pattern = "mqsc",
	callback = function()
		vim.lsp.start({
			name = "mqsc-lsp",
			cmd = { mqsc_bin },
			root_dir = vim.fs.dirname(vim.fs.find({ ".git", "mqsc-lsp" }, { upward = true })[1])
				or vim.fn.getcwd(),
			-- Optional per-code diagnostic overrides (see README "Configuration"):
			settings = {
				diagnostics = { UNDEFINED_OBJECT = "off", MISSING_REQUIRED = "warning" },
			},
			capabilities = require("vim.lsp.protocol").make_client_capabilities(),
		})
	end,
})

-- 3. Optional: nicer hover in a split instead of a floating window.
-- vim.keymap.set("n", "K", function() vim.lsp.buf.hover({ border = "rounded" }) end)

-- NOTE: with lazy.nvim you can also wrap the above in an `ft = "mqsc"` plugin
-- spec. nvim-lspconfig has no built-in mqsc server definition yet; if you use
-- lspconfig, register it via require("lspconfig.util").base_server or
-- on_setup — the capabilities advertised by the server cover hover,
-- completion, code actions, documentSymbol, foldingRange, documentHighlight
-- and semanticTokens.

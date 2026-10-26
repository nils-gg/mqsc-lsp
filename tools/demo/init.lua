-- Deterministic minimal config for the recorded demo (tools/record_demo.sh).
-- Not a user example -- see editors/nvim/init.example.lua for that.

local bin = assert(vim.env.MQSC_LSP_BIN, "MQSC_LSP_BIN not set")

vim.opt.termguicolors = true
vim.opt.number = true
vim.opt.signcolumn = "yes"
vim.opt.showmode = false
vim.opt.swapfile = false
vim.opt.backup = false
vim.opt.undofile = false
vim.opt.foldcolumn = "1"
vim.opt.foldmethod = "expr"
vim.opt.foldexpr = "v:lua.vim.lsp.foldexpr()"
vim.opt.foldlevel = 99 -- start open; the fold step closes them with zM
vim.opt.completeopt = "menu,menuone"
vim.opt.iskeyword:append(".") -- dot-separated MQSC names are single words
pcall(vim.cmd, "colorscheme habamax")

vim.diagnostic.config({ virtual_text = { prefix = ">>" }, severity_sort = true })

vim.filetype.add({ extension = { mqsc = "mqsc" } })
vim.api.nvim_create_autocmd("FileType", {
	pattern = "mqsc",
	callback = function()
		vim.lsp.start({ name = "mqsc-lsp", cmd = { bin }, root_dir = vim.fn.getcwd() })
		local buf = 0
		vim.keymap.set("n", "<Space>ca", vim.lsp.buf.code_action, { buffer = buf, desc = "code action" })
		vim.keymap.set("n", "<Space>rn", vim.lsp.buf.rename, { buffer = buf, desc = "rename" })
		vim.keymap.set("n", "<Space>gr", function()
			vim.lsp.buf.references()
			vim.schedule(function() pcall(vim.cmd, "botright copen 7") end)
		end, { buffer = buf, desc = "references" })
		vim.keymap.set("n", "gl", function() vim.diagnostic.open_float({ scope = "line" }) end,
			{ buffer = buf, desc = "full message float" })
	end,
})

-- Native completion wiring for <= 0.11: after typing a trigger character
-- (the server declares " ", "(", "," ":") or after <C-x><C-o>, completion
-- menu comes from the server. (0.12's vim.lsp.completion.trigger does not
-- exist here, hence the omnifunc key in tools/record_demo.sh.)
vim.api.nvim_create_autocmd("LspAttach", {
	callback = function(ev)
		local client = vim.lsp.get_client_by_id(ev.data.client_id)
		if client and client.server_capabilities.completionProvider then
			vim.lsp.completion.enable(true, client.id, ev.buf)
		end
	end,
})

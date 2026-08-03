--%includefile group_selected("neovim")
-- You must enable the exrc setting in neovim for this config file to be used.
local rust_analyzer = {
    cargo = {
        target = "{{ rust_target }}",
        allTargets = false,
        --%if is_xtensa
        --REPLACE esp rust_toolchain
        extraEnv = { RUSTUP_TOOLCHAIN = "esp" },
        --%endif
    },
}

--%if option("neovim-rustaceanvim")
-- Note the rustaceanvim name of the language server is rust-analyzer with a dash.
vim.lsp.config("rust-analyzer", {
--%else
-- Note the neovim name of the language server is rust_analyzer with an underscore.
vim.lsp.config("rust_analyzer", {
--%endif
--%if is_xtensa
	cmd = { "rustup", "run", "stable", "rust-analyzer" },
--%endif
    settings = {
        ["rust-analyzer"] = rust_analyzer,
    },
})

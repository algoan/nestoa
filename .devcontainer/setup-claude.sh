#! /bin/bash

# Install the latest version of the Claude Code CLI
# with the TypeScript LSP plugin and its dependencies.
npm install -g @anthropic-ai/claude-code@latest typescript-language-server typescript

# The Rust LSP server. Installed as a rustup component so it always matches the
# toolchain the rust devcontainer feature pinned, rather than drifting from it.
rustup component add rust-analyzer

# Set up the Claude Code CLI plugins. The analyser is Rust (crates/nestoa), the
# npm shim and the input fixtures under codebase/ are TypeScript, so both LSPs
# are worth having. Re-running these is a no-op on an already-set-up container.
claude plugin marketplace update claude-plugins-official
claude plugin install typescript-lsp@claude-plugins-official
claude plugin install rust-analyzer-lsp@claude-plugins-official

# List the installed plugins to verify the installation.
claude plugin list

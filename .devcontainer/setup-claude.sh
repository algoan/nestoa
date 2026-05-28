#! /bin/bash

# Install the latest version of the Claude Code CLI
# with the TypeScript LSP plugin and its dependencies.
npm install -g @anthropic-ai/claude-code@latest typescript-language-server typescript

# Set up the Claude Code CLI plugins.
claude plugin marketplace update claude-plugins-official
claude plugin install typescript-lsp@claude-plugins-official

# Install code-review-graph via pip (MCP configured via .mcp.json, not as a Claude plugin)
# https://github.com/tirth8205/code-review-graph
if ! command -v code-review-graph &> /dev/null; then
  sudo apt-get update -qq && sudo apt-get install -y -qq python3-pip > /dev/null
  pip3 install --break-system-packages code-review-graph
fi

# List the installed plugins to verify the installation.
claude plugin list

# Rebuild code-review-graph (retry once after clearing stale DB if it fails)
if ! code-review-graph build; then
  rm -fr .code-review-graph
  code-review-graph build
fi

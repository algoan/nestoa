# Makefile for nestoa — mirrors .github/workflows/ci.yml so the same
# checks and builds can be run locally.

CARGO_TERM_COLOR ?= always
RUST_BACKTRACE ?= 1
export CARGO_TERM_COLOR
export RUST_BACKTRACE

.DEFAULT_GOAL := help

.PHONY: help ci check test build build-debug lint fmt fmt-fix clippy bench clean

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

## --- Everything CI runs ---

ci: check ## Run the full CI suite (lint + test + release build)

check: lint test build ## Alias for the full CI suite

## --- test job (ci.yml) ---

test: ## Run the test suite (cargo test --workspace --locked)
	cargo test --workspace --locked

build: ## Build the optimized release binary (cargo build --release --workspace --locked)
	cargo build --release --workspace --locked

build-debug: ## Build the debug binary
	cargo build --workspace --locked

## --- lint job (ci.yml) ---

lint: fmt clippy ## Run format check + clippy

fmt: ## Check formatting (cargo fmt --all -- --check)
	cargo fmt --all -- --check

fmt-fix: ## Apply formatting fixes
	cargo fmt --all

clippy: ## Run clippy, denying warnings
	cargo clippy --workspace --all-targets -- -D warnings

## --- benchmark (benchmark.yml) ---

BENCHMARK_DIR ?= ../nestjs-openapi-generation-benchmark
export BENCHMARK_DIR

bench: build ## Check against the benchmark; BASELINE=<binary> to fail on a regression
	scripts/benchmark.sh target/release/nestoa $(BASELINE)

## --- housekeeping ---

clean: ## Remove build artifacts
	cargo clean

# Makefile for nestoa — mirrors .github/workflows/ci.yml so the same
# checks and builds can be run locally.

CARGO_TERM_COLOR ?= always
RUST_BACKTRACE ?= 1
export CARGO_TERM_COLOR
export RUST_BACKTRACE

.DEFAULT_GOAL := help

.PHONY: help ci check test npm-test smoke build build-debug lint fmt fmt-fix clippy bench clean

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

## --- Everything CI runs ---

ci: check ## Run the full CI suite (lint + test + npm-test + release build + smoke)

check: lint test npm-test build smoke ## Alias for the full CI suite

## --- test job (ci.yml) ---

test: ## Run the test suite (cargo test --workspace --locked)
	cargo test --workspace --locked

npm-test: ## Test the npm packaging scripts (node --test)
	node --test 'npm/test/*.test.js'

## --- smoke job (ci.yml) ---

SMOKE_TAG = $(shell node -p "require('./npm/nestoa/install.js').platformTag()")

smoke: build ## Pack the npm packages for this machine, install them and run the CLI
	node npm/build-platform-packages.js $(SMOKE_TAG) target/release/nestoa$(if $(filter win32%,$(SMOKE_TAG)),.exe)
	node npm/smoke-test.js npm/platforms/$(SMOKE_TAG)

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
	scripts/benchmark.sh target/release/nestoa "$(BASELINE)"

## --- housekeeping ---

clean: ## Remove build artifacts
	cargo clean

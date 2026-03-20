# duvm - Distributed Unified Virtual Memory
#
# Top-level Makefile with easy-to-use commands.
# Run `make help` for a list of available targets.

CARGO   ?= cargo
CLIPPY  ?= cargo clippy
FMT     ?= cargo fmt
KDIR    ?= /lib/modules/$(shell uname -r)/build

.PHONY: help build test clippy fmt check clean kmod kmod-clean install doc

##@ General

help: ## Show this help message
	@awk 'BEGIN {FS = ":.*##"; printf "\n\033[1mUsage:\033[0m\n  make \033[36m<target>\033[0m\n"} \
		/^[a-zA-Z_-]+:.*?##/ { printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2 } \
		/^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) }' $(MAKEFILE_LIST)

##@ Build

build: ## Build all Rust crates (debug mode)
	$(CARGO) build

release: ## Build all Rust crates (release mode, optimized)
	$(CARGO) build --release

##@ Test

test: ## Run all tests (unit + integration)
	$(CARGO) test

test-unit: ## Run unit tests only
	$(CARGO) test --lib

test-integration: ## Run integration tests only
	$(CARGO) test --test integration

test-verbose: ## Run all tests with verbose output
	$(CARGO) test -- --nocapture

##@ Quality

clippy: ## Run clippy linter
	$(CLIPPY) --all-targets --all-features -- -D warnings

fmt: ## Format all Rust code
	$(FMT) --all

fmt-check: ## Check formatting without changes
	$(FMT) --all -- --check

check: fmt-check clippy test ## Run all checks (format, lint, test)
	@echo "All checks passed!"

##@ Kernel Module

kmod: ## Build the kernel module (requires kernel headers)
	$(MAKE) -C duvm-kmod

kmod-clean: ## Clean kernel module build artifacts
	$(MAKE) -C duvm-kmod clean

##@ Documentation

doc: ## Generate Rust documentation
	$(CARGO) doc --no-deps --workspace

doc-open: ## Generate and open documentation in browser
	$(CARGO) doc --no-deps --workspace --open

##@ Install

install: release ## Install duvm-daemon and duvm-ctl to ~/.cargo/bin
	$(CARGO) install --path crates/duvm-daemon
	$(CARGO) install --path crates/duvm-ctl

##@ Cleanup

clean: ## Clean all build artifacts
	$(CARGO) clean
	$(MAKE) -C duvm-kmod clean 2>/dev/null || true

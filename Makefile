.DEFAULT_GOAL := help

CARGO ?= cargo

MSRV ?= 1.85.0

.PHONY: help fmt fmt-check check clippy test doc build release verify msrv clean

help: ## Show available targets
	@awk 'BEGIN {FS = ":.*## "; printf "Foremerge development targets:\n"} /^[a-zA-Z_-]+:.*## / {printf "  %-12s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

fmt: ## Format Rust sources
	$(CARGO) fmt --all

fmt-check: ## Check Rust formatting without changing files
	$(CARGO) fmt --all -- --check

check: ## Type-check every target and feature
	$(CARGO) check --workspace --all-targets --all-features

clippy: ## Run Clippy and fail on warnings
	$(CARGO) clippy --workspace --all-targets --all-features -- -D warnings

test: ## Run all tests
	$(CARGO) test --workspace --all-targets --all-features

doc: ## Build documentation and fail on warnings
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc --workspace --all-features --no-deps

build: ## Build debug binaries
	$(CARGO) build --workspace --all-targets --all-features

release: ## Build an optimized release binary
	$(CARGO) build --workspace --all-features --release

msrv: ## Run clippy and tests on the pinned MSRV toolchain, as CI does
	rustup toolchain list | grep -q '^$(MSRV)' || rustup toolchain install $(MSRV) --profile minimal --component clippy,rustfmt
	rustup run $(MSRV) $(CARGO) clippy --workspace --all-targets --all-features -- -D warnings
	rustup run $(MSRV) $(CARGO) test --workspace --all-targets --all-features

verify: fmt-check check clippy test doc ## Run the local release gate

clean: ## Remove Cargo build output
	$(CARGO) clean

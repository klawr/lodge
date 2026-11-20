PREFIX ?= /usr/local/bin
BINDIR  = $(PREFIX)

.DEFAULT_GOAL := help
.PHONY: help all build install uninstall clean dev test lint fmt check integration deny hooks schema

help: ## Show this list
	@awk 'BEGIN {FS = ":.*## "} /^[a-z-]+:.*## / {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

all: build

build: ## Release build (target/release/ldg)
	cargo build --release

test: ## Unit tests (no docker needed)
	cargo test

lint: ## Clippy with warnings as errors
	cargo clippy --all-targets -- -D warnings

fmt: ## Format the code
	cargo fmt

check: ## What CI gates on: fmt, clippy, unit tests
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test

# Needs a running docker daemon (with compose), patchelf, tmux and ssh. Quiet on success.
integration: ## The real binary against docker (~30 s); narrow with T=name
	@cargo test --test integration --quiet -- --ignored $(T)

schema: ## Regenerate docs/config.schema.json from the config types
	UPDATE_SCHEMA=1 cargo test --quiet the_config_schema_is_current

deny: ## Licenses, advisories and sources of dependencies (needs cargo-deny)
	cargo deny check --config .github/deny.toml

hooks: ## Install a git pre-push hook that runs `make check`
	@printf '#!/bin/sh\nexec make check\n' > .git/hooks/pre-push
	@chmod +x .git/hooks/pre-push
	@echo "pre-push hook installed (skip once with git push --no-verify)"

dev: ## Open this repo's devcontainer with the ldg built from this tree
	cargo run --quiet -- up .

install: build ## Install to $(PREFIX) (override: make install PREFIX=~/.local/bin)
	install -Dm755 target/release/ldg $(DESTDIR)$(BINDIR)/ldg

uninstall: ## Remove the installed binary
	rm -f $(DESTDIR)$(BINDIR)/ldg

clean: ## Remove build output
	cargo clean

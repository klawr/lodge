PREFIX ?= /usr/local/bin
BINDIR  = $(PREFIX)

.PHONY: all build install uninstall clean dev test lint fmt check

all: build

build:
	cargo build --release

test:
	cargo test

lint:
	cargo clippy --all-targets -- -D warnings

fmt:
	cargo fmt

# What CI runs.
check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test

install: build
	install -Dm755 target/release/ldg $(DESTDIR)$(BINDIR)/ldg

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/ldg

clean:
	cargo clean

dev:
	@./dev.sh

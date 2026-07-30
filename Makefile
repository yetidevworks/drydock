PREFIX ?= $(HOME)/.local

.PHONY: install
install:
	cargo install --force --path crates/drydock --root $(PREFIX)

.PHONY: check
check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test

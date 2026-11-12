# Quality gate for iyuu-reseed.
#
# `make check` is the single command that must pass before a commit: it runs
# formatting, clippy (pedantic is on via `[workspace.lints]`), and the tests.
# Individual targets exist so CI or a pre-commit hook can call them separately.

CARGO ?= cargo
CLIPPY_ARGS := --all-targets --all-features --locked -- -D warnings

.PHONY: help check fmt clippy test build web-check clean

help:
	@echo "make check   # fmt + clippy + test (the gate)"
	@echo "make fmt     # format the workspace"
	@echo "make clippy  # clippy with warnings as errors"
	@echo "make test    # cargo test"
	@echo "make build   # release build"
	@echo "make web-check  # type-check + build the Vue frontend"

check: fmt-check clippy test
	@echo "✅ all checks passed"

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

clippy:
	$(CARGO) clippy $(CLIPPY_ARGS)

test:
	$(CARGO) test --all-features --locked

build:
	$(CARGO) build --release --locked

web-check:
	cd web && bun run build

clean:
	$(CARGO) clean

# bevy_tono — canonical Makefile.
# No shell scripts a human runs: every recipe calls real tools only. `make` is
# the sole entrypoint; the git hooks call `make pre-commit-checks` / `make test`.

.DEFAULT_GOAL := help
.PHONY: help hooks run test pre-commit-checks clean

help: ## Show this help.
	@awk 'BEGIN{FS=":.*##"; printf "Usage: make \033[36m<target>\033[0m\n\nTargets:\n"} \
	      /^[a-zA-Z0-9_-]+:.*?##/{ printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2 }' \
	    $(MAKEFILE_LIST)

hooks: ## Install the git hooks (pre-commit + pre-push).
	git config core.hooksPath .githooks
	@echo "✓ hooks installed (.githooks)"

run: ## Run the example: press SPACE to blip; the music breathes.
	cargo run --example play

test: ## Run the test suite.
	cargo test

pre-commit-checks: ## Format-check + clippy gate. Exactly what the git hooks run.
	cargo fmt --all -- --check
	cargo clippy --all-targets --all-features -- -D warnings

clean: ## Wipe build artifacts.
	cargo clean

# bevy_tono — canonical Makefile.
# No shell scripts a human runs: every recipe calls real tools only. `make` is
# the sole entrypoint; the git hooks call `make pre-commit-checks` / `make test`.

RELEASE_BRANCH ?= master

.DEFAULT_GOAL := help
.PHONY: help hooks run test pre-commit-checks branding release clean

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

branding: ## No marketing surface for a library. Documented no-op (target exists per standard).
	@echo "bevy_tono is a library — no branding surface."

release: ## Cut + push a vX.Y.Z tag from clean master. CI publishes to crates.io.
	@[ "$$(git branch --show-current)" = "$(RELEASE_BRANCH)" ] \
	    || { echo "Release only from $(RELEASE_BRANCH)." >&2; exit 1; }
	@git diff --quiet && git diff --cached --quiet \
	    || { echo "Working tree dirty — commit before releasing." >&2; exit 1; }
	@V=$$(grep -m1 '^version = ' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/'); \
	 echo "→ Releasing v$$V"; \
	 if git rev-parse "v$$V" >/dev/null 2>&1; then \
	   echo "  tag v$$V exists — bump version in Cargo.toml first." >&2; exit 1; fi; \
	 git tag -a "v$$V" -m "v$$V" && git push origin "v$$V"
	@echo "✓ Tagged. The release workflow publishes to crates.io — watch GitHub Actions."

clean: ## Wipe build artifacts.
	cargo clean

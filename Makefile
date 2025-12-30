.PHONY: help

# Should remain at the top, otherwise `make` won't print help
help: ## Display this help message
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## / {printf "\033[36m%-30s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

lint:  ## cargo fmt, check and clippy.
	## fmt first, because it's the cheapest
	cargo fmt --all --check
	cargo check --workspace --all-targets --all-features
	cargo clippy --workspace --all-targets --all-features

test:  ## Runs test suite using next test
	@cargo nextest run --no-fail-fast --status-level skip --all-features

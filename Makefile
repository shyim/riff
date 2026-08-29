PHP_BIN ?= $(shell command -v php 2>/dev/null)
COMPOSER_SRC_DIR ?= $(CURDIR)/shopware/composer
RIFF_BIN ?= $(CURDIR)/target/debug/riff

.PHONY: build release benchmark benchmark-render fmt clippy test test-core test-cli test-composer test-composer-case composer-test-check composer-test-inventories composer-test-inventory composer-test-pending composer-php-test-inventory composer-php-test-pending composer-php-test-delegated composer-php-test-case composer-php-test-group composer-functional-test-inventory composer-functional-test-pending composer-functional-test-case homebrew-formula-check check composer-reference parity

build:
	@cargo build --workspace

release:
	@cargo build --release --workspace --locked

benchmark: release
	@./scripts/benchmark-symfony-demo.sh

benchmark-render:
	@./scripts/render-symfony-demo-benchmark.sh

homebrew-formula-check:
	@./packaging/homebrew/test-render-formula.sh

fmt:
	@cargo fmt --all --check

clippy:
	@cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

test:
	@cargo test --workspace --all-targets --locked

test-core:
	@cargo test-core

test-cli:
	@cargo test-cli

test-composer:
	@cargo test-composer

test-composer-case:
	@test -n "$(CASE)" || { echo "Usage: make test-composer-case CASE=update_all" >&2; exit 1; }
	@if test "$(PENDING)" = "1"; then \
		cargo test-composer "$(CASE)" -- --ignored --nocapture; \
	else \
		cargo test-composer "$(CASE)"; \
	fi

composer-test-inventories: composer-test-inventory composer-php-test-inventory composer-functional-test-inventory

composer-test-check: composer-test-inventories
	@status=0; \
	for target in composer-test-pending composer-php-test-pending composer-functional-test-pending; do \
		pending="$$( $(MAKE) --no-print-directory "$$target")"; \
		if test -n "$$pending"; then \
			printf '%s reported pending Composer contracts:\n%s\n' "$$target" "$$pending" >&2; \
			status=1; \
		fi; \
	done; \
	exit "$$status"

composer-test-inventory:
	@./scripts/composer-test-inventory.sh

composer-test-pending:
	@./scripts/composer-test-inventory.sh --pending

composer-php-test-inventory:
	@./scripts/composer-php-test-inventory.sh

composer-php-test-pending:
	@./scripts/composer-php-test-inventory.sh --pending

composer-php-test-delegated:
	@./scripts/composer-php-test-inventory.sh --delegated

composer-php-test-case:
	@test -n "$(CASE)" || { echo "Usage: make composer-php-test-case CASE=TransactionTest" >&2; exit 1; }
	@./scripts/composer-php-test-inventory.sh --run "$(CASE)"

composer-php-test-group:
	@test -n "$(GROUP)" || { echo "Usage: make composer-php-test-group GROUP=InstalledVersionsTest.php" >&2; exit 1; }
	@./scripts/composer-php-test-inventory.sh --run-group "$(GROUP)"

composer-functional-test-inventory:
	@./scripts/composer-functional-test-inventory.sh

composer-functional-test-pending:
	@./scripts/composer-functional-test-inventory.sh --pending

composer-functional-test-case:
	@test -n "$(CASE)" || { echo "Usage: make composer-functional-test-case CASE=create-project-command.test" >&2; exit 1; }
	@./scripts/composer-functional-test-inventory.sh --run "$(CASE)"

check: fmt clippy test release homebrew-formula-check

composer-reference: build
	@test -f "$(COMPOSER_SRC_DIR)/bin/composer" || { echo "Missing Composer checkout at $(COMPOSER_SRC_DIR)" >&2; exit 1; }
	@if test ! -f "$(COMPOSER_SRC_DIR)/vendor/autoload.php"; then \
		RIFF_PHP="$(PHP_BIN)" "$(RIFF_BIN)" install --no-scripts --no-plugins --no-audit --no-interaction -d "$(COMPOSER_SRC_DIR)"; \
	fi

parity: composer-reference
	@for suite in core mutations operations validate config status platform; do \
		COMPOSER_SRC_DIR="$(COMPOSER_SRC_DIR)" PHP_BIN="$(PHP_BIN)" RIFF_BIN="$(RIFF_BIN)" \
			"./scripts/composer-$$suite-differential.sh" || exit; \
	done

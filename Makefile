BIN := target/release/phylactery-mcp
TEST_DSN ?= postgresql://localhost:5432/phylactery_test

.PHONY: fmt lint test test-live smoke release check

fmt:
	cargo fmt --check

lint:
	cargo clippy --all-targets -- -D warnings

# Hermetic: unit + smoke + contract. Database tests skip themselves without a DSN.
test:
	cargo test

# Full pyramid against a disposable database.
test-live:
	PHYLACTERY_TEST_DSN="$(TEST_DSN)" cargo test

# End-to-end through the real binary: handshake, doctor, write, read, forget.
smoke:
	cargo build --release
	PHYLACTERY_TEST_DSN="$(TEST_DSN)" cargo test --test live_smoke -- --nocapture

release:
	cargo build --release
	@ls -lh $(BIN)

check: fmt lint test

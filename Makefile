.PHONY: lint test test-linux tui quality-fast quality-resources quality-compaction

lint:
	cargo fmt --all
	cargo clippy --fix --allow-dirty --all-targets --all-features -- --deny warnings

# Run the pinned, deterministic suite. Plain `cargo` resolves the pin in
# rust-toolchain.toml through the rustup shim; do not hardcode the nightly
# channel here (see scripts/check-toolchain-pin.sh).
# The PTY tests use a loopback streaming fixture or no model request at all;
# neither reaches a real provider. The focused terminal behavior tests cover
# the host presentation contract.
test:
	cargo test --workspace --locked
	cargo test -p tea-agent --features pty-harness --test pty_streaming --locked

# Build and run the deterministic suite inside Linux AArch64. Docker's
# platform selection also makes this usable from an x86_64 or Apple host.
DOCKER ?= docker
TEST_LINUX_IMAGE ?= tea-test-linux-aarch64
TEA_RELEASE_GIT_SHA ?= $(shell git rev-parse --short=7 HEAD 2>/dev/null)

test-linux:
	$(DOCKER) build --platform linux/arm64 --progress=plain --build-arg TEA_RELEASE_GIT_SHA="$(TEA_RELEASE_GIT_SHA)" --tag $(TEST_LINUX_IMAGE) -f Dockerfile .

tui:
	cargo build --release --package tea-agent --bin tea

quality-fast:
	PYTHONDONTWRITEBYTECODE=1 python3 -m evals.quality fast

quality-resources:
	PYTHONDONTWRITEBYTECODE=1 python3 -m evals.quality resources

quality-compaction:
	PYTHONDONTWRITEBYTECODE=1 python3 -m evals.quality compaction --out /tmp/tea-compaction-quality

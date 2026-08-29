.PHONY: lint test test-linux tui local-install local-model local quality-fast quality-resources quality-compaction pi-shootout pi-shootout-plan pi-shootout-check

lint:
	cargo fmt --all
	cargo clippy --fix --allow-dirty --all-targets --all-features -- --deny warnings

# Run the pinned, deterministic suite. The PTY tests use a loopback streaming
# fixture or no model request at all; neither reaches a real provider. The
# focused terminal behavior tests cover the host presentation contract.
test:
	rustup run nightly-2026-07-24 cargo test --workspace --locked
	rustup run nightly-2026-07-24 cargo test -p tea-agent --features pty-harness --test pty_streaming --locked

# Build and run the deterministic suite inside Linux AArch64. Docker's
# platform selection also makes this usable from an x86_64 or Apple host.
DOCKER ?= docker
TEST_LINUX_IMAGE ?= tea-test-linux-aarch64

test-linux:
	$(DOCKER) build --platform linux/arm64 --progress=plain --tag $(TEST_LINUX_IMAGE) -f Dockerfile .

tui:
	cargo build --release --package tea-agent --bin tea

OMLX_ROOT ?= $(HOME)/d/omlx
OMLX_VENV ?= $(OMLX_ROOT)/.venv
OMLX_PYTHON_VERSION ?= 3.13
OMLX_PYTHON ?= $(OMLX_VENV)/bin/python
OMLX_BIN ?= $(OMLX_VENV)/bin/omlx
OMLX_HF ?= $(OMLX_VENV)/bin/hf
LOCAL_PORT ?= 12345
LOCAL_BASE_URL ?= http://127.0.0.1:$(LOCAL_PORT)/v1
LOCAL_MODEL ?= Qwen3.5-4B-MLX-4bit
# Effective oMLX prompt capacity used by the TUI's automatic-compaction policy.
LOCAL_CONTEXT_WINDOW ?= 32768
LOCAL_MODEL_REPO ?= mlx-community/Qwen3.5-4B-MLX-4bit
LOCAL_MODEL_DIR ?= $(HOME)/.omlx/models/$(LOCAL_MODEL)
LOCAL_OMLX_BASE_PATH ?= /tmp/tea-omlx-$(LOCAL_PORT)
LOCAL_OMLX_LOG ?= $(LOCAL_OMLX_BASE_PATH)/server.log
LOCAL_PI_ARGS ?=

# Keep the source checkout runnable without a separate manual Python setup. Both
# commands are safe to repeat: uv preserves an existing environment and only
# updates the editable install or dependencies that changed.
local-install:
	@command -v uv >/dev/null 2>&1 || { echo "missing required command: uv" >&2; exit 1; }
	@test -d "$(OMLX_ROOT)" || { echo "missing oMLX checkout: $(OMLX_ROOT)" >&2; exit 1; }
	@uv venv --allow-existing --python "$(OMLX_PYTHON_VERSION)" "$(OMLX_VENV)"
	@uv pip install --python "$(OMLX_PYTHON)" --editable "$(OMLX_ROOT)"

# Newer huggingface_hub releases provide `hf` as the supported oMLX virtual-environment CLI.
local-model: local-install
	@test -x "$(OMLX_HF)" || { echo "missing oMLX Hugging Face CLI: $(OMLX_HF)" >&2; exit 1; }
	@mkdir -p "$(dir $(LOCAL_MODEL_DIR))"
	@echo "Ensuring $(LOCAL_MODEL_REPO) is present at $(LOCAL_MODEL_DIR)"
	@"$(OMLX_HF)" download --local-dir "$(LOCAL_MODEL_DIR)" "$(LOCAL_MODEL_REPO)"

local-server: local-model
	@test -x "$(OMLX_BIN)" || { echo "missing oMLX executable: $(OMLX_BIN)" >&2; exit 1; }
	@if curl -fsS --max-time 1 "$(LOCAL_BASE_URL)/models" 2>/dev/null | grep -Fq '"id":"$(LOCAL_MODEL)"'; then \
		echo "oMLX already serving $(LOCAL_MODEL) at $(LOCAL_BASE_URL)"; \
	else \
		if curl -sS --max-time 1 "$(LOCAL_BASE_URL)/models" >/dev/null 2>&1; then \
			echo "$(LOCAL_BASE_URL) is already occupied by a different service" >&2; \
			exit 1; \
		fi; \
		mkdir -p "$(LOCAL_OMLX_BASE_PATH)"; \
		echo "Starting oMLX on $(LOCAL_BASE_URL)"; \
		nohup "$(OMLX_BIN)" serve --base-path "$(LOCAL_OMLX_BASE_PATH)" --model-dir "$(HOME)/.omlx/models" --no-hf-cache --host 127.0.0.1 --port "$(LOCAL_PORT)" >"$(LOCAL_OMLX_LOG)" 2>&1 </dev/null & \
		ready=0; \
		for attempt in $$(seq 1 60); do \
			if curl -fsS --max-time 1 "$(LOCAL_BASE_URL)/models" 2>/dev/null | grep -Fq '"id":"$(LOCAL_MODEL)"'; then ready=1; break; fi; \
			sleep 1; \
		done; \
		if [ "$$ready" -ne 1 ]; then \
			echo "oMLX did not become ready; see $(LOCAL_OMLX_LOG)" >&2; \
			tail -n 80 "$(LOCAL_OMLX_LOG)" >&2 2>/dev/null || true; \
			exit 1; \
		fi; \
		echo "oMLX ready at $(LOCAL_BASE_URL)"; \
	fi

local: local-server
	cargo build --package tea-agent --bin tea
	"$(CURDIR)/target/debug/tea" --provider local --local-base-url "$(LOCAL_BASE_URL)" --model "$(LOCAL_MODEL)" --local-context-window "$(LOCAL_CONTEXT_WINDOW)" --thinking low $(LOCAL_PI_ARGS)

quality-fast:
	PYTHONDONTWRITEBYTECODE=1 python3 -m evals.quality fast

quality-resources:
	PYTHONDONTWRITEBYTECODE=1 python3 -m evals.quality resources

quality-compaction:
	PYTHONDONTWRITEBYTECODE=1 python3 -m evals.quality compaction --out /tmp/tea-compaction-quality

# This is an explicit, provider-opt-in one-task experiment. The model is fixed
# for v0 so a typo cannot silently compare a different model.
PI_SHOOTOUT_TASK ?= express-3936-medium
PI_SHOOTOUT_PROVIDER ?= openrouter
PI_SHOOTOUT_MODEL ?= deepseek/deepseek-v4-flash-0731
PI_SHOOTOUT_THINKING ?= high
PI_SHOOTOUT_MAX_OUTPUT_TOKENS ?= unlimited
PI_SHOOTOUT_TIMEOUT_SECONDS ?= 900
PI_SHOOTOUT_REPEATS ?= 1
PI_SHOOTOUT_SEED ?= 20260823
PI_SHOOTOUT_CACHE_ROOT ?= /tmp/tea-pi-shootout-cache
PI_SHOOTOUT_WORKSPACE_ROOT ?= /tmp/tea-pi-shootout-workspaces
PI_SHOOTOUT_OUT ?= /tmp/tea-pi-shootout

PI_SHOOTOUT_ARGS = --task "$(PI_SHOOTOUT_TASK)" --provider "$(PI_SHOOTOUT_PROVIDER)" --model "$(PI_SHOOTOUT_MODEL)" --thinking "$(PI_SHOOTOUT_THINKING)" --max-output-tokens "$(PI_SHOOTOUT_MAX_OUTPUT_TOKENS)" --timeout-seconds "$(PI_SHOOTOUT_TIMEOUT_SECONDS)" --repeats "$(PI_SHOOTOUT_REPEATS)" --seed "$(PI_SHOOTOUT_SEED)" --cache-root "$(PI_SHOOTOUT_CACHE_ROOT)" --workspace-root "$(PI_SHOOTOUT_WORKSPACE_ROOT)" --out "$(PI_SHOOTOUT_OUT)"

pi-shootout-plan:
	@command -v node >/dev/null 2>&1 || { echo "missing required command: node" >&2; exit 1; }
	@command -v npm >/dev/null 2>&1 || { echo "missing required command: npm" >&2; exit 1; }
	@command -v curl >/dev/null 2>&1 || { echo "missing required command: curl" >&2; exit 1; }
	@command -v git >/dev/null 2>&1 || { echo "missing required command: git" >&2; exit 1; }
	@node -e 'const [major, minor] = process.versions.node.split(".").map(Number); if (major < 22 || (major === 22 && minor < 19)) process.exit(1)' || { echo "node >=22.19.0 is required" >&2; exit 1; }
	PYTHONDONTWRITEBYTECODE=1 python3 -m evals.pi_shootout plan $(PI_SHOOTOUT_ARGS)

pi-shootout-check:
	PYTHONDONTWRITEBYTECODE=1 python3 -m unittest evals.pi_shootout.test_contract evals.pi_shootout.test_report
	npm --prefix evals/pi_shootout/sdk ci
	npm --prefix evals/pi_shootout/sdk run check
	npm --prefix evals/pi_shootout/sdk test
	cargo +nightly-2026-07-24 test -p tea-providers --bin tea-eval --features eval-runner --locked
	cargo +nightly-2026-07-24 test -p tea-session --locked jsonl_reopen_fixed_point_covers_compaction_harness_activation_and_core_rollover
	PYTHONDONTWRITEBYTECODE=1 python3 -m unittest evals.quality.test_coding_cases

pi-shootout: pi-shootout-plan
	@command -v vault >/dev/null 2>&1 || { echo "missing required command: vault (expected: vault OPENROUTER_API_KEY -- <adapter>)" >&2; exit 1; }
	npm --prefix evals/pi_shootout/sdk ci
	cargo +nightly-2026-07-24 build -p tea-providers --bin tea-eval --features eval-runner --locked
	PYTHONDONTWRITEBYTECODE=1 python3 -m evals.pi_shootout run $(PI_SHOOTOUT_ARGS)

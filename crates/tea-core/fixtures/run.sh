#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../../.." && pwd)
fixture_dir="$script_dir/declarative"
expected_dir="$script_dir/expected"

# rust-toolchain.toml is the single source of truth; plain `cargo` under the
# repository root resolves the pinned nightly through the rustup shim.
command -v jq >/dev/null 2>&1 || {
  echo "Rust fixture check: jq is required" >&2
  exit 2
}

(cd "$repo_root" && cargo build -q -p tea-core --features fixture-runner --bin tea-fixtures)
rust_runner="$repo_root/target/debug/tea-fixtures"

temp_dir=$(mktemp -d "${TMPDIR:-/tmp}/pi-rust-fixtures.XXXXXX")
trap 'rm -rf "$temp_dir"' EXIT

passed=0
failed=0
while IFS= read -r fixture; do
  relative_fixture=${fixture#"$fixture_dir"/}
  name=${relative_fixture%.json}
  expected="$expected_dir/$relative_fixture"
  output="$temp_dir/$name.result.json"
  expected_canonical="$temp_dir/$name.expected.json"
  actual_canonical="$temp_dir/$name.actual.json"
  mkdir -p "$(dirname "$output")"

  set +e
  "$rust_runner" "$fixture" >"$output"
  status=$?
  set -e
  if [[ "$status" != 0 && "$status" != 1 ]]; then
    echo "FAIL $name: runner exited with status $status" >&2
    failed=$((failed + 1))
    continue
  fi
  jq -e -cS . "$expected" >"$expected_canonical"
  jq -e -cS . "$output" >"$actual_canonical"
  if diff -u "$expected_canonical" "$actual_canonical" >"$temp_dir/$name.diff"; then
    echo "ok $name"
    passed=$((passed + 1))
  else
    echo "FAIL $name" >&2
    sed -n '1,160p' "$temp_dir/$name.diff" >&2
    failed=$((failed + 1))
  fi
done < <(find "$fixture_dir" -type f -name '*.json' -print | sort)

echo "Rust fixture check: $passed passed, $failed failed"
[[ "$failed" == 0 ]]

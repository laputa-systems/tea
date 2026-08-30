import assert from "node:assert/strict";
import test from "node:test";

import { canonical } from "../src/canonical.ts";

test("canonical object ordering matches the byte ordering used by Python and Rust", () => {
	assert.equal(
		canonical({ npm_config_cache: "cache", NPM_CONFIG_FUND: "false", NPM_CONFIG_AUDIT: "false" }),
		'{"NPM_CONFIG_AUDIT":"false","NPM_CONFIG_FUND":"false","npm_config_cache":"cache"}',
	);
});

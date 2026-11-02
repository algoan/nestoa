const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawnSync } = require("node:child_process");
const { test } = require("node:test");

const SHIM = path.join(__dirname, "..", "nestoa", "bin", "nestoa.js");
const posixOnly = { skip: process.platform === "win32" };

function runShim(script) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "nestoa-shim-"));
  const binary = path.join(dir, "nestoa");
  fs.writeFileSync(binary, script, { mode: 0o755 });
  return spawnSync(process.execPath, [SHIM, "--flag"], {
    env: { ...process.env, NESTOA_BINARY: binary },
    encoding: "utf8",
  });
}

test("passes the arguments and the exit code through", posixOnly, () => {
  const result = runShim('#!/bin/sh\necho "args: $*"\nexit 3\n');
  assert.equal(result.stdout, "args: --flag\n");
  assert.equal(result.status, 3);
});

test("dies of the signal that killed the binary", posixOnly, () => {
  const result = runShim("#!/bin/sh\nkill -TERM $$\n");
  assert.equal(result.signal, "SIGTERM");
});

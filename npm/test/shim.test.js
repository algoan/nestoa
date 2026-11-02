const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawnSync } = require("node:child_process");
const { test } = require("node:test");

const SHIM = path.join(__dirname, "..", "nestoa", "bin", "nestoa.js");
const posixOnly = { skip: process.platform === "win32" };

function runShim(script, { platform } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "nestoa-shim-"));
  const env = { ...process.env };
  delete env.NESTOA_BINARY;
  if (script != null) {
    env.NESTOA_BINARY = path.join(dir, "nestoa");
    fs.writeFileSync(env.NESTOA_BINARY, script, { mode: 0o755 });
  }
  const args = [SHIM, "--flag"];
  if (platform) {
    const preload = path.join(dir, "platform.js");
    fs.writeFileSync(
      preload,
      `Object.defineProperty(process, "platform", { value: ${JSON.stringify(platform)} });`,
    );
    args.unshift("--require", preload);
  }
  return spawnSync(process.execPath, args, { env, encoding: "utf8" });
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

test("says when the platform has no prebuilt binary", () => {
  const result = runShim(null, { platform: "aix" });
  assert.equal(result.status, 1);
  assert.match(result.stderr, new RegExp(`no prebuilt binary for aix-${process.arch}`));
});

test("runs NESTOA_BINARY on a platform without a prebuilt binary", posixOnly, () => {
  const result = runShim('#!/bin/sh\necho "args: $*"\n', { platform: "aix" });
  assert.equal(result.stdout, "args: --flag\n");
  assert.equal(result.status, 0);
});

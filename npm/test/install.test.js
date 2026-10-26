const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { execFileSync } = require("node:child_process");
const { test } = require("node:test");

const { canReplaceShim, install } = require("../nestoa/install.js");

const SHIM = fs.readFileSync(
  path.join(__dirname, "..", "nestoa", "bin", "nestoa.js"),
  "utf8",
);
const posixOnly = { skip: process.platform === "win32" };
const npmEnv = { npm_config_user_agent: "npm/11.0.0 node/v24.0.0 darwin arm64" };

// A package laid out as npm installs it: the platform package's binary and the
// main package's `bin/` holding the shim.
function fixture(binary) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "nestoa-install-"));
  const source = path.join(dir, "platform", "nestoa");
  fs.mkdirSync(path.dirname(source));
  fs.writeFileSync(source, binary, { mode: 0o755 });
  const binDir = path.join(dir, "main", "bin");
  fs.mkdirSync(binDir, { recursive: true });
  fs.writeFileSync(path.join(binDir, "nestoa.js"), SHIM, { mode: 0o755 });
  return { source, binDir, shim: path.join(binDir, "nestoa.js") };
}

const WORKING = '#!/bin/sh\necho "nestoa 1.2.3"\n';

test("replaces the shim with a binary that runs", posixOnly, () => {
  const { source, binDir, shim } = fixture(WORKING);
  assert.equal(install(source, binDir, { platform: "linux", env: npmEnv }), "native");
  assert.equal(fs.readFileSync(shim, "utf8"), WORKING);
  assert.equal(execFileSync(shim, ["--version"], { encoding: "utf8" }), "nestoa 1.2.3\n");
  assert.deepEqual(fs.readdirSync(binDir), ["nestoa.js"]);
});

test("leaves nothing behind when the shim is already the binary", posixOnly, () => {
  const { source, binDir } = fixture(WORKING);
  install(source, binDir, { platform: "linux", env: npmEnv });
  assert.equal(install(source, binDir, { platform: "linux", env: npmEnv }), "native");
  assert.deepEqual(fs.readdirSync(binDir), ["nestoa.js"]);
});

test("keeps the shim when the binary does not run", posixOnly, () => {
  const { source, binDir, shim } = fixture("#!/bin/sh\nexit 1\n");
  assert.equal(install(source, binDir, { platform: "linux", env: npmEnv }), "shim");
  assert.equal(fs.readFileSync(shim, "utf8"), SHIM);
  assert.deepEqual(fs.readdirSync(binDir).sort(), ["nestoa", "nestoa.js"]);
});

test("keeps the shim when the binary is not nestoa", posixOnly, () => {
  const { source, binDir, shim } = fixture('#!/bin/sh\necho "other 1.0.0"\n');
  assert.equal(install(source, binDir, { platform: "linux", env: npmEnv }), "shim");
  assert.equal(fs.readFileSync(shim, "utf8"), SHIM);
});

test("does not write through a shim hard-linked into a store", posixOnly, () => {
  const { source, binDir, shim } = fixture(WORKING);
  const store = path.join(path.dirname(binDir), "store-copy.js");
  fs.linkSync(shim, store);
  assert.equal(install(source, binDir, { platform: "linux", env: npmEnv }), "native");
  assert.equal(fs.readFileSync(store, "utf8"), SHIM);
});

test("keeps the shim where `.bin` runs it with node", () => {
  const agent = (name) => ({ npm_config_user_agent: `${name} node/v24.0.0 linux x64` });
  assert.equal(canReplaceShim({ platform: "linux", env: npmEnv }), true);
  assert.equal(canReplaceShim({ platform: "win32", env: npmEnv }), false);
  assert.equal(canReplaceShim({ platform: "linux", env: agent("pnpm/10.0.0 npm/?") }), false);
  assert.equal(canReplaceShim({ platform: "linux", env: agent("yarn/4.5.0 npm/?") }), false);
  assert.equal(canReplaceShim({ platform: "linux", env: {} }), false);
  assert.equal(
    canReplaceShim({ platform: "linux", env: { ...npmEnv, NESTOA_BINARY: "/opt/nestoa" } }),
    false,
  );
});

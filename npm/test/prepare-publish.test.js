const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { test } = require("node:test");

const { prepare } = require("../prepare-publish.js");

const LAUNCHER = path.join(__dirname, "..", "nestoa", "package.json");

// The npm/ directory of a release job: the launcher and the platform packages
// the matrix built.
function fixture(tags) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "nestoa-publish-"));
  fs.mkdirSync(path.join(root, "nestoa"));
  fs.copyFileSync(LAUNCHER, path.join(root, "nestoa", "package.json"));
  for (const tag of tags) {
    const dir = path.join(root, "platforms", tag);
    fs.mkdirSync(dir, { recursive: true });
    const pkg = { name: `@algoan/nestoa-${tag}`, version: "0.1.0" };
    fs.writeFileSync(path.join(dir, "package.json"), JSON.stringify(pkg));
  }
  return root;
}

const read = (root, ...p) => JSON.parse(fs.readFileSync(path.join(root, ...p), "utf8"));

test("names the launcher for each registry", () => {
  const root = fixture(["darwin-arm64"]);
  prepare({ root, registry: "github", version: "1.2.3" });
  assert.equal(read(root, "nestoa", "package.json").name, "@algoan/nestoa");
  prepare({ root, registry: "npm", version: "1.2.3" });
  assert.equal(read(root, "nestoa", "package.json").name, "nestoa");
  assert.equal(
    read(root, "platforms", "darwin-arm64", "package.json").name,
    "@algoan/nestoa-darwin-arm64",
  );
});

test("depends on the built platform packages at the release version", () => {
  const root = fixture(["linux-x64-gnu", "win32-x64-msvc"]);
  prepare({ root, registry: "npm", version: "1.2.3-rc.1" });
  const launcher = read(root, "nestoa", "package.json");
  assert.equal(launcher.version, "1.2.3-rc.1");
  assert.deepEqual(launcher.optionalDependencies, {
    "@algoan/nestoa-linux-x64-gnu": "1.2.3-rc.1",
    "@algoan/nestoa-win32-x64-msvc": "1.2.3-rc.1",
  });
  assert.equal(read(root, "platforms", "win32-x64-msvc", "package.json").version, "1.2.3-rc.1");
  assert.equal(launcher.repository.url, "git+https://github.com/algoan/nestoa.git");
});

test("rejects an unknown registry", () => {
  assert.throws(() => prepare({ root: fixture([]), registry: "jsr", version: "1.0.0" }), /unknown registry/);
});

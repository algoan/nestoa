#!/usr/bin/env node
// Rewrites the npm packages for publication on the GitHub Packages registry.
//
// GitHub's npm registry (npm.pkg.github.com) only accepts packages scoped to
// the repository owner, so the locally-developed names (`nestoa` and
// `@nestoa/<tag>`) are remapped here at publish time:
//
//   nestoa            -> @algoan/nestoa
//   @nestoa/<tag>     -> @algoan/nestoa-<tag>
//
// The source tree keeps the `@nestoa/*` names; this transform only runs in CI
// against the build artifacts. It also:
//   - stamps every package with the release version (from argv or env),
//   - prunes the main package's optionalDependencies to the platform packages
//     actually present under ./platforms (the release matrix may build a
//     subset),
//   - adds the publishConfig/repository fields GitHub Packages expects.
//
// Usage:
//   node prepare-github-publish.js [version]
//   VERSION=1.2.3 node prepare-github-publish.js

const fs = require("fs");
const path = require("path");

const SCOPE = "@algoan";
const OLD_SCOPE = "@nestoa";
// `@nestoa/linux-x64-gnu` -> `@algoan/nestoa-linux-x64-gnu`
const NEW_PREFIX = `${SCOPE}/nestoa-`;
const REPO_URL = "git+https://github.com/algoan/nestoa.git";

const root = __dirname;
const mainDir = path.join(root, "nestoa");
const platformsDir = path.join(root, "platforms");

function scopedPlatformName(tag) {
  return `${NEW_PREFIX}${tag}`;
}

function readJson(p) {
  return JSON.parse(fs.readFileSync(p, "utf8"));
}

function writeJson(p, obj) {
  fs.writeFileSync(p, JSON.stringify(obj, null, 2) + "\n");
}

// Replace `@nestoa/<tag>` occurrences inside a JS source file (install.js / the
// runtime shim) with the published `@algoan/nestoa-<tag>` form.
function rewriteSource(p) {
  if (!fs.existsSync(p)) return;
  const src = fs.readFileSync(p, "utf8");
  // `@nestoa/${tag}` (template) and `@nestoa/<literal>` both start with the
  // scope + slash; remap the scope-and-slash to the flat prefix.
  const out = src.split(`${OLD_SCOPE}/`).join(NEW_PREFIX);
  fs.writeFileSync(p, out);
}

function discoverTags() {
  if (!fs.existsSync(platformsDir)) return [];
  return fs
    .readdirSync(platformsDir, { withFileTypes: true })
    .filter((e) => e.isDirectory())
    .map((e) => e.name)
    .sort();
}

function main() {
  const version = process.argv[2] || process.env.VERSION || null;
  const tags = discoverTags();

  // --- platform packages -------------------------------------------------
  for (const tag of tags) {
    const pkgPath = path.join(platformsDir, tag, "package.json");
    if (!fs.existsSync(pkgPath)) {
      console.warn(`skipping ${tag}: no package.json`);
      continue;
    }
    const pkg = readJson(pkgPath);
    pkg.name = scopedPlatformName(tag);
    if (version) pkg.version = version;
    pkg.repository = { type: "git", url: REPO_URL };
    pkg.publishConfig = { registry: "https://npm.pkg.github.com" };
    writeJson(pkgPath, pkg);
    console.log(`prepared ${pkg.name}@${pkg.version}`);
  }

  // --- main package ------------------------------------------------------
  const mainPkgPath = path.join(mainDir, "package.json");
  const mainPkg = readJson(mainPkgPath);
  mainPkg.name = `${SCOPE}/nestoa`;
  if (version) mainPkg.version = version;

  // Only keep optionalDependencies for the platform packages we actually
  // built/published, remapped to the @algoan scope and the release version.
  const optDeps = {};
  for (const tag of tags) {
    optDeps[scopedPlatformName(tag)] = version || mainPkg.version;
  }
  mainPkg.optionalDependencies = optDeps;
  mainPkg.repository = { type: "git", url: REPO_URL };
  mainPkg.publishConfig = { registry: "https://npm.pkg.github.com" };
  writeJson(mainPkgPath, mainPkg);
  console.log(
    `prepared ${mainPkg.name}@${mainPkg.version} ` +
      `(${Object.keys(optDeps).length} platform deps)`,
  );

  // --- runtime sources that reference the old scope ----------------------
  rewriteSource(path.join(mainDir, "install.js"));
  rewriteSource(path.join(mainDir, "bin", "nestoa.js"));
}

main();

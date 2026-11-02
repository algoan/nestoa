#!/usr/bin/env node
// Stamps the release version into the built npm packages and names the
// launcher for the registry it is published to:
//
//   github   @algoan/nestoa   (npm.pkg.github.com only accepts the owner's scope)
//   npm      nestoa           (registry.npmjs.org)
//
// The platform packages keep their `@algoan/nestoa-<tag>` name on both
// registries. The launcher's optionalDependencies are pruned to the platform
// packages present under ./platforms, as the release matrix may build a
// subset. Running it again for the other registry only renames the launcher.
//
// Usage:
//   node prepare-publish.js <github|npm> <version>

const fs = require("fs");
const path = require("path");

const LAUNCHER_NAMES = { github: "@algoan/nestoa", npm: "nestoa" };
const REPOSITORY = { type: "git", url: "git+https://github.com/algoan/nestoa.git" };

function readJson(p) {
  return JSON.parse(fs.readFileSync(p, "utf8"));
}

function writeJson(p, obj) {
  fs.writeFileSync(p, JSON.stringify(obj, null, 2) + "\n");
}

function platformDirs(root) {
  const platforms = path.join(root, "platforms");
  if (!fs.existsSync(platforms)) return [];
  return fs
    .readdirSync(platforms, { withFileTypes: true })
    .filter((e) => e.isDirectory())
    .map((e) => path.join(platforms, e.name))
    .filter((dir) => fs.existsSync(path.join(dir, "package.json")))
    .sort();
}

function prepare({ root, registry, version }) {
  const name = LAUNCHER_NAMES[registry];
  if (!name) throw new Error(`unknown registry: ${registry}`);
  if (!version) throw new Error("missing version");

  const optionalDependencies = {};
  for (const dir of platformDirs(root)) {
    const pkgPath = path.join(dir, "package.json");
    const pkg = readJson(pkgPath);
    writeJson(pkgPath, { ...pkg, version, repository: REPOSITORY });
    optionalDependencies[pkg.name] = version;
  }

  const launcherPath = path.join(root, "nestoa", "package.json");
  const launcher = readJson(launcherPath);
  writeJson(launcherPath, {
    ...launcher,
    name,
    version,
    optionalDependencies,
    repository: REPOSITORY,
  });
  return { name, platforms: Object.keys(optionalDependencies) };
}

module.exports = { prepare };

if (require.main === module) {
  const [, , registry, version] = process.argv;
  try {
    const { name, platforms } = prepare({ root: __dirname, registry, version });
    console.log(`prepared ${name}@${version} with ${platforms.length} platform packages`);
  } catch (err) {
    console.error(`${err.message}\nusage: prepare-publish.js <github|npm> <version>`);
    process.exit(2);
  }
}

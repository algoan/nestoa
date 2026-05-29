#!/usr/bin/env node
// Build helper invoked from CI: takes a target triple + a path to a freshly
// built `nestoa` binary and produces an `@nestoa/<tag>` npm package ready to
// publish. Run once per target the release matrix builds for.
//
// Usage:
//   node build-platform-packages.js <tag> <binary-path>
// e.g.
//   node build-platform-packages.js linux-x64-gnu target/x86_64-unknown-linux-gnu/release/nestoa

const fs = require("fs");
const path = require("path");

const TARGETS = {
  "linux-x64-gnu":   { os: "linux",  cpu: "x64",   libc: "glibc" },
  "linux-arm64-gnu": { os: "linux",  cpu: "arm64", libc: "glibc" },
  "linux-x64-musl":  { os: "linux",  cpu: "x64",   libc: "musl" },
  "linux-arm64-musl":{ os: "linux",  cpu: "arm64", libc: "musl" },
  "darwin-x64":      { os: "darwin", cpu: "x64" },
  "darwin-arm64":    { os: "darwin", cpu: "arm64" },
  "win32-x64-msvc":  { os: "win32",  cpu: "x64" },
};

function main() {
  const [, , tag, binarySource] = process.argv;
  if (!tag || !binarySource) {
    console.error("usage: build-platform-packages.js <tag> <binary-path>");
    process.exit(2);
  }
  const meta = TARGETS[tag];
  if (!meta) {
    console.error(`unknown target tag: ${tag}`);
    process.exit(2);
  }
  if (!fs.existsSync(binarySource)) {
    console.error(`binary not found at ${binarySource}`);
    process.exit(2);
  }
  const outDir = path.join(__dirname, "platforms", tag);
  fs.mkdirSync(outDir, { recursive: true });
  const version = require("./nestoa/package.json").version;

  const pkg = {
    name: `@nestoa/${tag}`,
    version,
    description: `Prebuilt nestoa binary for ${tag}`,
    license: "MIT",
    os: [meta.os],
    cpu: [meta.cpu],
    ...(meta.libc ? { libc: [meta.libc] } : {}),
    files: [meta.os === "win32" ? "nestoa.exe" : "nestoa", "README.md"],
  };

  fs.writeFileSync(
    path.join(outDir, "package.json"),
    JSON.stringify(pkg, null, 2) + "\n",
  );
  const dest = path.join(
    outDir,
    meta.os === "win32" ? "nestoa.exe" : "nestoa",
  );
  fs.copyFileSync(binarySource, dest);
  fs.chmodSync(dest, 0o755);
  fs.writeFileSync(
    path.join(outDir, "README.md"),
    `# @nestoa/${tag}\n\nPlatform-specific binary for nestoa. Installed automatically by the \`nestoa\` package on matching systems.\n`,
  );
  console.log(`built ${pkg.name}@${version} -> ${outDir}`);
}

main();

#!/usr/bin/env node
// Stamps the release version into the Rust workspace so the compiled binary
// reports it via `nestoa --version` (clap reads CARGO_PKG_VERSION at build time).
//
// The workspace `version` in Cargo.toml is the source of truth at compile time;
// it is intentionally left at a placeholder (0.1.0) in the repo and stamped from
// the release tag / workflow_dispatch input in CI, before `cargo build`.
// Cargo.lock is updated in lockstep so `cargo build --locked` still succeeds.
//
// Usage:
//   node stamp-cargo-version.js <version>       # e.g. 0.1.3
//   VERSION=0.1.3 node stamp-cargo-version.js

const fs = require("fs");
const path = require("path");

const repoRoot = path.join(__dirname, "..");
const cargoTomlPath = path.join(repoRoot, "Cargo.toml");
const cargoLockPath = path.join(repoRoot, "Cargo.lock");

function main() {
  const version = process.argv[2] || process.env.VERSION;
  if (!version) {
    console.error("usage: stamp-cargo-version.js <version>");
    process.exit(2);
  }
  if (!/^\d+\.\d+\.\d+(?:[-+].+)?$/.test(version)) {
    console.error(`invalid semver version: ${version}`);
    process.exit(2);
  }

  // --- Cargo.toml: the version under [workspace.package] ------------------
  let toml = fs.readFileSync(cargoTomlPath, "utf8");
  const section = "[workspace.package]";
  const idx = toml.indexOf(section);
  if (idx === -1) {
    console.error(`could not find ${section} in Cargo.toml`);
    process.exit(1);
  }
  // Replace the first `version = "..."` line after [workspace.package].
  const before = toml.slice(0, idx);
  let after = toml.slice(idx);
  const replaced = after.replace(
    /^version = "[^"]*"/m,
    `version = "${version}"`,
  );
  if (replaced === after) {
    console.error("could not find version line under [workspace.package]");
    process.exit(1);
  }
  toml = before + replaced;
  fs.writeFileSync(cargoTomlPath, toml);

  // --- Cargo.lock: the nestoa package entry ------------------------------
  if (fs.existsSync(cargoLockPath)) {
    let lock = fs.readFileSync(cargoLockPath, "utf8");
    const lockReplaced = lock.replace(
      /(name = "nestoa"\nversion = ")[^"]*(")/,
      `$1${version}$2`,
    );
    if (lockReplaced === lock) {
      console.error("could not find nestoa entry in Cargo.lock");
      process.exit(1);
    }
    fs.writeFileSync(cargoLockPath, lockReplaced);
  }

  console.log(`stamped Cargo workspace version -> ${version}`);
}

main();

#!/usr/bin/env node
// Thin shim that dispatches to the platform-specific prebuilt binary.
//
// At install time, npm installs exactly one of the `@algoan/nestoa-<platform>`
// optional dependencies based on os/arch/libc, and `install.js` replaces this
// file with its binary when npm installed it outside Windows. This shim only
// runs where that was not possible: on Windows, under another package manager,
// with NESTOA_BINARY set at install time, or when install scripts were skipped.
// Then it execs the binary copied into ./bin/nestoa(.exe), or the optional
// dependency's own one, with the user's argv. NESTOA_BINARY, when set, wins
// over both.

const { spawnSync } = require("child_process");
const path = require("path");
const fs = require("fs");
const os = require("os");

const BINARY = process.platform === "win32" ? "nestoa.exe" : "nestoa";

function resolveBinary(tag) {
  if (process.env.NESTOA_BINARY) return process.env.NESTOA_BINARY;
  const local = path.join(__dirname, BINARY);
  if (fs.existsSync(local)) return local;
  // Fall back to walking optional-dependency packages in case install.js was
  // skipped (e.g. when run via `--ignore-scripts`).
  if (!tag) return null;
  try {
    return require.resolve(`@algoan/nestoa-${tag}/${BINARY}`);
  } catch {
    return null;
  }
}

const tag = require("../install.js").platformTag();
const bin = resolveBinary(tag);
if (!bin) {
  console.error(
    tag
      ? "nestoa: could not locate the platform-specific binary. " +
          "Was the post-install step skipped? Try reinstalling, or set " +
          "NESTOA_BINARY to the absolute path of a built `nestoa` executable."
      : `nestoa: no prebuilt binary for ${process.platform}-${process.arch}. ` +
          "Set NESTOA_BINARY to the absolute path of a built `nestoa` executable.",
  );
  process.exit(1);
}

const result = spawnSync(bin, process.argv.slice(2), {
  stdio: "inherit",
});
if (result.error) {
  console.error("nestoa:", result.error.message);
  process.exit(1);
}
if (result.signal) {
  // Die of the same signal so the caller sees e.g. 143 for SIGTERM. Node
  // ignores a few signals (SIGPIPE), hence the exit code fallback.
  process.kill(process.pid, result.signal);
  process.exit(128 + (os.constants.signals[result.signal] || 0));
}
process.exit(result.status == null ? 1 : result.status);

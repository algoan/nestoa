#!/usr/bin/env node
// Thin shim that dispatches to the platform-specific prebuilt binary.
//
// At install time, npm installs exactly one of the `@nestoa/<platform>` optional
// dependencies based on os/arch/libc, and `install.js` copies (or symlinks) the
// binary into ./bin/nestoa(.exe). This shim execs that binary with the user's
// argv.

const { spawnSync } = require("child_process");
const path = require("path");
const fs = require("fs");

function resolveBinary() {
  const local = path.join(
    __dirname,
    process.platform === "win32" ? "nestoa.exe" : "nestoa",
  );
  if (fs.existsSync(local)) return local;
  // Fall back to walking optional-dependency packages in case install.js was
  // skipped (e.g. when run via `--ignore-scripts`).
  const tag = require("../install.js").platformTag();
  if (!tag) return null;
  try {
    return require.resolve(
      `@nestoa/${tag}/${process.platform === "win32" ? "nestoa.exe" : "nestoa"}`,
    );
  } catch {
    return null;
  }
}

const bin = resolveBinary();
if (!bin) {
  console.error(
    "nestoa: could not locate the platform-specific binary. " +
      "Was the post-install step skipped? Try reinstalling, or set " +
      "NESTOA_BINARY to the absolute path of a built `nestoa` executable.",
  );
  process.exit(1);
}

const override = process.env.NESTOA_BINARY;
const result = spawnSync(override || bin, process.argv.slice(2), {
  stdio: "inherit",
});
if (result.error) {
  console.error("nestoa:", result.error.message);
  process.exit(1);
}
process.exit(result.status == null ? 1 : result.status);

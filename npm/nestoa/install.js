// Resolves the platform-specific binary that npm installed via
// optionalDependencies and, where possible, puts it in place of the
// `bin/nestoa.js` shim so `node_modules/.bin/nestoa` runs it without starting
// Node. Failures here are non-fatal — the shim stays and has a runtime
// fallback that walks the optional packages.

const fs = require("fs");
const path = require("path");
const { execFileSync } = require("child_process");

function platformTag() {
  const platform = process.platform;
  const arch = process.arch;
  if (platform === "linux") {
    const libc = isMusl() ? "musl" : "gnu";
    if (arch === "x64") return `linux-x64-${libc}`;
    if (arch === "arm64") return `linux-arm64-${libc}`;
  } else if (platform === "darwin") {
    if (arch === "x64") return "darwin-x64";
    if (arch === "arm64") return "darwin-arm64";
  } else if (platform === "win32" && arch === "x64") {
    return "win32-x64-msvc";
  }
  return null;
}

function isMusl() {
  // glibc builds of Node report the runtime glibc version; musl ones don't.
  const report = nodeReport();
  if (report && report.header) {
    if (report.header.glibcVersionRuntime) return false;
    const libs = report.sharedObjects || [];
    if (libs.some((lib) => /musl/i.test(lib))) return true;
  }
  return /musl/i.test(lddVersion());
}

function nodeReport() {
  try {
    return process.report && process.report.getReport();
  } catch {
    return null;
  }
}

function lddVersion() {
  // musl's ldd prints its banner but exits non-zero, so read the output from
  // the error too.
  const { execSync } = require("child_process");
  try {
    return execSync("ldd --version 2>&1", { encoding: "utf8" });
  } catch (err) {
    return `${err.stdout || ""}${err.stderr || ""}`;
  }
}

// Only npm links `.bin/nestoa` straight to the `bin` file, and only outside
// Windows: npm's Windows launchers, pnpm's and Yarn Berry's run it with `node`,
// so it must stay JavaScript there. NESTOA_BINARY is only read by the shim.
function canReplaceShim({ platform, env }) {
  if (platform === "win32") return false;
  if (env.NESTOA_BINARY) return false;
  return /^npm\//.test(env.npm_config_user_agent || "");
}

// Swaps the shim for the binary atomically, and only once the binary has
// answered `--version`. The shim may be a hard link into a package manager's
// store, so it is replaced by a rename, never written through.
function replaceShim(source, shim) {
  const temp = path.join(path.dirname(shim), `.nestoa-${process.pid}`);
  try {
    try {
      fs.linkSync(source, temp);
    } catch {
      fs.copyFileSync(source, temp);
    }
    fs.chmodSync(temp, 0o755);
    const version = execFileSync(temp, ["--version"], { encoding: "utf8" });
    if (!version.startsWith("nestoa ")) {
      throw new Error(`unexpected --version output: ${version.trim()}`);
    }
    fs.renameSync(temp, shim);
    return true;
  } catch (err) {
    console.warn("nestoa: keeping the Node launcher:", err.message);
    return false;
  } finally {
    // When the shim is already a hard link to the binary (a reinstall),
    // renaming onto it is a no-op that leaves `temp` in place.
    fs.rmSync(temp, { force: true });
  }
}

function stageBinary(source, binDir, binName) {
  const target = path.join(binDir, binName);
  try {
    fs.copyFileSync(source, target);
    fs.chmodSync(target, 0o755);
  } catch (err) {
    console.warn("nestoa: failed to stage binary:", err.message);
  }
}

function install(source, binDir, { platform, env }) {
  if (
    canReplaceShim({ platform, env }) &&
    replaceShim(source, path.join(binDir, "nestoa.js"))
  ) {
    return "native";
  }
  stageBinary(source, binDir, platform === "win32" ? "nestoa.exe" : "nestoa");
  return "shim";
}

function main() {
  const tag = platformTag();
  if (!tag) {
    console.warn(
      `nestoa: no prebuilt binary for ${process.platform}-${process.arch}. ` +
        "The CLI will not be runnable until you set NESTOA_BINARY=/path/to/nestoa.",
    );
    return;
  }
  const binName = process.platform === "win32" ? "nestoa.exe" : "nestoa";
  let source;
  try {
    source = require.resolve(`@algoan/nestoa-${tag}/${binName}`);
  } catch {
    console.warn(
      `nestoa: optional dependency @algoan/nestoa-${tag} was not installed; ` +
        "binary will be located at runtime if available.",
    );
    return;
  }
  install(source, path.join(__dirname, "bin"), {
    platform: process.platform,
    env: process.env,
  });
}

module.exports = { platformTag, canReplaceShim, install };

if (require.main === module) {
  main();
}

// Resolves the platform-specific binary that npm installed via
// optionalDependencies and copies it next to this shim so the `bin/nestoa.js`
// shim can find it. Failures here are non-fatal — the shim has a runtime
// fallback that walks the optional packages.

const fs = require("fs");
const path = require("path");

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
  const report = process.report && process.report.getReport();
  if (report && report.header) {
    if (report.header.glibcVersionRuntime) return false;
    const libs = report.sharedObjects || [];
    if (libs.some((lib) => /musl/i.test(lib))) return true;
  }
  return /musl/i.test(lddVersion());
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
    source = require.resolve(`@nestoa/${tag}/${binName}`);
  } catch {
    console.warn(
      `nestoa: optional dependency @nestoa/${tag} was not installed; ` +
        "binary will be located at runtime if available.",
    );
    return;
  }
  const target = path.join(__dirname, "bin", binName);
  try {
    fs.mkdirSync(path.dirname(target), { recursive: true });
    fs.copyFileSync(source, target);
    fs.chmodSync(target, 0o755);
  } catch (err) {
    console.warn("nestoa: failed to stage binary:", err.message);
  }
}

module.exports = { platformTag };

if (require.main === module) {
  main();
}

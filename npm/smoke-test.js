#!/usr/bin/env node
// Installs the launcher and a built platform package the way a user does, from
// packed tarballs, and runs the CLI on a small project: once with the install
// script (npm swaps the shim for the binary outside Windows) and once without
// it (the shim runs the platform package's binary, which must still be
// executable).
//
// Usage:
//   node smoke-test.js <platform-package-dir>

const assert = require("assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { spawnSync } = require("child_process");

// The project's controller imports a DTO, which imports a directory index:
// the DTO's path is canonical, `\\?\C:\...` on Windows.
const PROJECT = {
  "tsconfig.json": "{}\n",
  "src/models/index.ts": "export class Author {\n  name: string;\n}\n",
  "src/book.dto.ts":
    "import { Author } from './models';\n\n" +
    "export class Book {\n  title: string;\n  author: Author;\n}\n",
  "src/books.controller.ts":
    "import { Controller, Get } from '@nestjs/common';\n" +
    "import { Book } from './book.dto';\n\n" +
    "@Controller('books')\nexport class BooksController {\n" +
    "  @Get()\n  list(): Book[] {\n    return [];\n  }\n}\n",
};

// npm and npx are batch files on Windows, which only run through a shell that
// joins the arguments unquoted: a temporary directory may contain spaces.
const WINDOWS = process.platform === "win32";
const quote = (arg) => (WINDOWS && /\s/.test(arg) ? `"${arg}"` : arg);

function run(cmd, args, cwd) {
  const result = spawnSync(cmd, args.map(quote), { cwd, encoding: "utf8", shell: WINDOWS });
  const output = `${result.stdout}${result.stderr}`;
  if (result.status !== 0) {
    throw new Error(`${cmd} ${args.join(" ")} failed (${result.status ?? result.signal}):\n${output}`);
  }
  return output;
}

function pack(dir, dest) {
  return path.join(dest, run("npm", ["pack", "--silent", "--pack-destination", dest], dir).trim());
}

// The launcher, depending only on the platform package under test.
function launcherTarball(platform, tmp) {
  const dir = path.join(tmp, "launcher");
  fs.cpSync(path.join(__dirname, "nestoa"), dir, { recursive: true });
  const pkgPath = path.join(dir, "package.json");
  const pkg = JSON.parse(fs.readFileSync(pkgPath, "utf8"));
  pkg.version = platform.version;
  pkg.optionalDependencies = { [platform.name]: platform.version };
  fs.writeFileSync(pkgPath, JSON.stringify(pkg, null, 2) + "\n");
  return pack(dir, tmp);
}

function checkInstall(tarballs, tmp, name, installFlags) {
  const dir = path.join(tmp, name);
  for (const [file, content] of Object.entries(PROJECT)) {
    fs.mkdirSync(path.dirname(path.join(dir, file)), { recursive: true });
    fs.writeFileSync(path.join(dir, file), content);
  }
  fs.writeFileSync(path.join(dir, "package.json"), '{ "private": true }\n');
  run("npm", ["install", "--no-audit", "--no-fund", ...installFlags, ...tarballs], dir);

  const version = run("npx", ["--no-install", "nestoa", "--version"], dir);
  assert.match(version, /^nestoa \d+\.\d+\.\d+/);
  run("npx", ["--no-install", "nestoa", ".", "--strict", "-o", "openapi.json"], dir);
  const doc = JSON.parse(fs.readFileSync(path.join(dir, "openapi.json"), "utf8"));
  assert.deepEqual(doc.components.schemas.Book.properties.author, {
    $ref: "#/components/schemas/Author",
  });
  console.log(`ok ${name}: ${version.trim()}`);
}

function main() {
  const platformDir = process.argv[2];
  if (!platformDir) {
    console.error("usage: smoke-test.js <platform-package-dir>");
    process.exit(2);
  }
  const platform = JSON.parse(fs.readFileSync(path.join(platformDir, "package.json"), "utf8"));
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "nestoa-smoke-"));
  const tarballs = [launcherTarball(platform, tmp), pack(path.resolve(platformDir), tmp)];
  checkInstall(tarballs, tmp, "with-scripts", []);
  checkInstall(tarballs, tmp, "without-scripts", ["--ignore-scripts"]);
}

main();

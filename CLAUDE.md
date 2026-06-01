# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

**nestoa** is a deterministic, static OpenAPI 3.1 generator for NestJS / TypeScript codebases. It parses TypeScript source with the swc parser (no compilation, no runtime, no reflection) and emits an `openapi.json`/`.yaml` document. The design goal is CI/CD-friendly reproducibility — it is intentionally *not* runtime-pluggable; customization happens via config or one-file source changes.

The Rust analyser lives in `crates/nestoa`. It is distributed on npm as [`nestoa`](npm/nestoa) via prebuilt platform-specific optional dependencies.

## Commands

Run all `cargo` commands from the repo root.

```sh
cargo build --release                 # build the optimized binary -> target/release/nestoa
cargo build                           # debug build -> target/debug/nestoa
cargo test                            # run all tests
cargo test --package nestoa jsdoc     # run tests in one module (e.g. jsdoc / routes)
cargo test test_name                  # run a single test by name

./target/release/nestoa <project>     # analyse a project dir (defaults to ".")
./target/release/nestoa . -o spec.yaml   # YAML output
./target/release/nestoa . --print-config # print resolved config and exit
```

Tests are inline `#[cfg(test)] mod tests` blocks (in `routes.rs`, `jsdoc.rs`, `schema.rs`, `config.rs`, `interceptors.rs`, etc.), not a separate `tests/` directory.

To exercise the npm shim against a built binary without publishing:

```sh
NESTOA_BINARY=$PWD/target/debug/nestoa node npm/nestoa/bin/nestoa.js . -o /tmp/out.json
```

## Architecture

The pipeline is a straight line orchestrated in `crates/nestoa/src/main.rs`; each stage is one module:

1. **`config.rs`** — loads `nestoa.config.{json,yaml,yml}` from the project root (or `--config`). Defines `include`/`exclude` globs, `output`, `basePath`, `detectGlobalPrefix`, `info`, `resolvePackages`, `resolveTsconfigPaths`/`tsconfig`, `decoratorAliases`, and `interceptors`. A relative `output` is resolved against the analysed project dir (not the process CWD), so `nestoa services/foo` always writes `services/foo/openapi.yaml`.
2. **`discover.rs`** — walks `include` paths (walkdir + globset + ignore) collecting `.ts` files.
   - **`globalprefix.rs`** — recovers the NestJS global prefix from the bootstrap file's `app.setGlobalPrefix(...)` call (string literal, local string `const`, or `node-config-ts` member resolved against `config/default.json`). Fills `basePath` when it is unset and `detectGlobalPrefix` is on; an explicit `basePath` always wins.
   - **`tsconfig.rs`** — reads `compilerOptions.paths` from the project's `tsconfig.json` (following `extends` to the config that declares them; JSONC comments and trailing commas are tolerated) and resolves alias patterns (incl. a single `*` wildcard) to in-repo files under the resolved `baseUrl`. This is how workspace-package imports like `@algoan/core` → `libs/core/src` resolve in an Nx/pnpm monorepo.
3. **`parse.rs`** — swc-backed parser; `.d.ts` files are parsed in declaration mode. `ParsingContext` owns the shared `source_map`; source text and file start positions are captured alongside the AST because JSDoc lookups need byte offsets into the original source.
4. **`symbols.rs`** — `Project` builds a cross-file symbol table of every exported class/interface/type-alias/enum, resolving `import`, `export *`, `export * as Ns` (namespaces), barrel files, tsconfig path aliases, and node_modules package declarations so types can be looked up across modules. `resolve_namespace_member` handles namespace-qualified types like `Enums.AccountType`.
5. **`routes.rs`** — `Extractor` finds `@Controller` classes, walks members for HTTP-verb decorators (`@Get`/`@Post`/…/`@All`, `@HttpCode`), maps each parameter to its OpenAPI location (`@Param`/`@Query`/`@Body`/`@Headers` + config aliases), and `to_openapi()` assembles the final document.
6. **`schema.rs`** — lowers TypeScript types to JSON Schema. This is the largest module and where most type-handling lives: primitives, arrays, tuples, unions (`oneOf`/merged enums), intersections (`allOf`), utility types (`Promise`/`Observable`/`Partial`/`Pick`/`Omit`/`Record`/etc.), qualified enum members, `Date`. Reusable named types are collected into `ComponentSchemas` and referenced via `$ref`.
7. **`jsdoc.rs`** — parses leading JSDoc into `summary`/`description`, `@param` descriptions, and `@deprecated`.
8. **`interceptors.rs`** — applies NestJS interceptor transforms to a route's request/response (called from `routes.rs`). For `@UseInterceptors(...)` it resolves each interceptor class and reads its `implements NestInterceptor<In, Out>` type arguments: the handler's (Promise/Observable-unwrapped) return type is unified against `In` to bind the interceptor's generic params (a minimal `unify` + `TypeEnv` of `name -> (origin_file, TsType)`), then `Out` is lowered as the response schema with those bindings active. The motivating case is `@algoan/nestjs-pagination`'s `PaginationBodyInterceptor<T>` (`DataToPaginate<T>` → `PaginatedData<T>`). Request-body reshaping (e.g. single-or-array) is *not* in the type signature, so it comes from config instead.

Key cross-cutting behaviors:

- **Unresolvable types** (typically from `node_modules`) are emitted as `x-unresolved: <TypeName>` rather than silently becoming `{}`, so gaps are visible.
- **`resolvePackages`** is a glob allowlist of npm package names (e.g. `["@algoan/*"]`) whose TypeScript declarations should be parsed so their exported types resolve to real schemas instead of `x-unresolved`. After discovery, `main.rs::expand_packages` walks the import graph and transitively parses the `.d.ts` entry points of matching packages (resolved via `package.json` `exports`/`typesVersions`/`typings`) plus the relative files they re-export. It only ever adds files under `node_modules`, and route extraction is restricted to the originally-discovered `entry_files`, so pulled-in declarations can never introduce phantom routes. Empty by default (no third-party inlining unless opted in). Class `extends` and interface `extends` are both lowered to `allOf` so inherited DTO fields are preserved.
- **`resolveTsconfigPaths`** (default true) turns on tsconfig `paths` alias resolution (see `tsconfig.rs`). `tsconfig` optionally points at a specific config file. Aliased + node_modules type resolution share one transitive `expand_packages` pass: it follows relative re-exports, `export * as` namespace targets, tsconfig aliases, and allowlisted packages, pulling each into the symbol table. Use `resolvePackages` for *published* node_modules packages (e.g. a standalone service that `npm install`s `@algoan/aggregator-raw-data`); tsconfig paths cover *workspace* libs that map to in-repo `libs/*/src` (e.g. the backend monorepo, where the same `@algoan/*` names are NOT in node_modules). Most monorepo services need both: `paths` for workspace libs, `resolvePackages: ["@algoan/*"]` for the genuinely-external `@algoan/nestjs-*` packages.
- **`decoratorAliases`** maps a project-specific decorator name to one of the built-in handler kinds (`body`, `query`, `param`, `header`, `user`, `ignore`) — this is the main extension lever and avoids forking the analyser for custom decorators.
- **`interceptors`** is a config map keyed by interceptor class name (as written in `@UseInterceptors(...)`). The response transform is normally inferred from the interceptor's `NestInterceptor<In, Out>` types (see `interceptors.rs`); this map adds `ignore: true` for pure side-effect interceptors (logging/tracing/scrubbing) and `requestBody: "single-or-array"` for body reshaping the type signature can't express. The interceptor's generic `Out` wrapper (e.g. `PaginatedData<T>`) is *inlined* per route rather than emitted as a shared `$ref` component, so different element types don't collide.

When adding a new utility-type lowering, edit `schema.rs`; a new JSDoc tag, `jsdoc.rs`; a new HTTP decorator or param kind, `routes.rs`; an interceptor transform or `unify` case, `interceptors.rs`; a new module-resolution scheme (alias/package/namespace), `symbols.rs` (+ `tsconfig.rs` for path aliases). The code is organized so each is a single-file change.

## npm distribution (`npm/`)

`nestoa` (the published package) ships only the JS shim. `install.js` (postinstall) detects platform/arch/libc, resolves the matching `@nestoa/<tag>` optional dependency, and copies its binary into `bin/`. `bin/nestoa.js` execs that binary, with a runtime fallback that walks the optional deps (in case `--ignore-scripts` skipped postinstall) and a `NESTOA_BINARY` env override. `build-platform-packages.js` is the CI helper that produces each `@nestoa/<tag>` package per target. All packages publish at the same version.

## `codebase/`

Real NestJS projects (`backend` monorepo, `analysis-manager`, `connect-backend`, `console-backend`, `session-manager`, `openapi-v2`) used as **input fixtures** to validate nestoa against actual code. They are not part of the tool. The `backend` monorepo is the eventual integration target (nestoa will run in its GitHub Actions); the standalone services will be migrated into it.

# Nestjs OpenAPI generator

Deterministic, static OpenAPI generation for NestJS / TypeScript codebases —
designed to be wired into a CI/CD pipeline the same way prettier and oxlint
are.

The implementation lives in [`crates/nestoa`](crates/nestoa) (Rust) and is
distributed through npm as [`nestoa`](npm/nestoa) via prebuilt
platform-specific optional dependencies.

## Quick start

```sh
npm install --save-dev nestoa
npx nestoa                  # writes openapi.json from ./src
npx nestoa -o spec.yaml     # YAML output
```

Optional `nestoa.config.json` (or `.yaml` / `.yml`) at the project root:

```json
{
  "include": ["src"],
  "exclude": ["**/*.spec.ts", "**/dist/**"],
  "output": "openapi.json",
  "basePath": "/api/v1",
  "info": { "title": "My API", "version": "1.2.3" },
  "decoratorAliases": {
    "AuthUser": "user",
    "MongoPaginationParamDecorator": "ignore"
  }
}
```

`decoratorAliases` lets you teach nestoa about project-specific decorators
without modifying its source — the right-hand side picks one of the known
handlers (`body`, `query`, `param`, `header`, `user`, `ignore`).

## How it works

1. **Parse** every `.ts` file in the `include` paths with the swc parser.
2. **Index** every exported class / interface / type alias / enum into a
   cross-file symbol table, following `import`, `export *` and barrel files.
3. **Find** classes annotated with `@Controller` and walk their members for
   HTTP-verb decorators (`@Get`, `@Post`, `@Put`, `@Patch`, `@Delete`,
   `@Head`, `@Options`, `@All`) and `@HttpCode`.
4. **Map** each parameter to its OpenAPI location via `@Param`, `@Query`,
   `@Body`, `@Headers` (plus user-defined aliases). The parameter's TypeScript
   type becomes its schema.
5. **Lower** TypeScript types to JSON Schema, including primitives, arrays,
   tuples, unions (lifted to `oneOf` / merged enums), intersections (`allOf`),
   utility types (`Promise<T>`, `Observable<T>`, `Partial`, `Required`,
   `Readonly`, `NonNullable`, `Pick`, `Omit`, `Record<K,V>`), qualified
   enum-member references (`State.OK`), and `Date` (date-time string).
6. **Carry JSDoc** through to the output: the leading summary becomes
   `summary`, additional paragraphs become `description`, `@param` lines
   become parameter descriptions, `@deprecated` flips the operation flag.
7. **Emit** an OpenAPI 3.1 document (`openapi.json` or `.yaml`).

Unresolvable types (typically from external `node_modules`) are surfaced as
`x-unresolved: <TypeName>` so the gap is visible rather than silently turning
into `{}` — you can fill those in via the config later if needed.

## Repo layout

```
crates/nestoa/           Rust binary (the analyser)
  src/
    main.rs              CLI entry + orchestration
    config.rs            nestoa.config.* loader
    discover.rs          file discovery (walkdir + globset)
    parse.rs             swc-backed TypeScript parser
    symbols.rs           cross-file symbol table + import resolution
    jsdoc.rs             JSDoc parser
    schema.rs            TS type -> JSON Schema lowering
    routes.rs            @Controller / verb-decorator extraction + emitter
npm/
  nestoa/                The npm package users install
    bin/nestoa.js        Node shim that exec's the platform binary
    install.js           postinstall: stage the optional-dep binary
    package.json
  build-platform-packages.js
                         CI helper that produces @nestoa/<tag> packages
```

## Building from source

```sh
cargo build --release
./target/release/nestoa path/to/project
```

## Tests

```sh
cargo test
```

## Release / distribution

Each release builds the binary for every target in
`npm/build-platform-packages.js`, calls that script once per target to create
the `@nestoa/<tag>` packages, then publishes both `nestoa` and every
`@nestoa/<tag>` to npm in the same version. Users who `npm install nestoa`
will only ever resolve the optional-dependency matching their platform.

## Extension model

The tool is intentionally not pluggable at runtime (no WASM, no JS plugin
loader) — for the CI use-case, "predictable and reproducible" beats "dynamic
and customisable". Two levers are exposed instead:

- **Config-driven decorator aliases** — map any project-specific decorator
  onto one of the built-in parameter kinds.
- **Source contributions** — adding a new utility-type lowering or a new
  JSDoc tag is a one-file change in `schema.rs` / `jsdoc.rs`. The code is
  organised to make that the easy path.

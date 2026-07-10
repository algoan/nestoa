# Nestoa - Nestjs OpenAPI generator

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
  "passthroughTypes": ["PopulatedDoc"],
  "decoratorAliases": {
    "AuthUser": "user",
    "MongoPaginationParamDecorator": "ignore"
  },
  "errorResponses": {
    "auth": "global",
    "routes": [{ "whenPathEndsWith": "/aggregate", "add": [409] }]
  }
}
```

`decoratorAliases` lets you teach nestoa about project-specific decorators
without modifying its source — the right-hand side picks one of the known
handlers (`body`, `query`, `param`, `header`, `user`, `ignore`).

`passthroughTypes` lists generic wrapper types that should be treated as
transparent: each is lowered to its first type argument, discarding the wrapper.
This extends the built-in passthrough set (`Promise`, `Observable`, `Partial`,
…) and is meant for library/project wrappers whose API surface is just the type
they wrap — for example mongoose's `PopulatedDoc<T>` (`T | RawId`), so a field
typed `PopulatedDoc<Metadata>` resolves to `Metadata` instead of an
`x-unresolved` placeholder.

### Global prefix

When `basePath` is not set, nestoa recovers the NestJS global prefix from the
bootstrap file by reading the `app.setGlobalPrefix(...)` call. The argument is
resolved when it is a string literal, a local `const` bound to a string, or a
[`node-config-ts`](https://www.npmjs.com/package/node-config-ts) member access
(e.g. `config.globalPrefix`, looked up in `config/default.json`). An explicit
`basePath` always wins, and the detection can be turned off with
`"detectGlobalPrefix": false`.

### Per-path response types (`@route-mapping`)

A single handler can serve several paths (`@Get([':id/cats', ':id/dogs'])`).
When they all return the same shape nestoa handles this automatically. When each
path returns a *different* shape — same logic, one method — the declared return
type is necessarily a union, so nestoa can't tell which path yields which member.
The `@route-mapping` JSDoc tag resolves this by mapping a **trailing path suffix**
to the type that path returns:

```ts
/**
 * Find a pet detail by kind
 * @route-mapping{
 * 'cats': Cat
 * 'dogs': Dog
 * 'birds': Bird
 * }
 */
@Get([':id/cats', ':id/dogs', ':id/birds'])
public async findDetail(...): Promise<Cat | Dog | Bird> { … }
```

Each key matches when the route path equals it or ends with `/<key>`, so the last
segment (`cats`) is usually enough; multi-segment suffixes (`cats/latest`) work
too when the last segment alone is ambiguous. The type name is resolved
against the controller file's imports and emitted as a `$ref`. Any path with no
matching entry falls back to the handler's declared return type.

### Error responses

A handler's return type only describes its *success* response. The `4xx`/`5xx`
responses NestJS actually produces come from guards, pipes, validation, and
thrown exceptions — none of which appear in the signature. nestoa reconstructs
them and every error response references one shared `Error` component
(`{ code, message, status }`).

Two tiers, both configured under `errorResponses`:

**Tier A — auto-detected from route metadata** (emitted with no config needed):

| Response | When |
| --- | --- |
| `401` | The route is authenticated and not opted out. `auth` selects the wiring: `global` (default — an `APP_GUARD`), `per-controller` (only routes with `@UseGuards(AuthGuard)`), or `none`. A decorator in `authOptOut` (default `Unprotected`) turns it off per route/controller. |
| `403` | The route or controller carries a `roleDecorators` decorator (default `Roles`). |
| `400` | The route has a `@Body` DTO and `validation` is on (default true — assumes a global `ValidationPipe`). |
| `404` / `400` | A pipe in the `pipes` map is applied to a `@Param`/`@Query`. Defaults map the `@algoan` `ParseObjectIdPipe`/`ParseDatePipe` to `404` and Nest's built-in parse pipes to `400`. |
| any | The handler body itself contains `throw new <HttpException>(...)` — e.g. `throw new UnprocessableEntityException()` → `422`. Only the handler's own body is scanned (not called services), so it stays deterministic. Turn off with `scanThrows: false`. |

**Tier B — declared** (for errors thrown deep in *service* code that no
controller-level analysis can see):

- The `@throws <status> [description]` JSDoc tag (aliases `@httpError`,
  `@exception`) on the handler:
  ```ts
  /**
   * Aggregate the connection.
   * @throws 409 The connection has already been aggregated
   * @throws 422 Product disabled
   */
  @Post(':id/aggregate')
  ```
- Config `routes[].add` (add status codes per path / path-suffix, optionally
  restricted to one `method` so `POST /customers` can get a `422` its `GET`
  sibling doesn't) and `global` (filter-level codes such as a `413` from an
  Express body-size limit, optionally gated on a suffix).

Everything is overridable — decorator/pipe/guard names, the `auth` scope, and
per-route additions — so an unusual setup is one config change, not a fork.
`500` is never emitted by default: it is a processing failure, not a documented
outcome. Set `"errorResponses": { "enabled": false }` to turn the whole feature
off.

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
   become parameter descriptions, `@deprecated` flips the operation flag,
   `@internal` / `@private` omit a property, `@route-mapping` gives each
   path of a multi-path handler its own response schema, and `@throws` declares
   error responses thrown in service code (see above).
7. **Reconstruct error responses** from guards, pipes, validation, and declared
   throws, referencing a shared `Error` component (see [Error responses](#error-responses)).
8. **Emit** an OpenAPI 3.1 document (`openapi.json` or `.yaml`).

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
    errors.rs            error-response reconstruction (guards/pipes/throws)
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
- **Config-driven error responses** — tune the guard/role/pipe names and auth
  scope, or add service-thrown codes per route, under `errorResponses`.
- **Source contributions** — adding a new utility-type lowering, JSDoc tag, or
  error-response source is a one-file change in `schema.rs` / `jsdoc.rs` /
  `errors.rs`. The code is organised to make that the easy path.

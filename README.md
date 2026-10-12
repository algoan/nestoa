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

### Interceptors

A NestJS interceptor can reshape the payload between the handler and the wire,
so the handler's declared return type is not what the client receives. nestoa
recovers the real response from the interceptor's type signature: for a class
`implements NestInterceptor<In, Out>`, it unifies the handler's (Promise /
Observable-unwrapped) return type against `In` to bind the interceptor's generic
parameters, then lowers `Out` as the response schema.

Lowering `Out` performs **generic instantiation** — a named type carrying
arguments (`PaginatedData<T>`, or `MultiStatusResponse<Omit<T, 'validity'>>`)
binds the referenced declaration's own parameters to those arguments and is
inlined *per route*, so utility types like `Omit` applied to a bound parameter
resolve and two controllers using the same interceptor with different element
types never collide on one component. For example `@algoan/multi-status`'s
`MultiStatusInterceptor<T>` (`T[]` → `MultiStatusResponse<Omit<T, …>>`) yields
the batch envelope with each element's `resource` set to the controller's own
DTO, and `@HttpCode(MULTI_STATUS)` on the handler sets the status to `207` (the
argument may name a `const`, resolved to its value across modules, not only a
numeric literal).

Not every transform lives in the type signature. The `interceptors` config map,
keyed by the class name as written in `@UseInterceptors(...)`, covers the rest:

```json
{
  "interceptors": {
    "LoggingInterceptor": { "ignore": true },
    "BulkBodyInterceptor": { "requestBody": "single-or-array" },
    "SignResponseInterceptor": {
      "response": {
        "whenPathEndsWith": "/jws",
        "schema": { "type": "object", "properties": { "payload": { "type": "string" } } }
      }
    }
  }
}
```

- `ignore` — a pure side-effect interceptor (logging / tracing / scrubbing)
  that never touches the wire shape; skip it.
- `requestBody: "single-or-array"` — the client may send one object or an
  array (emitted as `anyOf: [S, { type: array, items: S }]`); this reshaping is
  imperative, not in the type signature.
- `response: { whenPathEndsWith?, schema }` — an OpenAPI schema used verbatim,
  for interceptors whose wire shape is runtime control flow rather than a type.
  The optional suffix lets sibling routes sharing one interceptor via an array
  path (`@Get([':id', ':id/jws'])`) keep distinct shapes.

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
   `@Head`, `@Options`, `@All`) and `@HttpCode` (whose argument may be a
   numeric literal or a named `const`, resolved across modules).
4. **Map** each parameter to its OpenAPI location via `@Param`, `@Query`,
   `@Body`, `@Headers` (plus user-defined aliases). The parameter's TypeScript
   type becomes its schema.
5. **Lower** TypeScript types to JSON Schema, including primitives, arrays,
   tuples, unions (lifted to `anyOf` / merged enums, with `| null` widening the
   `type` keyword — see [Output conventions](#output-conventions)),
   intersections (`allOf`),
   utility types (`Promise<T>`, `Observable<T>`, `Partial`, `Required`,
   `Readonly`, `NonNullable`, `Pick`, `Omit`, `Record<K,V>`), qualified
   enum-member references (`State.OK`), and `Date` (date-time string).
6. **Rewrite** the response for any `@UseInterceptors(...)` from the
   interceptor's `NestInterceptor<In, Out>` signature, plus config-driven
   request/response reshaping (see [Interceptors](#interceptors)).
7. **Carry JSDoc** through to the output: the leading summary becomes
   `summary`, additional paragraphs become `description`, `@param` lines
   become parameter descriptions, `@deprecated` flips the operation flag,
   `@internal` / `@private` omit a property, `@route-mapping` gives each
   path of a multi-path handler its own response schema, and `@throws` declares
   error responses thrown in service code (see above).
8. **Reconstruct error responses** from guards, pipes, validation, and declared
   throws, referencing a shared `Error` component (see [Error responses](#error-responses)).
9. **Emit** an OpenAPI 3.1 document (`openapi.json` or `.yaml`), or a 3.0.3
   one when [`openapiVersion`](#emitting-openapi-303) asks for it.

Unresolvable types (typically from external `node_modules`) are surfaced as
`x-unresolved: <TypeName>` so the gap is visible rather than silently turning
into `{}` — you can fill those in via the config later if needed.

## Output conventions

The document is OpenAPI 3.1 by default. Most of it is written to read the same
way under 3.0:

- **`$ref` never carries siblings.** Annotations such as a JSDoc `description`
  are legal beside a `$ref` in 3.1, but 3.0's JSON Reference semantics discard
  everything next to the reference ("All other properties in a $ref object are
  ignored"), so a documented reference is emitted as an `allOf` branch instead:

  ```yaml
  type:
    allOf:
      - $ref: '#/components/schemas/AccountType'
    description: Account's type
  ```

- **`undefined` / `void` members drop out of unions.** `T | undefined` lowers to
  plain `T`; optionality is carried by the object's `required` list.
- **Unions are `anyOf`, not `oneOf`.** TypeScript unions may overlap
  (`Base | Child`, where every `Child` is also a `Base`), and `oneOf` rejects a
  value that matches more than one branch. Client generators may map `anyOf`
  differently from `oneOf`.

Two constructs, however, simply have no spelling that is valid in both
versions, so a 3.1 document is never also a 3.0 one. Use
[`openapiVersion`](#emitting-openapi-303) when you need 3.0.3.

- **Nullability.** 3.1 has no `nullable` keyword (a 3.1 consumer ignores it, so
  the field reads as non-nullable) and 3.0 has no `"null"` type. The 3.1 output
  uses JSON Schema 2020-12 throughout: `string | null` widens the type
  (`type: ["string", "null"]`), a nullable enum lists `null` among its values,
  and a nullable union or `$ref` gains an explicit `{ "type": "null" }` member.
- **Tuples.** `[number, number]` lowers to `prefixItems` + `items: false`.
  3.0 arrays are homogeneous and have neither keyword.

A property declared `null` outright lowers to `type: "null"`, which 3.0 also
lacks; the downgrade drops the constraint, keeping any annotations beside it.

### Emitting OpenAPI 3.0.3

Set `openapiVersion` (or pass `--openapi-version`) to emit a document that
validates against 3.0.3:

```json
{
  "openapiVersion": "3.0.3"
}
```

```sh
nestoa . --openapi-version 3.0.3
```

The analyser still lowers types into 3.1; the finished document is then
downgraded in one pass (`dialect.rs`), which also covers schemas you supplied
verbatim through `interceptors[].response.schema`:

| 3.1 | 3.0.3 |
| --- | --- |
| `type: [T, "null"]` | `type: T` + `nullable: true` |
| `enum: [a, b, null]` | `enum: [a, b]` + `nullable: true` |
| `anyOf: [<schema>, {type: "null"}]` | `<schema>` — **nullability is lost** |
| `type: "null"` on its own | an unconstrained schema — **the type is lost** |
| `type: [A, B]` (multi-type) | `anyOf: [{type: A}, {type: B}]` |
| `const: v` | `enum: [v]` |
| `prefixItems: [A, B]` + `items: false` | `items: <A merged with B>` + `maxItems` — **order is lost** |

The nullable-union, standalone-null and tuple rows lose information, and that
is a limit of 3.0.3 rather than a shortcut. `nullable` only means something
beside a `type` keyword, and a union — which is how a nullable `$ref`, `allOf`
or `x-unresolved` is expressed — has none of its own; keeping the flag there
would be a no-op that linters reject (Redocly's `nullable-type-sibling`). So a
3.0.3 document cannot say "this `$ref` may be null", and it has no null-only
schema either. Likewise its arrays are homogeneous, so a tuple keeps its
maximum length — `maxItems`, matching `prefixItems`, which caps an array
without requiring every position — but not its per-position types.

Only the closed tuple the analyser emits (`prefixItems` with `items: false`)
is converted. An open tuple — `items` absent, `true`, or a tail schema — is
passed through untouched: approximating one means inventing bounds and
guessing how to fold the tail into a single element schema, and nothing in the
analyser produces one. `minItems` is never synthesized either, since
`prefixItems` constrains the positions an array has without requiring them.

Instance data is never touched: the pass does not descend into `example`,
`examples`, `default`, `enum`/`const` values or `x-` extensions, so a
documented payload that happens to look like a schema survives unchanged. That
applies to those words as *keywords* only — a DTO field named `example` is
still a schema and is downgraded like any other.

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
cargo build --release          # or: make build
./target/release/nestoa path/to/project
```

## Tests

```sh
cargo test                     # or: make test
```

## Development (Makefile)

A `Makefile` mirrors the CI pipeline (`.github/workflows/ci.yml`) so the same
checks and builds run locally with one command. Run `make help` for the full
list; the common targets are:

```sh
make ci          # everything CI runs: lint + test + release build
make test        # cargo test --workspace --locked
make build       # cargo build --release --workspace --locked
make lint        # fmt check + clippy (deny warnings)
make fmt-fix     # apply formatting fixes
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
- **Config-driven interceptors** — mark side-effect interceptors as ignored,
  declare request-body reshaping, or supply a verbatim response schema, under
  `interceptors`.
- **Source contributions** — adding a new utility-type lowering, JSDoc tag, or
  error-response source is a one-file change in `schema.rs` / `jsdoc.rs` /
  `errors.rs`. The code is organised to make that the easy path.

# nestoa

Static OpenAPI / AsyncAPI generator for NestJS and TypeScript codebases.

`nestoa` analyses TypeScript source code (no runtime, no decorators metadata
required) to produce OpenAPI 3.1 documents from your controllers, DTOs,
interfaces and JSDoc comments. It is implemented in Rust and distributed as
prebuilt platform binaries through npm.

## Install

```sh
npm install --save-dev nestoa
# or
pnpm add -D nestoa
```

The right prebuilt binary (`@nestoa/<platform>`) is selected automatically via
`optionalDependencies`.

## Use

```sh
npx nestoa                  # generates openapi.json from ./src
npx nestoa ./apps/api       # point at a different project root
npx nestoa -o spec.yaml     # YAML output
```

Drop a `nestoa.config.json` (or `.yaml`) at the project root to customise
inputs, output path, base path, info block, and decorator aliases:

```json
{
  "include": ["src"],
  "exclude": ["**/*.spec.ts"],
  "output": "openapi.json",
  "basePath": "/api/v1",
  "info": { "title": "My API", "version": "1.2.3" },
  "decoratorAliases": {
    "AuthUser": "user",
    "ApiHeader": "header"
  }
}
```

## How it works

`nestoa` parses every `.ts` file with the swc parser, builds a cross-file
symbol table, walks classes decorated with `@Controller`, and emits an
OpenAPI document including:

- Routes (`@Get` / `@Post` / `@Put` / `@Patch` / `@Delete` / `@Head` /
  `@Options` / `@All`) with their summary and description sourced from JSDoc.
- Path / query / header parameters via `@Param`, `@Query`, `@Headers`.
- Request bodies via `@Body`.
- Response types inferred from the method return type, unwrapping `Promise`,
  `Observable`, arrays and tuples.
- Schemas for referenced interfaces, type aliases, classes and enums, with
  property-level JSDoc preserved.

## License

MIT

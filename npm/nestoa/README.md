# nestoa

Generate an OpenAPI document from a NestJS project's source code.

nestoa reads your controllers, DTOs and JSDoc comments and writes
`openapi.json` (or `.yaml`). It never compiles or starts your app, so the same
code always gives the same document.

## Install

```ini
# .npmrc
@algoan:registry=https://npm.pkg.github.com
```

```sh
npm install --save-dev @algoan/nestoa
```

The right prebuilt binary for your platform is picked automatically.

## Use

```sh
npx nestoa                # reads ./src, writes ./openapi.json
npx nestoa -o openapi.yaml
npx nestoa --strict       # in CI: fail if anything could not be documented
```

Configuration, CLI and output are documented in the
[repository's docs](https://github.com/algoan/nestoa/tree/main/docs).

## License

MIT

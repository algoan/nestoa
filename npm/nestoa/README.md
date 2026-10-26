# nestoa

Generate an OpenAPI document from a NestJS project's source code.

nestoa reads your controllers, DTOs and JSDoc comments and writes
`openapi.json` (or `.yaml`). It never compiles or starts your app, so the same
code always gives the same document.

## Install

GitHub Packages requires a token even to install: a personal access token
(classic) with `read:packages`, exported as `GITHUB_TOKEN` (never committed).

```ini
# .npmrc
@algoan:registry=https://npm.pkg.github.com
//npm.pkg.github.com/:_authToken=${GITHUB_TOKEN}
```

```sh
npm install --save-dev @algoan/nestoa
```

The right prebuilt binary for your platform is picked automatically.

With npm on macOS and Linux, `node_modules/.bin/nestoa` is that binary itself.
On Windows, with pnpm or Yarn, or when install scripts are disabled, it goes
through a small Node launcher instead, which adds about 30 ms to each run.

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

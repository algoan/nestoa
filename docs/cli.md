# CLI

```sh
nestoa [PROJECT] [OPTIONS]
```

`PROJECT` is the project root to analyse. It defaults to the current directory.

| Option | Effect |
| --- | --- |
| `-c, --config <FILE>` | Config file to use, relative to the current directory. Without it, the first of `nestoa.config.json`, `nestoa.config.yaml`, `nestoa.config.yml` in the project root is used, if any. A `--config` file that doesn't exist is an error. |
| `-o, --output <FILE>` | Output file, relative to the current directory. Overrides [`output`](options/output.md). |
| `--asyncapi-output <FILE>` | Write the [AsyncAPI](options/asyncapi.md) document to this file, relative to the current directory, turning it on with the defaults when the config has no `asyncapi`. |
| `--openapi-version <3.1.0\|3.0.3>` | Overrides [`openapiVersion`](options/openapi-version.md). |
| `--print-config` | Print the resolved configuration (defaults, `extends` and flags applied) and exit. |
| `--strict` | Exit with status 1 after writing the output if there was any warning or any `x-unresolved` type. |

```sh
nestoa                                   # ./src → ./openapi.json
nestoa services/books -o books.yaml      # another project, YAML output
nestoa --print-config                    # check what config is in effect
nestoa --strict                          # for CI
```

## Diagnostics and `--strict`

Progress and warnings go to stderr. A warning flags something nestoa could not
document correctly. For example:

- a route path it could not evaluate (the route is left out),
- an event pattern or a publish call's channel it could not evaluate (the
  operation is left out, see [`asyncapi`](options/asyncapi.md)),
- an `include` entry that doesn't exist,
- an error-response rule that matches no route,
- an error body type (`errorResponses.schema`) that no file, or several, declare,
- a parameter whose name it could not evaluate (the parameter is left out),
- two handlers on the same method and path (the later one wins),
- a handler without a return type that returns a value (its response body is
  left undocumented; declare the return type),
- paths that differ only in parameter names, which OpenAPI forbids.

A type nestoa cannot lower is emitted as `x-unresolved` with its name or source
text, so it never becomes an empty `{}` schema:

```yaml
author:
  x-unresolved: AuthorFromSomeUnparsedPackage
```

With `--strict`, the document is still written. nestoa then lists every
`x-unresolved` by JSON pointer (prefixed with the AsyncAPI file for that document) and exits with status 1 if there was any warning
or unresolved type. Use it in CI to keep the document complete. Most
`x-unresolved` types are fixed with [`resolvePackages`](options/resolve-packages.md),
[`resolveTsconfigPaths`](options/resolve-tsconfig-paths.md) or
[`passthroughTypes`](options/passthrough-types.md).

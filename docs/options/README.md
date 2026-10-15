# Configuration options

nestoa reads `nestoa.config.json`, `nestoa.config.yaml` or `nestoa.config.yml`
from the project root (see [CLI](../cli.md) for `--config` and `extends`).
Every key is optional, and an unknown key is an error.

```json
{
  "include": ["src"],
  "output": "openapi.yaml",
  "info": { "title": "Bookstore API", "version": "1.0.0" }
}
```

## Input

| Key | Default | Page |
| --- | --- | --- |
| `include` | `["src"]` | [include](include.md) |
| `exclude` | `node_modules`, `dist`, test files | [exclude](exclude.md) |
| `resolveTsconfigPaths` | `true` | [resolveTsconfigPaths](resolve-tsconfig-paths.md) |
| `tsconfig` | `tsconfig.json` | [tsconfig](tsconfig.md) |
| `resolvePackages` | `[]` | [resolvePackages](resolve-packages.md) |

## Output

| Key | Default | Page |
| --- | --- | --- |
| `output` | `openapi.json` | [output](output.md) |
| `format` | from the `output` extension | [format](format.md) |
| `openapiVersion` | `3.1.0` | [openapiVersion](openapi-version.md) |
| `info` | `API` / `0.0.0` (see page) | [info](info.md) |

## Paths

| Key | Default | Page |
| --- | --- | --- |
| `basePath` | none | [basePath](base-path.md) |
| `detectGlobalPrefix` | `true` | [detectGlobalPrefix](detect-global-prefix.md) |

## Types and decorators

| Key | Default | Page |
| --- | --- | --- |
| `passthroughTypes` | `[]` | [passthroughTypes](passthrough-types.md) |
| `decoratorAliases` | `{}` | [decoratorAliases](decorator-aliases.md) |
| `interceptors` | `{}` | [interceptors](interceptors.md) |
| `errorResponses` | enabled | [errorResponses](error-responses.md) |

## Composition

| Key | Default | Page |
| --- | --- | --- |
| `extends` | none | [extends](extends.md) |

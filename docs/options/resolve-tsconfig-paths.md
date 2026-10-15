# `resolveTsconfigPaths`

Resolve TypeScript `compilerOptions.paths` aliases.

- **Type:** boolean
- **Default:** `true`

```json
{ "resolveTsconfigPaths": false }
```

## Behaviour

Monorepos often import shared libraries through an alias that maps to source
in the same repository:

```jsonc
// tsconfig.base.json
{
  "compilerOptions": {
    "baseUrl": ".",
    "paths": {
      "@acme/*": ["libs/*/src"],
      "@acme/core/*": ["libs/core/src/*"]
    }
  }
}
```

With this option on, `import { Book } from '@acme/catalog'` resolves to
`libs/catalog/src`, and `Book` becomes a real schema instead of `x-unresolved`.

- The config is read from [`tsconfig`](tsconfig.md), by default `tsconfig.json`
  in the project root.
- `extends` is followed: a relative path, a package such as `@tsconfig/node20`,
  or a TypeScript 5 array, where later entries override earlier ones. Comments
  and trailing commas are accepted.
- Targets are resolved against the inherited `baseUrl`. Without a `baseUrl`,
  they resolve against the directory of the config that declares `paths`.
- Matching follows TypeScript. An exact pattern beats a wildcard, the longest
  wildcard prefix wins among wildcards, and only the winning pattern's targets
  are tried.

Use [`resolvePackages`](resolve-packages.md) for packages that are installed in
`node_modules` instead.

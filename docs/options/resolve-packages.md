# `resolvePackages`

npm packages whose type declarations should be parsed.

- **Type:** array of glob patterns over package names
- **Default:** `[]` (no third-party package is inlined)

```json
{ "resolvePackages": ["@acme/*", "shared-dto"] }
```

## Behaviour

A DTO imported from `node_modules` is normally emitted as `x-unresolved`. For a
package matching one of these patterns, nestoa parses its `.d.ts` entry point
and every file it re-exports, so its exported types become real schemas.

- The entry point is found the way TypeScript finds it: `package.json`
  `exports`, `typesVersions`, `typings` / `types`, then `main`.
- A `.js` / `.mjs` / `.cjs` target or import stands for its `.ts` / `.d.ts`,
  `.mts` / `.d.mts` or `.cts` / `.d.cts` file.
- Packages are followed transitively: a matching package that imports another
  matching package pulls it in too.
- An invalid pattern produces a warning and the list is ignored.

Use it for packages that are actually installed. Workspace libraries mapped
through tsconfig `paths` are covered by
[`resolveTsconfigPaths`](resolve-tsconfig-paths.md) instead. A monorepo service
often needs both.

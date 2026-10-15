# `include`

Directories or files to scan for controllers, relative to the project root.

- **Type:** array of paths
- **Default:** `["src"]`

```json
{ "include": ["src", "apps/api/src"] }
```

## Behaviour

- Each directory is walked recursively. Every `.ts`, `.tsx`, `.mts` and `.cts`
  file that is not [excluded](exclude.md) is parsed.
- A file entry is parsed on its own.
- Entries are paths, not globs. A missing path, or an entry containing glob
  characters (`*`, `?`, `[`, `{`), produces a warning and is skipped. Filter
  with [`exclude`](exclude.md) instead.
- Symbolic links are followed.
- **Routes only come from included files.** Files the project imports from
  outside `include` (a relative `../../libs/dto`, a [tsconfig alias](resolve-tsconfig-paths.md)
  or an [allowlisted package](resolve-packages.md)) are still parsed so their
  types resolve, but a controller found there never adds routes.

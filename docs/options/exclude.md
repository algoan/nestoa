# `exclude`

Glob patterns for files to skip during discovery.

- **Type:** array of glob patterns
- **Default:**
  ```json
  ["**/node_modules/**", "**/dist/**", "**/*.spec.ts", "**/*.test.ts", "**/*.e2e-spec.ts"]
  ```

```json
{ "exclude": ["**/node_modules/**", "**/dist/**", "**/*.spec.ts", "**/fixtures/**"] }
```

## Behaviour

- Patterns match each path **relative to the project root**, so a `dist` or
  `node_modules` folder *above* the project never excludes it.
- A pattern ending in `/**` also stops the walk from entering matching
  directories, so large folders such as `node_modules` are never listed.
- Setting `exclude` **replaces** the default list. Repeat the defaults you
  still want.
- `exclude` only affects discovery. A file excluded here is still parsed if an
  included file imports a type from it.

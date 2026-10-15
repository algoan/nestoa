# `extends`

Inherit settings from another config file.

- **Type:** path, relative to the file that declares it
- **Default:** none

`nestoa.base.json`, shared by every service of a monorepo:

```json
{
  "output": "openapi.yaml",
  "resolvePackages": ["@acme/*"],
  "errorResponses": { "authOptOut": ["Public"] }
}
```

`services/books/nestoa.config.json`:

```json
{
  "extends": "../../nestoa.base.json",
  "info": { "title": "Books", "version": "1.0.0" }
}
```

## Merge rules

- Objects merge key by key, recursively. The extending file wins.
- Arrays and scalars **replace** the base value. `"include": ["lib"]` replaces
  the base `include` instead of appending to it.
- The base can itself use `extends`. A cycle is reported as an error.
- JSON and YAML files can extend each other.
- A missing base file is an error.

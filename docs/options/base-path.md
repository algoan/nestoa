# `basePath`

A prefix added in front of every route path.

- **Type:** string
- **Default:** none (the detected global prefix, see below)

```json
{ "basePath": "/api/v1" }
```

## Behaviour

- `"/api/v1"` turns `@Controller('books') @Get(':id')` into `/api/v1/books/{id}`.
- When `basePath` is unset and [`detectGlobalPrefix`](detect-global-prefix.md)
  is on, the prefix passed to `app.setGlobalPrefix(...)` in the bootstrap file
  is used instead.
- An explicit `basePath` always wins. The detected prefix's `exclude` option is
  then ignored too, so every route gets `basePath`.
- [URI versioning](detect-global-prefix.md#versioning) segments go after the
  base path: `/api/v1` + version `2` → `/api/v1/v2/books`.

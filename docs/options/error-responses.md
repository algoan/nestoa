# `errorResponses`

Rebuild the error responses NestJS produces, which never appear in a handler's
return type.

- **Type:** object
- **Default:** enabled, with the defaults below

```json
{
  "errorResponses": {
    "auth": "global",
    "authOptOut": ["Public"],
    "routes": [
      { "whenPathEndsWith": "/publish", "method": "post", "add": [409] }
    ],
    "global": [{ "status": 413, "whenPathEndsWith": "/cover" }]
  }
}
```

Every error response references one shared `Error` component:

```yaml
Error:
  type: object
  properties:
    code: { type: string }
    message: { type: string }
    status: { type: integer }
```

This is the body of a custom global exception filter, **not** NestJS's stock
error body (`{ statusCode, message, error }`). nestoa does not read your
application's filter. If your app keeps Nest's default filter, the documented
error bodies will not match what it serves.

`500` is never added by default: it is a failure, not a documented outcome.

## Fields

| Field | Default | Effect |
| --- | --- | --- |
| `enabled` | `true` | `false` turns the feature off: no error response is emitted. |
| `auth` | `"global"` | Where `401` applies. `global`: every route (an `APP_GUARD`). `per-controller`: routes whose controller or method has `@UseGuards(<one of authGuards>)`. `none`: never. |
| `authGuards` | `["AuthGuard"]` | Guard class names that mark a route as authenticated in `per-controller` mode. |
| `authOptOut` | `["Unprotected"]` | Decorators that remove `401` from a route, or from its whole controller. |
| `roleDecorators` | `["Roles"]` | Decorators that add `403` to a route or controller. |
| `validation` | `true` | Add `400` to every route with a `@Body`, assuming a global `ValidationPipe`. |
| `scanThrows` | `true` | Add the status of each `throw new <HttpException>(…)` in the handler's own body. |
| `pipes` | see below | Pipe class name → status it throws on bad input, for pipes on `@Param` / `@Query`. |
| `global` | `[]` | Codes added to every route, optionally limited to a path suffix. |
| `routes` | `[]` | Codes added to matching routes. |

Default `pipes`:

| Pipe | Status |
| --- | --- |
| `ParseObjectIdPipe`, `ParseDatePipe` | `404` |
| `ParseEnumPipe`, `ParseBoolPipe`, `ParseIntPipe`, `ParseFloatPipe`, `ParseUUIDPipe`, `ParseArrayPipe`, `ValidationPipe` | `400` |

Setting `pipes` replaces this whole list, so repeat the defaults you still need.

## Sources, in priority order

When two sources add the same status, the first one keeps its description.

1. **Route metadata** (no config needed): `401`, `403`, `400` and pipe statuses,
   as described above.
2. **`global`** entries.
3. **Thrown in the handler**, when `scanThrows` is on. Only the handler's own
   body is read, never the services it calls, so the result stays
   deterministic:
   ```ts
   @Post()
   create(@Body() dto: CreateBookDto) {
     if (!dto.title) throw new UnprocessableEntityException(); // → 422
   }
   ```
4. **The `@throws` JSDoc tag**, for errors thrown deep in service code. See
   [JSDoc tags](../jsdoc.md#throws).
5. **`routes`** entries.

## `global` entries

| Field | Required | Effect |
| --- | --- | --- |
| `status` | yes | Status code to add. |
| `whenPathEndsWith` | no | Only routes whose path ends with this suffix. |
| `description` | no | Response description. Defaults to the status text. |

## `routes` entries

| Field | Required | Effect |
| --- | --- | --- |
| `add` | yes | Status codes to add. |
| `whenPathEndsWith` | one of the two | Match routes whose path ends with this suffix, on whole segments. |
| `path` | one of the two | Match the route whose path is exactly this. |
| `method` | no | Only this HTTP method (case-insensitive). Without it, every method on the path matches. |

Paths are **full** paths, global prefix included, and may use Express `:id` or
OpenAPI `{id}` parameters: `/api/books/:id` matches `/api/books/{id}`. A rule
that matches no route produces a warning, usually because the global prefix is
missing.

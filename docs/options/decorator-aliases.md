# `decoratorAliases`

Teach nestoa what a project-specific parameter decorator means.

- **Type:** object mapping a decorator name to a handler kind
- **Default:** `{}`

```json
{
  "decoratorAliases": {
    "CurrentUser": "user",
    "ValidatedBody": "body",
    "PaginationParams": "ignore",
    "TenantId": "header"
  }
}
```

## Handler kinds

| Kind | Treated as |
| --- | --- |
| `body` | `@Body` — the request body |
| `query` | `@Query` — query parameters |
| `param` | `@Param` — path parameters |
| `header` | `@Headers` — request headers |
| `user` | The authenticated user, filled in by the framework. Not part of the request, so dropped. |
| `ignore` | Dropped from the route signature |

A parameter carrying an unknown decorator is not documented. Map the decorator
here instead of changing nestoa's source.

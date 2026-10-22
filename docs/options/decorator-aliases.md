# `decoratorAliases`

Teach nestoa what a project-specific parameter decorator means.

- **Type:** object mapping a decorator name to a handler kind, or to the
  [query parameters it reads](#query-parameters-read-by-a-decorator)
- **Default:** `{}`

```json
{
  "decoratorAliases": {
    "CurrentUser": "user",
    "ValidatedBody": "body",
    "AuditContext": "ignore",
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

## Query parameters read by a decorator

A decorator built with `createParamDecorator` can read query parameters and hand
the handler something else. The parameter's type then says nothing about the
request, so declare what the decorator reads:

```ts
@Get()
list(@Paginate({ perPageName: 'limit' }) page: PageQuery) { … } // reads ?page, ?limit, ?sort
```

```json
{
  "decoratorAliases": {
    "Paginate": {
      "query": {
        "page": { "schema": { "type": "integer", "default": 1 } },
        "per_page": {
          "nameFrom": "perPageName",
          "schema": { "type": "integer", "default": 10 }
        },
        "sort": {
          "schema": { "type": "string" },
          "description": "Field to sort by"
        }
      }
    }
  }
}
```

Each entry of `query` is one `in: query` parameter, keyed by its name:

| Field | Type | Effect |
| --- | --- | --- |
| `schema` | OpenAPI schema | The parameter's schema, used verbatim. Required. |
| `required` | boolean | Whether the client must send it. Defaults to `false`. |
| `description` | string | The parameter's description. |
| `nameFrom` | string | A property of the decorator's options argument that renames the parameter. The route above reads `?limit`; a route with `@Paginate()` reads `?per_page`. |

The options argument can be an object literal or a constant holding one. When
the name can't be read, the parameter keeps its key as name, with a warning:
the property's value can't be evaluated, or a spread (`{ ...OPTIONS }`) or a
computed key comes after it and may override it.

The declared parameters come after the handler's own `@Query()` parameters. A
handler parameter of the same name is merged with the declared one, as two
`@Query('page')` would be. The decorated parameter itself is not documented.

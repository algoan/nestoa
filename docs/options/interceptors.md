# `interceptors`

How each NestJS interceptor changes the request or response, for the cases its
types don't express.

- **Type:** object keyed by interceptor class name, as written in
  `@UseInterceptors(...)`
- **Default:** `{}`

```json
{
  "interceptors": {
    "LoggingInterceptor": { "ignore": true },
    "BulkBodyInterceptor": { "requestBody": "single-or-array" },
    "SigningInterceptor": {
      "response": {
        "whenPathEndsWith": "/signed",
        "schema": {
          "type": "object",
          "properties": {
            "protected": { "type": "string" },
            "payload": { "type": "string" },
            "signature": { "type": "string" }
          },
          "required": ["protected", "payload", "signature"]
        }
      }
    }
  }
}
```

## Without configuration: inferred from types

An interceptor sits between the handler and the wire, so the handler's return
type may not be what the client receives. For a class declared as
`implements NestInterceptor<In, Out>`, nestoa:

1. unwraps the handler's return type (`Promise` / `Observable`),
2. matches it against `In` to bind the interceptor's generic parameters,
3. lowers `Out` with those bindings as the response schema.

```ts
export class PaginateInterceptor<T>
  implements NestInterceptor<Page<T>, PaginatedData<T>> { … }

@Get()
@UseInterceptors(PaginateInterceptor)
list(): Promise<Page<Book>> { … } // response: PaginatedData<T> with T = Book, inlined
```

Generic types reached from `Out` are **inlined per route**, so two
controllers using the same interceptor with different element types never share
one component. Utility types work on bound parameters: `Out` can be
`BatchResponse<Omit<T, 'createdAt'>>`, for example. A self-referencing generic
(`TreeNode<T>` holding `TreeNode<T>[]`) becomes a named component such as
`TreeNodeBook`, so it is never inlined forever.

## Fields

| Field | Type | Effect |
| --- | --- | --- |
| `ignore` | boolean | The interceptor only has side effects (logging, tracing, metrics) and never changes the payload. It is skipped. |
| `requestBody` | `"single-or-array"` | The client may send one object or an array of them, and the interceptor turns it into an array. The body is emitted as `anyOf: [S, { type: array, items: S }]`. |
| `response.schema` | OpenAPI schema | Used verbatim as the response body. Use it when the transform is runtime logic rather than a type. |
| `response.whenPathEndsWith` | string | Only apply `response` to routes whose path ends with this suffix. Without it, the override applies to every route using the interceptor. |

The suffix lets sibling routes of one handler keep different shapes:

```ts
@Get([':id', ':id/signed'])
@UseInterceptors(SigningInterceptor)
findOne(): Promise<Book> { … }
// GET /books/{id}        → Book
// GET /books/{id}/signed → the configured envelope
```

A `response` override wins over the type-inferred `Out`. When several of a
route's interceptors have a matching override, the last one wins: controller
interceptors come first, then the method's, each in `@UseInterceptors(...)`
order. The suffix check here is a plain string suffix. For one handler that
returns a different type per path without an interceptor, use the
[`@route-mapping`](../jsdoc.md#route-mapping) JSDoc tag.

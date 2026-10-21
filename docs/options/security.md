# `security`

Document which operations need credentials, and how they are sent.

- **Type:** object
- **Default:** enabled, a bearer token named `bearerAuth`

```json
{
  "security": {
    "name": "bearerAuth",
    "scheme": { "type": "http", "scheme": "bearer", "bearerFormat": "JWT" }
  }
}
```

The scheme is emitted once under `components.securitySchemes`, and each
operation states its own requirement:

```yaml
security:
  - bearerAuth: []
```

## Which operations are secured

The operations that get a `401` from [`errorResponses`](error-responses.md)
are the secured ones: its `auth` scope, `authGuards` and `authOptOut` decide
both, even when `errorResponses.enabled` is `false`. Besides, a handler
reading `@Headers('Authorization')` is secured too, typically one that opts
out of the guard to check the header itself:

| Handler | `security` |
| --- | --- |
| guarded, not opted out | `[{ bearerAuth: [] }]` |
| reads a required `Authorization` header | `[{ bearerAuth: [] }]` |
| reads an optional one (`auth?: string`) | `[{}, { bearerAuth: [] }]`, credentials optional |
| anything else | `[]`, public |

The document has no top-level `security`, so an operation's own list is the
whole story.

## Fields

| Field | Default | Effect |
| --- | --- | --- |
| `enabled` | `true` | `false` leaves out `securitySchemes` and every operation's `security`. |
| `name` | `"bearerAuth"` | The scheme's key in `components.securitySchemes`, referenced by each operation. |
| `scheme` | `{ "type": "http", "scheme": "bearer" }` | The [Security Scheme Object](https://spec.openapis.org/oas/v3.1.0#security-scheme-object), emitted verbatim: an `apiKey`, `oauth2` or `openIdConnect` scheme works too. |

When documents are merged with others that declare the scheme under another
name, set `name` to that name so every operation refers to it.

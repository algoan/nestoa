# `info`

The document's `info` object.

- **Type:** object
- **Default:** empty `title` and `version` when `info` is absent

| Field | Type | Default |
| --- | --- | --- |
| `title` | string | `"API"` when `info` is set without it |
| `version` | string | `"0.0.0"` when `info` is set without it |
| `description` | string | none |

```json
{
  "info": {
    "title": "Bookstore API",
    "version": "1.4.0",
    "description": "Books, authors and reviews."
  }
}
```

The fields are copied to the output as they are. Other `info` fields are rejected.

Without an `info` key, both `title` and `version` are empty strings, which
OpenAPI validators reject. Always set `info`.

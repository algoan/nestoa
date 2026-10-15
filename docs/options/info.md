# `info`

The document's `info` object.

- **Type:** object
- **Default:** `{ "title": "API", "version": "0.0.0" }`, except when a config
  file exists without an `info` key (see below)

| Field | Type | Default |
| --- | --- | --- |
| `title` | string | `"API"` |
| `version` | string | `"0.0.0"` |
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

| Situation | `title` / `version` |
| --- | --- |
| No config file | `"API"` / `"0.0.0"` |
| Config file with `info`, field missing | `"API"` / `"0.0.0"` |
| Config file **without** an `info` key | empty strings, which OpenAPI validators reject |

Whenever you have a config file, set `info`.

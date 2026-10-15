# `format`

Serialisation of the output document.

- **Type:** `"json"` or `"yaml"`
- **Default:** inferred from the [`output`](output.md) extension (`.yaml` /
  `.yml` → YAML, otherwise JSON)

```json
{ "output": "openapi.txt", "format": "yaml" }
```

Only needed when the output file's extension does not say which format it is.

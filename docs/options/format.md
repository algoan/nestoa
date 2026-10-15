# `format`

Serialisation of the output document.

- **Type:** `"json"` or `"yaml"`
- **Default:** inferred from the [`output`](output.md) extension (`.yaml` /
  `.yml` → YAML, otherwise JSON)

```json
{ "output": "openapi.txt", "format": "yaml" }
```

An explicit `format` wins over the extension: `"output": "openapi.json"` with
`"format": "yaml"` writes YAML. Set it when the extension is ambiguous or
you need a format other than the one it implies.

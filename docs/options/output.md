# `output`

Where the generated document is written.

- **Type:** path
- **Default:** `openapi.json`

```json
{ "output": "openapi.yaml" }
```

## Behaviour

- A relative path in the config is resolved against the **analysed project
  directory**, not the current directory. `nestoa services/books` writes
  `services/books/openapi.yaml` wherever it is run from.
- The `-o` / `--output` flag overrides it. Like any CLI argument, the flag is
  relative to the current directory.
- Missing parent directories are created.
- The extension picks the [format](format.md) unless `format` is set:
  `.yaml` / `.yml` give YAML, anything else gives JSON.

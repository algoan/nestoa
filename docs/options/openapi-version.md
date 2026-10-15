# `openapiVersion`

The OpenAPI version of the emitted document.

- **Type:** `"3.1.0"` or `"3.0.3"`
- **Default:** `"3.1.0"`
- **CLI:** `--openapi-version 3.0.3` overrides the config.

```json
{ "openapiVersion": "3.0.3" }
```

## Behaviour

nestoa always lowers types into OpenAPI 3.1 (JSON Schema 2020-12). With
`3.0.3`, the finished document is rewritten in one pass to the 3.0.3 subset.
That pass also covers schemas supplied verbatim through
[`interceptors`](interceptors.md).

Nullable unions, null-only schemas and tuples cannot be expressed exactly in
3.0.3, so those conversions lose information. See
[Output conventions](../output.md#emitting-openapi-303) for the full table.

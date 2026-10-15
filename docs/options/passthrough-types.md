# `passthroughTypes`

Generic wrapper types to treat as transparent.

- **Type:** array of type names
- **Default:** `[]`

```json
{ "passthroughTypes": ["PopulatedDoc", "Lazy"] }
```

## Behaviour

Each listed type is lowered to its **first type argument**, and the wrapper is
dropped. `Lazy<Author>` with `["Lazy"]` produces the same schema as `Author`.

This extends the built-in set (`Promise`, `Observable`, `Readonly`,
`NonNullable`). A listed name is checked before the built-in handling, and a
bare reference without type arguments is left as it is. It is meant for library or project wrappers
whose wire shape is just the wrapped type. For example, mongoose's
`PopulatedDoc<T>` is `T | ObjectId`, and only the populated `T` is meaningful
in a response:

```ts
export class Book {
  author: PopulatedDoc<Author>; // → $ref: Author, instead of x-unresolved
}
```

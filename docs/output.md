# Output conventions

## What gets documented

For every `@Controller` class in the [included](options/include.md) files,
including handlers inherited through `extends`:

- **Operations** from `@Get`, `@Post`, `@Put`, `@Patch`, `@Delete`, `@Head`,
  `@Options` and `@All` (expanded to every method). When several verb
  decorators are stacked, the topmost one wins, as in NestJS.
- **Paths** built from the [base path](options/base-path.md), the version, the
  controller path and the route path. A path can be a literal or a constant
  (`ROUTES.LIST`, `` `redirect/${Species.CAT}` ``, `BASE + '/x'`, a string enum
  member), followed across modules. An optional parameter (`:id?`, `{/:id}`)
  produces both paths. A constraint (`:id(\\d+)`) becomes the parameter's
  `pattern`.
- **Parameters** from `@Param`, `@Query`, `@Headers` and
  [aliased decorators](options/decorator-aliases.md). An object `@Query()` or
  `@Param()` produces one parameter per property. `@Headers()` without a name,
  and the `Accept`, `Content-Type` and `Authorization` headers, are left out.
  Pipes shape the parameter: `DefaultValuePipe` makes it optional with a
  `default`, and `ParseIntPipe`, `ParseFloatPipe`, `ParseBoolPipe` and
  `ParseUUIDPipe` set its type.
- **Request body** from `@Body`.
- **Success response** from the return type, after unwrapping `Promise` and
  `Observable`. The status is `201` for `POST` and `200` otherwise, unless
  `@HttpCode(...)` says otherwise. Its argument can be a number, a named
  constant or an enum member such as `HttpStatus.NO_CONTENT`.
- **Error responses**, see [`errorResponses`](options/error-responses.md).
- **Tags**: each operation is tagged with its controller's class name, without
  the `Controller` suffix (`BooksController` → `Books`).
- **Summaries and descriptions** from [JSDoc](jsdoc.md).

## How types are lowered

| TypeScript | Schema |
| --- | --- |
| `string`, `number`, `boolean` | the same `type` |
| `Date` | `type: string`, `format: date-time` |
| `ObjectId`, `Buffer` | `type: string` |
| `T[]`, `Array<T>`, `readonly T[]` | `type: array` |
| `[A, B]` | `prefixItems` + `items: false` |
| `A \| B` | `anyOf`, or one merged `enum` for literals |
| `A & B` | `allOf` |
| `class B extends A`, `interface B extends A` | `allOf` with the base |
| `enum`, `Enum.MEMBER`, `keyof typeof Enum` | `enum` |
| `Partial`, `Required`, `Pick`, `Omit`, `Record<K, V>`, `Readonly`, `NonNullable` | the reshaped object |
| `PartialType`, `PickType`, `OmitType`, `IntersectionType` from `@nestjs/*` | the reshaped object |
| `Exclude`, `Extract` | the filtered union |
| `X['key']` | the property's schema |
| a named class, interface, type alias or enum | a `$ref` to a component |

Static class members are never part of a schema. A type nestoa does not lower
(mapped and conditional types, `typeof`, …) or cannot find is emitted as
`x-unresolved` with its source text, never as an empty schema. See
[CLI](cli.md#diagnostics-and---strict).

## OpenAPI 3.1 and 3.0

The document is OpenAPI 3.1 by default. Most of it is written to read the same
way under 3.0:

- **`$ref` never carries siblings.** Annotations such as a JSDoc `description`
  are legal beside a `$ref` in 3.1, but 3.0's JSON Reference semantics discard
  everything next to the reference ("All other properties in a $ref object are
  ignored"), so a documented reference is emitted as an `allOf` branch instead:

  ```yaml
  genre:
    allOf:
      - $ref: '#/components/schemas/Genre'
    description: The book's genre
  ```

- **`undefined` / `void` members drop out of unions.** `T | undefined` lowers to
  plain `T`; optionality is carried by the object's `required` list.
- **Unions are `anyOf`, not `oneOf`.** TypeScript unions may overlap
  (`Base | Child`, where every `Child` is also a `Base`), and `oneOf` rejects a
  value that matches more than one branch. Client generators may map `anyOf`
  differently from `oneOf`.

Two constructs, however, simply have no spelling that is valid in both
versions, so a 3.1 document is never also a 3.0 one. Use
[`openapiVersion`](options/openapi-version.md) when you need 3.0.3.

- **Nullability.** 3.1 has no `nullable` keyword (a 3.1 consumer ignores it, so
  the field reads as non-nullable) and 3.0 has no `"null"` type. The 3.1 output
  uses JSON Schema 2020-12 throughout: `string | null` widens the type
  (`type: ["string", "null"]`), a nullable enum lists `null` among its values,
  and a nullable union or `$ref` gains an explicit `{ "type": "null" }` member.
- **Tuples.** `[number, number]` lowers to `prefixItems` + `items: false`.
  3.0 arrays are homogeneous and have neither keyword.

A property declared `null` outright lowers to `type: "null"`, which 3.0 also
lacks; the downgrade drops the constraint, keeping any annotations beside it.

### Emitting OpenAPI 3.0.3

Set `openapiVersion` (or pass `--openapi-version`) to emit a document that
validates against 3.0.3:

```json
{
  "openapiVersion": "3.0.3"
}
```

```sh
nestoa . --openapi-version 3.0.3
```

The analyser still lowers types into 3.1; the finished document is then
downgraded in one pass, which also covers schemas you supplied
verbatim through `interceptors[].response.schema`:

| 3.1 | 3.0.3 |
| --- | --- |
| `type: [T, "null"]` | `type: T` + `nullable: true` |
| `enum: [a, b, null]` | `enum: [a, b]` + `nullable: true` |
| `anyOf: [<schema>, {type: "null"}]` | `<schema>` — **nullability is lost** |
| `type: "null"` on its own | an unconstrained schema — **the type is lost** |
| `type: [A, B]` (multi-type) | `anyOf: [{type: A}, {type: B}]` |
| `const: v` | `enum: [v]` |
| `prefixItems: [A, B]` + `items: false` | `items: <A merged with B>` + `maxItems` — **order is lost** |

The nullable-union, standalone-null and tuple rows lose information, and that
is a limit of 3.0.3 rather than a shortcut. `nullable` only means something
beside a `type` keyword, and a union — which is how a nullable `$ref`, `allOf`
or `x-unresolved` is expressed — has none of its own; keeping the flag there
would be a no-op that linters reject (Redocly's `nullable-type-sibling`). So a
3.0.3 document cannot say "this `$ref` may be null", and it has no null-only
schema either. Likewise its arrays are homogeneous, so a tuple keeps its
maximum length — `maxItems`, matching `prefixItems`, which caps an array
without requiring every position — but not its per-position types.

Only the closed tuple the analyser emits (`prefixItems` with `items: false`)
is converted. An open tuple — `items` absent, `true`, or a tail schema — is
passed through untouched: approximating one means inventing bounds and
guessing how to fold the tail into a single element schema, and nothing in the
analyser produces one. `minItems` is never synthesized either, since
`prefixItems` constrains the positions an array has without requiring them.

Instance data is never touched: the pass does not descend into `example`,
`examples`, `default`, `enum`/`const` values or `x-` extensions, so a
documented payload that happens to look like a schema survives unchanged. That
applies to those words as *keywords* only — a DTO field named `example` is
still a schema and is downgraded like any other.

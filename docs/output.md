# Output conventions

## What gets documented

For every `@Controller` class in the [included](options/include.md) files,
including handlers inherited through `extends`:

- **Operations** from `@Get`, `@Post`, `@Put`, `@Patch`, `@Delete`, `@Head`,
  `@Options`, `@All` (expanded to every method) and `@Sse` (a `GET`). When
  several verb decorators are stacked, the topmost one wins, as in NestJS.
  Each operation's `operationId` is `<Controller>_<method>`
  (`BooksController_findOne`); when one handler serves several paths or
  methods, the later ones in the document get `_2`, `_3`, ….
- **Paths** built from the [base path](options/base-path.md), the version, the
  controller path and the route path. A path can be a literal or a constant
  (`ROUTES.LIST`, `` `redirect/${Species.CAT}` ``, `BASE + '/x'`, a string enum
  member), followed across modules. An optional parameter (`:id?`, `{/:id}`)
  produces both paths. A constraint (`:id(\\d+)`) becomes the parameter's
  `pattern`.
- **Parameters** from `@Param`, `@Query`, `@Headers` and
  [aliased decorators](options/decorator-aliases.md), including the query
  parameters a custom decorator is declared to read. An object `@Query()` or
  `@Param()` produces one parameter per property. `@Headers()` without a name,
  and the `Accept`, `Content-Type` and `Authorization` headers, are left out;
  `Authorization` marks the operation as [secured](options/security.md).
  Pipes shape the parameter: `DefaultValuePipe` makes it optional with a
  `default`, and `ParseIntPipe`, `ParseFloatPipe`, `ParseBoolPipe` and
  `ParseUUIDPipe` set its type.
- **Request body** from `@Body`, as `application/json`. A file-upload
  interceptor makes it `multipart/form-data`, see [File uploads](#file-uploads).
- **Success response** from the return type, after unwrapping `Promise` and
  `Observable`, and after the route's [interceptors](options/interceptors.md).
  See [Success responses](#success-responses).
- **Error responses**, see [`errorResponses`](options/error-responses.md).
- **Security**: each operation lists the [`security`](options/security.md)
  scheme it needs, or `security: []` when it is public.
- **Tags**: each operation is tagged with its controller's class name, without
  the `Controller` suffix (`BooksController` → `Books`).
- **Summaries and descriptions** from [JSDoc](jsdoc.md).

### Success responses

The status is `201` for `POST` and `200` otherwise, unless `@HttpCode(...)`
says otherwise. Its argument can be a number, a constant (`MULTI_STATUS`,
`STATUS.ACCEPTED`) or an enum member such as `HttpStatus.NO_CONTENT`.

| Handler | Response |
| --- | --- |
| returns a type | that type, as `application/json` |
| `@Header('Content-Type', 'text/csv')` | the same schema under that media type |
| returns `StreamableFile` | `type: string`, `format: binary`, as `application/octet-stream` or the `@Header('Content-Type', …)` type |
| `@Sse()`, even under another verb decorator | `200`, whatever `@HttpCode` says, with `type: string` as `text/event-stream` |
| `@Redirect(url, status)` | the redirect status (`302` by default, whatever `@HttpCode` says), with a `Location` header and no body |
| status `204`, `205` or `304` | no body, whatever the return type |
| no return type | no body, unless an interceptor's output doesn't depend on its input; a warning when the handler returns a value |

A `StreamableFile` class of your own is lowered like any other type. A handler
returning `{ url, statusCode }` from a `@Redirect` route changes the status at
runtime, which nestoa doesn't follow.

### File uploads

A handler whose `@UseInterceptors(...)`, on it or on its controller, holds a
file-upload interceptor from `@nestjs/platform-express` takes a
`multipart/form-data` body. Each file is a `type: string`, `format: binary`
property:

| Interceptor | Properties |
| --- | --- |
| `FileInterceptor('cover')` | `cover`, one file |
| `FilesInterceptor('photos', 5)` | `photos`, an array of files with `maxItems: 5` |
| `FileFieldsInterceptor([{ name: 'front', maxCount: 1 }, { name: 'back' }])` | `front` and `back`, arrays of files |
| `AnyFilesInterceptor()` | files under any other field name (`additionalProperties`) |
| `NoFilesInterceptor()` | none: a multipart body of `@Body` fields only |

The `@Body` DTO or `@Body('field')` properties sit beside the files: inline
for `@Body('field')`, in an `allOf` with the DTO's `$ref` otherwise. The file
of a `FileInterceptor` or `FilesInterceptor` is required when the
`@UploadedFile()` / `@UploadedFiles()` parameter isn't optional (`file?:`).
The `400` of [`validation`](options/error-responses.md#fields) still only
applies to a route with a `@Body`.

## How types are lowered

| TypeScript | Schema |
| --- | --- |
| `string`, `number`, `boolean` | the same `type` |
| `Date` | `type: string`, `format: date-time` |
| `ObjectId`, `Buffer` | `type: string` |
| `T[]`, `Array<T>`, `readonly T[]` | `type: array` |
| `[A, B]` | `prefixItems` + `items: false` |
| `[A, ...B[]]` | `prefixItems` + `items: B` |
| `42`, `'a'`, `true` | a one-value `enum` (a whole number is written `42`, not `42.0`) |
| `A \| B` | `anyOf`, or one merged `enum` for literals |
| `A & B` | `allOf` |
| `class B extends A`, `interface B extends A` | `allOf` with the base |
| `enum`, `Enum.MEMBER`, `keyof typeof Enum` | `enum`, with computed members (`1 << 2`, `A \| B`, `B = A`) evaluated |
| `Partial`, `Required`, `Pick`, `Omit`, `Readonly` | the reshaped object |
| `NonNullable<T>` | `T` without its `null` member |
| `Record<'a' \| 'b', V>` | `properties` `a` and `b`, both required |
| `Record<string, V>`, `Map<K, V>` | `additionalProperties: V` |
| `Set<T>` | `type: array`, `uniqueItems: true` |
| `Awaited<T>`, `Promise<T>`, `Observable<T>` | `T` |
| `PartialType`, `PickType`, `OmitType`, `IntersectionType` from `@nestjs/*` | the reshaped object |
| `Exclude`, `Extract` | the filtered union |
| `X['key']` | the property's schema |
| a named class, interface, type alias or enum | a `$ref` to a component |
| a generic one with type arguments, `Page<Book>` | a `$ref` to one component per distinct set of arguments, `PageBook`; missing arguments take their defaults |

### Properties

A class's properties are its fields and its constructor parameter properties
(`constructor(public name: string)`). Static members are never part of a
schema, and neither is a property marked `@internal`/`@private` in
[JSDoc](jsdoc.md), `@ApiHideProperty()`, or class-transformer's `@Exclude()`
(unless `toClassOnly: true` without `toPlainOnly: true`, which only drops it
from incoming payloads). A
subclass hiding a property this way hides the inherited one too. A
field without a type annotation takes the type of its initialiser
(`count = 0` is a `number`, `at = new Date()` a date-time string); one nestoa
cannot infer is `x-unresolved`.

A property is left out of `required` when it is declared with `?`, when its
type admits `undefined` (`string | undefined`, directly or through a type
alias), or when it carries
`@IsOptional()`, `@ApiPropertyOptional()` or `@ApiProperty({ required: false })`.

### Validation constraints

The class-validator decorators with a JSON Schema equivalent are lowered onto
the schema of the property they decorate, and so onto the query parameters a
DTO expands into. Only decorators imported from `class-validator` count,
directly, under an alias, through a namespace import or an in-repo barrel; a
same-named decorator of another package or of the project is ignored.

| Decorator | Schema |
| --- | --- |
| `@IsInt()` | `type: integer` instead of `number` |
| `@Min(n)`, `@Max(n)` | `minimum`, `maximum` |
| `@IsPositive()`, `@IsNegative()` | `exclusiveMinimum: 0`, `exclusiveMaximum: 0` |
| `@MinLength(n)`, `@MaxLength(n)`, `@Length(min, max?)` | `minLength`, `maxLength` |
| `@ArrayMinSize(n)`, `@ArrayMaxSize(n)`, `@ArrayNotEmpty()` | `minItems`, `maxItems`, `minItems: 1` |
| `@IsEmail()`, `@IsUrl()`, `@IsUUID()`, `@IsDateString()` | `format: email`, `uri`, `uuid`, `date-time` |
| `@Matches(/re/)`, `@Matches('re')` | `pattern` |

```ts
export class CreateShelterDto {
  @IsInt() @Min(0) @Max(MAX_CAPACITY) capacity: number;
  @IsOptional() @MaxLength(20, { each: true }) @ArrayMaxSize(10) tags?: string[];
}
```

```yaml
capacity: { type: integer, minimum: 0, maximum: 1000 }
tags: { type: array, items: { type: string, maxLength: 20 }, maxItems: 10 }
```

- Arguments are evaluated like other decorator arguments: literals, negative
  numbers, constants and enum members, across modules. A decorator whose
  argument cannot be evaluated (`@Max(Number.MAX_SAFE_INTEGER)`) is skipped.
- `{ each: true }` applies the constraint to the elements of an array or
  `Set`.
- A constraint only lands on a schema of its type: `@MaxLength()` on a string,
  `@Min()` on a number, `@ArrayMaxSize()` on an array. On a nullable property
  (`string | null`) it constrains the non-null branch. A type's own `format`
  wins: a `Date` stays `date-time`.
- A property whose type is a component (`$ref`: a class, an enum, a named type
  alias) is left unchanged, since the component is shared with every other use.
- A class read from a package's declaration files (`.d.ts`, see
  [`resolvePackages`](options/resolve-packages.md)) has no decorators left, so
  it carries no constraint.
- `@Matches()` with a flag that changes what the expression matches (`i`, `m`,
  `s`) has no `pattern` spelling, and is skipped.
- 3.0.3 output writes `exclusiveMinimum: 0` as `minimum: 0` +
  `exclusiveMinimum: true` (see [below](#openapi-31-and-30)).

`Partial`, `Pick`, `Omit` and the `@nestjs/*` helpers edit inherited
properties too. When a parent cannot be resolved (a class from an unresolved
package), it is kept as an `x-unresolved` `allOf` branch beside the edited
properties of the rest.

### Unresolved types

A type nestoa does not lower (mapped and conditional types, `typeof`,
`ReturnType`, …) or cannot find is emitted as `x-unresolved` with its source
text, never as an empty schema. See
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
- **Tuples.** `[number, number]` lowers to `prefixItems` + `items: false`,
  and `[string, ...number[]]` to `prefixItems` + `items: {type: number}`.
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
| `prefixItems: [A]` + `items: B` | `items: <A merged with B>` — **order is lost** |
| `exclusiveMinimum: n` (and maximum) | `minimum: n` + `exclusiveMinimum: true` |
| `examples: [a, …]` on a schema | `example: a` |

The nullable-union, standalone-null and tuple rows lose information, and that
is a limit of 3.0.3 rather than a shortcut. `nullable` only means something
beside a `type` keyword, and a union — which is how a nullable `$ref`, `allOf`
or `x-unresolved` is expressed — has none of its own; keeping the flag there
would be a no-op that linters reject (Redocly's `nullable-type-sibling`). So a
3.0.3 document cannot say "this `$ref` may be null", and it has no null-only
schema either. Likewise its arrays are homogeneous, so a tuple keeps its
maximum length — `maxItems`, matching `prefixItems`, which caps an array
without requiring every position — but not its per-position types.

An open tuple (`[A, ...B[]]`) merges its tail into the same `items` and gets
no `maxItems`. `minItems` is never synthesized, since `prefixItems` constrains
the positions an array has without requiring them.

The last two rows only concern schemas you supply verbatim: the analyser never
emits `exclusiveMinimum`, `exclusiveMaximum` or `examples` itself. An
`enum: [null]` loses its constraint, like a standalone `type: "null"`.

Instance data is never touched: the pass does not descend into `example`,
`examples`, `default`, `enum`/`const` values or `x-` extensions, so a
documented payload that happens to look like a schema survives unchanged. That
applies to those words as *keywords* only — a DTO field named `example` is
still a schema and is downgraded like any other.

# JSDoc tags

nestoa copies JSDoc comments into the document. A comment counts when it is the
nearest `/** … */` block before the declaration: above its `export` keyword and
its decorators. A plain `/* */` or `//` comment in between is skipped, as in
TypeScript.

A line opens a tag when it starts with `@` and a lowercase name followed by a
space, `{` or the end of the line, optionally after a `:`, `.` or `,`
(`@deprecated: use v2`). So `@nestjs/common`, `@Get()` or a bare
`@Injectable` at the start of a line are text, and so is anything inside a
```` ``` ```` code fence, up to its closing fence: an `@example` whose fence is
never closed runs to the end of the comment.

`@param` accepts the JSDoc forms `@param {Type} name`, `@param [name]`,
`@param [name=default]`, `@param name - text` and `@param name: text`.

Inline `{@link Target}`, `{@link Target text}` and `{@link Target|text}` (and
`@linkplain`) become their text, or `Target` when there is none; `@linkcode`
puts it in backticks. A URL target with a text becomes a markdown link
`[text](url)`. An empty or unclosed tag stays as written, and so does the outer
one of two nested tags.

Description lines are trimmed, even inside a code fence. Only an `@example`
keeps its indentation.

## On a handler

```ts
/**
 * Find a book
 *
 * Looks the book up by id, including soft-deleted ones.
 * @param id The book id
 * @deprecated Use `GET /v2/books/{id}`
 * @throws 404 The book does not exist
 */
@Get(':id')
findOne(@Param('id') id: string): Promise<Book> { … }
```

| Source | Output |
| --- | --- |
| First paragraph | operation `summary` |
| Following paragraphs | operation `description` |
| `@summary` / `@description` | set either one explicitly |
| `@param <name> <text>` | the parameter's `description` |
| `@deprecated [reason]` | `deprecated: true`, and `Deprecated: <reason>` ends the `description` |
| `@throws`, `@route-mapping` | see below |

## On a DTO, interface, enum or property

```ts
/** A book in the catalogue */
export class Book {
  /** ISBN-13, without dashes */
  isbn: string;

  /**
   * Internal ranking score
   * @internal
   */
  rank?: number;

  /** @example 2024-01-31 */
  publishedOn: string;
}
```

| Source | Output |
| --- | --- |
| Text (summary and description) | the schema's `description` |
| `@deprecated [reason]` | `deprecated: true`, and `Deprecated: <reason>` ends the `description` |
| `@example <value>` | `example`, see below |
| `@internal` / `@private` | the property is left out of the schema |

`@example` is read as JSON when it parses (`42`, `true`, `["a", "b"]`,
`{"shelf": "B2"}`, `"quoted"`), and a `'single-quoted'` value loses its
quotes. Anything else is a string. The property's type decides between the two:

- a value the property's `type` doesn't admit stays text, so
  `@example 9780306406157` on an `isbn: string` stays `"9780306406157"`;
- an example holding an integer too large for 64 bits stays text, rather than
  being rounded;
- a property without a `type` of its own (a `$ref` to an enum, alias or class,
  or a union) keeps a number, boolean or `null` as text, and takes an array or
  object as JSON.

A multi-line `@example` keeps its lines and their relative indentation.

When a property's schema is a `$ref`, the annotation is put on an `allOf`
wrapper, because OpenAPI 3.0 ignores siblings of `$ref` (see
[Output conventions](output.md)).

## `@throws`

Declares an error response for an error thrown somewhere nestoa cannot see,
such as a called service. The aliases are `@httpError` and `@exception`.

```ts
/**
 * Publish the book.
 * @throws 409 The book has already been published
 * @throws {422} Author disabled
 */
@Post(':id/publish')
```

The status can be bare or in braces, and the rest of the line is the
description. An entry without a numeric status (`@exception {ConflictException}`)
is skipped. These codes join the other
[error-response sources](options/error-responses.md#sources-in-priority-order).

## `@route-mapping`

One handler can serve several paths. When each path returns a different type,
the declared return type has to be a union, so nestoa can't tell which path
returns which member. `@route-mapping` maps a **trailing path suffix** to the
type that path returns:

```ts
/**
 * Find a pet's detail by kind
 * @route-mapping{
 * 'cats': Cat
 * 'dogs': Dog
 * 'birds': Bird
 * }
 */
@Get([':id/cats', ':id/dogs', ':id/birds'])
findDetail(): Promise<Cat | Dog | Bird> { … }
```

- A key matches when the path equals it or ends with `/<key>`. The last segment
  is usually enough. Use a longer suffix (`cats/latest`) when it isn't.
- Entries are separated by newlines or commas, and keys may be quoted.
- The type name is resolved from the controller file's imports and emitted as a
  `$ref`.
- A path with no matching entry keeps the declared return type.

For a reshaping done by an interceptor, use the
[`interceptors`](options/interceptors.md) option instead.

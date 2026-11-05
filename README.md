# nestoa

**Generate an OpenAPI document from a NestJS project's source code.**

nestoa reads your controllers, DTOs and JSDoc comments and writes
`openapi.json` (or `.yaml`). It never compiles or starts your app: it only
parses the TypeScript. The same code always gives the same document, so it fits
in CI next to your linter and formatter.

```ts
/** A book in the catalogue */
export class Book {
  /** ISBN-13, without dashes */
  isbn: string;
  title: string;
  publishedOn?: Date;
}

@Controller('books')
export class BooksController {
  /** Find a book */
  @Get(':id')
  findOne(@Param('id') id: string): Promise<Book> {
    return this.books.findOne(id);
  }
}
```

```yaml
paths:
  /books/{id}:
    get:
      summary: Find a book
      operationId: BooksController_findOne
      tags: [Books]
      parameters:
        - { name: id, in: path, required: true, schema: { type: string } }
      responses:
        '200':
          description: OK
          content:
            application/json:
              schema: { $ref: '#/components/schemas/Book' }
        '401':
          description: Authentication required
          content:
            application/json:
              schema: { $ref: '#/components/schemas/Error' }
      security:
        - bearerAuth: []
components:
  schemas:
    Book:
      type: object
      description: A book in the catalogue
      properties:
        isbn: { type: string, description: 'ISBN-13, without dashes' }
        title: { type: string }
        publishedOn: { type: string, format: date-time }
      required: [isbn, title]
```

The shared `Error` component referenced by `401` and the `bearerAuth` security
scheme are omitted here.

With [`asyncapi`](docs/options/asyncapi.md), it also writes an AsyncAPI 3.0
document of the service's message events: the `@EventPattern` and
`@MessagePattern` handlers it receives, and the publish calls it sends.

## Install

Install it as a dev dependency from npm:

```sh
npm install --save-dev nestoa
```

The right prebuilt binary for your platform is picked automatically: Linux
(x64 and arm64, glibc and musl), macOS (x64 and arm64) and Windows (x64). Each
release is also published on the GitHub Packages npm registry as
`@algoan/nestoa`, and its binaries are attached to the
[GitHub release](https://github.com/algoan/nestoa/releases).

## Use

```sh
npx nestoa                # reads ./src, writes ./openapi.json
npx nestoa -o openapi.yaml
npx nestoa --strict       # in CI: fail if anything could not be documented
```

To customise it, add a `nestoa.config.json` at the project root:

```json
{
  "include": ["src"],
  "output": "openapi.yaml",
  "info": { "title": "Bookstore API", "version": "1.0.0" }
}
```

## Documentation

- [All configuration options](docs/options/README.md)
- [CLI](docs/cli.md)
- [JSDoc tags](docs/jsdoc.md)
- [Output conventions](docs/output.md)
- [Architecture](docs/architecture.md) and [development](docs/development.md)

## License

MIT

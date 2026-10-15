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
  findOne(@Param('id') id: string): Promise<Book> { … }
}
```

```yaml
paths:
  /books/{id}:
    get:
      summary: Find a book
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
    Error: { … }
```

## Install

nestoa is published on the GitHub Packages npm registry. Point the `@algoan`
scope at it once, in the project's `.npmrc`:

```ini
@algoan:registry=https://npm.pkg.github.com
```

Then install it as a dev dependency:

```sh
npm install --save-dev @algoan/nestoa
```

The right prebuilt binary for your platform is picked automatically.

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

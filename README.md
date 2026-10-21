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

The shared `Error` component referenced by `401` is omitted here.

## Install

nestoa is published on the GitHub Packages npm registry, which requires a token
even to install. Create a personal access token (classic) with the
`read:packages` scope, export it as `GITHUB_TOKEN`, and point the `@algoan`
scope at the registry in the project's `.npmrc`:

```ini
@algoan:registry=https://npm.pkg.github.com
//npm.pkg.github.com/:_authToken=${GITHUB_TOKEN}
```

The `.npmrc` only references the variable, so it is safe to commit. Never commit
the token itself. In GitHub Actions, the workflow's own `GITHUB_TOKEN` works
when the job has `packages: read` permission.

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

# `detectGlobalPrefix`

Read the global prefix from the NestJS bootstrap file.

- **Type:** boolean
- **Default:** `true`

```json
{ "detectGlobalPrefix": false }
```

## Behaviour

When [`basePath`](base-path.md) is unset, nestoa reads the bootstrap file
(`main.ts` first, among the [included](include.md) files only) and looks for
`app.setGlobalPrefix(...)`:

```ts
const app = await NestFactory.create(AppModule);
app.setGlobalPrefix('api', {
  exclude: ['health', { path: 'metrics', method: RequestMethod.GET }],
});
```

- **The prefix** is resolved when it is a string literal, a local `const` bound
  to a string, or a [`node-config-ts`](https://www.npmjs.com/package/node-config-ts)
  member such as `config.globalPrefix` (looked up in `config/default.json`).
- If the prefix can't be evaluated, nestoa warns and leaves **every** route
  unprefixed. It does not look for another `setGlobalPrefix` call.
- **`exclude`** keeps the listed routes unprefixed. Entries can be a path
  (`'health'`), a `{ path, method }` object, or a path ending in a `(.*)` / `*`
  wildcard. If `exclude` can't be evaluated, nestoa warns and every route keeps
  the prefix.

Set it to `false` to keep routes unprefixed when no `basePath` is given.

## Versioning

`app.enableVersioning(...)` is always read, whatever this option says, as long as
it uses URI versioning. Each handler gets the versions from its own
`@Version(...)`, else from its `@Controller({ version })`, else from
`defaultVersion`. Each version adds a `<prefix><N>` segment after the global
prefix. The segment prefix defaults to `v`. `VERSION_NEUTRAL` adds no segment.

```ts
app.enableVersioning({ type: VersioningType.URI, defaultVersion: '1' });

@Controller('books')
export class BooksController {
  @Get() list() {}                 // GET /api/v1/books
  @Version('2') @Get() listV2() {} // GET /api/v2/books
}
```

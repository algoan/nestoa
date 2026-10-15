# nestoa documentation

nestoa reads the TypeScript source of a NestJS project and writes an OpenAPI
document for it, without compiling or running the app.

## Guides

| Page | What it covers |
| --- | --- |
| [CLI](cli.md) | Command-line flags, config file discovery, `extends`, `--strict` |
| [Configuration options](options/README.md) | One page per `nestoa.config.*` key |
| [JSDoc tags](jsdoc.md) | How comments become `summary`, `description`, per-path responses and error codes |
| [Output conventions](output.md) | The shape of the emitted document, and the OpenAPI 3.0.3 downgrade |
| [Architecture](architecture.md) | The analysis pipeline, module by module |
| [Development](development.md) | Building, testing, releasing, and where to make a change |

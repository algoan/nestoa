# `asyncapi`

Write an [AsyncAPI 3.0](https://www.asyncapi.com/docs/reference/specification/v3.0.0) document of
the service's message events, next to the OpenAPI document, from the same analysis.

- **Type:** object
- **Default:** none (no AsyncAPI document)

```json
{
  "asyncapi": {
    "output": "asyncapi.yaml",
    "payloadWrappers": ["Envelope", "EventPayload"],
    "publishers": [
      { "method": "publish", "receiverTypes": ["EventsClient"], "payloadProperty": "data", "typeArgument": 0 }
    ]
  }
}
```

| Key | Default | Effect |
| --- | --- | --- |
| `output` | `asyncapi.json` | Output file, relative to the project. JSON or YAML from its extension. `--asyncapi-output` overrides it. The OpenAPI document's file is an error. |
| `info` | the top-level [`info`](info.md) | The document's `info`. |
| `defaultContentType` | `application/json` | The document's `defaultContentType`. |
| `payloadWrappers` | `[]` | Generic envelopes a payload type is unwrapped from (see below). |
| `publishers` | `ClientProxy` `emit` and `send` | The calls that send a message (see below). |

An empty `"asyncapi": {}` turns the document on with every default.

## Receiving: `@EventPattern` and `@MessagePattern`

Every method of a `@Controller` class (inherited ones included) decorated with `@EventPattern(…)` or
`@MessagePattern(…)` is a `receive` operation on the channel its pattern names. The pattern is
evaluated like a route path: a string literal, a constant, a string enum member (`EventName.BOOK_PUBLISHED`,
from a package's `.d.ts` too with [`resolvePackages`](resolve-packages.md)), or an array of those. An
object pattern (`{ cmd: 'sum' }`) is the address NestJS matches it by, its keys sorted:
`{"cmd":"sum"}`. A pattern nestoa cannot evaluate is a warning, and the handler is left out.

The message payload is the type of the `@Payload()` parameter. Several `@Payload('key')` parameters
form an object of those properties, required unless the parameter is optional (`?`, a default, or a
type admitting `undefined`). Without `@Payload`, it is the first undecorated parameter, as
NestJS passes it.

```ts
@Controller()
export class BookListenerController {
  /** Index a published book. */
  @EventPattern(EventName.BOOK_PUBLISHED)
  onPublished(@Payload() message: Envelope<EventPayload<BookDto>>) {}
}
```

### `payloadWrappers`

A transport often hands the handler an envelope around the payload. Each type listed is replaced by
its first type argument, at the root of the payload only. With `["Envelope", "EventPayload"]`, the
handler above receives `BookDto`; with `["Envelope"]`, `EventPayload<BookDto>`, the envelope's own
fields included. List the wrappers so that both sides of a channel document the same type: a
publisher's payload is what it passes, usually without the outer envelope.

## Sending: `publishers`

Each entry describes a method call that publishes a message, found in the members of every class
(services, controllers, …): methods, constructors, properties, private ones and static blocks,
nested callbacks included. Every matching call is a `send` operation.

| Key | Default | Effect |
| --- | --- | --- |
| `method` | required | The method name: `this.client.<method>(…)`. |
| `receiverTypes` | `[]` | The types the receiver, a `this.<field>` (a constructor parameter property too), must be declared with. Any receiver when empty. |
| `channel` | `0` | The argument naming the channel, evaluated like a pattern. |
| `payload` | `1` | The argument holding the payload. |
| `payloadProperty` | none | The property of an object-literal payload argument that holds the payload: `data` in `publish(topic, { data, meta })`. |
| `typeArgument` | none | The call's type argument that gives the payload type, when written: `publish<BookDto>(…)`. |

The default is NestJS's `ClientProxy.emit(pattern, data)` and `send(pattern, data)`, on a field of a
`ClientProxy` type (`ClientKafka`, `ClientRMQ`, … included), with the payload from the second type
argument. Setting `publishers` replaces the list; repeat those entries to keep them.

A channel that cannot be evaluated is a warning, and the call is left out. So is one reading a
parameter or variable of the member, which shadows a module constant of the same name. An object
pattern may also be a constant (`const SUM = { cmd: 'sum' }`), for handlers too. With
`payloadProperty`, an object with a spread is `x-unresolved`: the spread may supply the property.

### Inferred payloads

Without the type argument, the payload's type is inferred from the expression, as far as declared
types reach:

- literals and templates, `x as T`, `<T>x`, `new T()`;
- a parameter, a variable or a field (`this.x`) declared with a type, or a `const` initialised with
  any of these (a `let` or `var` may be reassigned, so its initializer is not followed);
- a property of one of those (`book.id`), and an enum member, as its enum;
- a parameter property, an inherited field (a receiver of `receiverTypes` too) or method;
- a call to a function, a `const` arrow function or a method of the class declared with a return
  type (`Promise` unwrapped), when every overload declares the same one;
- object and array literals of the above, `await`, `a ? b : c`. A later property or spread replaces
  an earlier one, as in JavaScript (an optional spread property may also leave the earlier one), a
  property whose value may be `undefined` (`book.subtitle?`, `x: T | undefined`) is optional, a lone
  `{ ...x }` copies an object `x` (or is `{}` when `x` may be `undefined`), a call is `undefined`
  when its declared return type admits it, and an `undefined` array element is `null`. An object with a key
  that cannot be evaluated, a method or accessor, or with a spread of a dictionary (`Record<string, T>`), is unresolved: it
  may override any property.

`this` is the instance: static fields and methods are not read. A name the member declares (a
parameter, a destructured variable, a local function or class) shadows a module one of that name.
Optional calls (`this.client?.emit(…)`) publish too; calls in a nested class do not, since its
`this` is another object. A spread argument at or before the payload leaves it unresolved.

`payloadWrappers` only apply to the whole payload: an envelope nested in it is documented as is.

Anything else (a method call on a value, an untyped callback parameter, …) is
[`x-unresolved`](../cli.md#diagnostics-and---strict) with its source text, and `--strict` reports
it. A payload of `undefined` gives a message without payload.

## The document

- One channel per address. Its id is the address, with the characters AsyncAPI does not allow in an
  id replaced by `_`.
- One operation per handler, action and channel. Its id is `<Class>_<method>`, numbered when the
  same method sends to several channels, so two operations on one channel never overwrite each other.
  Its `summary` and `description` come from the method's JSDoc, and its tag from the class name.
- Each distinct payload of a channel is one message, named after the component it references, else
  after the channel. The operations reference it.
- Payloads are lowered like OpenAPI schemas, into `components.schemas` (only those referenced are
  kept), then written in JSON Schema draft-07, the base of AsyncAPI's schema format: a tuple's
  `prefixItems` becomes an `items` array and `additionalItems`.

A `@MessagePattern` reply is not documented.

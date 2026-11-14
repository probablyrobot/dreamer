# API schema

Every typed operation that the GUI, the `dream` CLI, scripts, and extensions
use is defined once, in `crates/api/schema/api.yaml`. The following are
generated from it and checked in:

| File | What it is |
| --- | --- |
| `crates/api/src/generated.rs` | Rust SDK types. Each record encodes as Protobuf (prost) for the native wire and as JSON (serde) for JSON-RPC. Also includes the method table, typed `Request` and `Response`, the `Service` trait, and `dispatch` |
| `crates/api/generated/openrpc.json` | The JSON-RPC 2.0 surface as an OpenRPC 1.3 document |
| `crates/api/generated/dreamer.luau` | Luau module stubs for sandboxed scripts |
| `crates/api/generated/dreamer.proto` | The records as a `.proto` file, for clients in other languages |
| `crates/api/schema/compat.lock` | The accepted contract, used by the compatibility check |

## Changing the API

1. Edit `crates/api/schema/api.yaml`.
2. Run `cargo xtask api`. It validates the schema, checks it against
   `compat.lock`, and rewrites the generated files and the lock.
3. Commit the schema and the generated files together.

`cargo xtask api --check` runs in the pre-commit gate and in Linux CI. It
fails if a generated file is stale or if the change breaks compatibility.

## Evolution rules

Within one major version, peers of different minor versions must
interoperate. The check enforces this:

- **Allowed:** adding a message, enum, enum value, field, oneof variant, or
  method. An addition requires a minor version bump.
- **Refused:** removing or renumbering anything, or changing a field's name,
  type, or cardinality, a variant's type, a method's parameter or result
  type, or whether a method is a mutation.
- **Never reuse a field number.** List retired numbers under `reserved:`.
  The generator refuses a field that uses a reserved number.
- **Breaking changes need a major version bump.** A new major version is a
  new protocol, and the handshake refuses mismatched majors before any
  payload is decoded.

Enums need a value numbered 0 (`unspecified`), which is the default when a
field is absent.

## JSON conventions

- A JSON-RPC call's `params` is the method's parameter record as a JSON
  object. Its `result` is the result record.
- Absent fields take their defaults. Unknown fields are ignored, so an older
  peer can read a newer peer's records.
- Enums are written as their snake_case names (`"outcome_unknown"`). Numbers
  are accepted too, and a value this build does not know stays a number.
  This differs from the standard Protobuf JSON mapping.
- `bytes` fields are base64 strings (RFC 4648, padded).
- 64-bit integers are JSON numbers. Readers in JavaScript lose precision
  above 2^53.
- Errors are JSON-RPC 2.0 error objects (`RpcError`). The codes are in
  `api::code`: the standard JSON-RPC codes, plus `-32001` not found,
  `-32002` precondition failed, `-32003` idempotency conflict, `-32004`
  unavailable, `-32005` forbidden by the connection's grant, and `-32006`
  limited (a rate, or a message chain's depth).

## Luau

`api::LUAU_MODULE` evaluates to a function. The host calls it with its own
`call(method, params)` function and gets back a frozen `dreamer` table with
one function per method, for example `dreamer.actions.submit{...}`. Records
are plain tables with the same field names as JSON.

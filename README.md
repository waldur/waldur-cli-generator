# waldur-cli-generator

Generates [waldur-cli](https://code.opennodecloud.com/waldur/waldur-cli)'s command surface by
parsing Waldur's OpenAPI schema directly -- the schema is the single source of truth for each
operation's path, params, and request/response shape.
[rs-client](https://code.opennodecloud.com/waldur/rs-client) is still used, purely as a source
of typed request-body structs for validating `--request` JSON locally; waldur-cli itself makes
raw HTTP calls (see `waldur-cli`'s `src/http.rs`/`src/pagination.rs`) rather than calling
rs-client's generated methods, so a schema/response mismatch on a field nobody reads can never
break a command the way it used to.

Mirrors the pattern already used by
[ansible-waldur-generator](https://code.opennodecloud.com/waldur/ansible-waldur-generator) →
`ansible-waldur-module-next` and
[terraform-provider-waldur-generator](https://code.opennodecloud.com/waldur/terraform-provider-waldur-generator)
→ `terraform-provider-waldur`: a generator repo that produces code and pushes it into a
separate target repo.

## What's covered

[`commands.toml`](commands.toml) is the single source of truth for what's in scope --
deliberately a curated subset (~60 commands: `list`/`get`/`create`/`update`/`delete` across
16 OpenStack + team-management resources), not a mechanical 1:1 wrap of Waldur's ~451
operations. See the comment at the top of that file for what's excluded and why (mainly:
OpenStack tenant/instance/volume creation goes through Waldur's marketplace ordering flow).

To add a resource or verb: add a `commands.*` entry to `commands.toml` referencing the exact
`operationId` from the OpenAPI schema, then regenerate. `list`'s query parameters don't get a
dedicated flag each (some resources have 20+) -- the generator classifies each one's real type
(string/bool/i64) and emits a `FILTER_SPEC` const from it, which the generated command's single
`--filter KEY=VALUE` flag (`waldur-cli`'s hand-written `src/filter.rs`) validates against at
runtime. A query parameter type this generator doesn't recognize makes generation fail loudly
for that operation rather than silently emit broken code -- extend `classify_param()` in
`src/schema.rs` if you hit one you need to support.

For `create`/`update`, the generator also walks the operation's request-body schema (resolving
`$ref`/`allOf`/`oneOf`, enums, and nested objects) into a fillable JSON template, embedded in
the generated command as a `const` for its `--generate-skeleton` flag -- required fields get a
typed placeholder, optional ones default to `null` so the raw template is valid input.

A resource can also declare a `[group.resource.order]` block with an `offering_type` (e.g.
`OpenStack.Instance`). That resource gets `provision`/`terminate` subcommands for Waldur's
async marketplace-order flow instead of a direct REST create/delete. `provision`'s
`--generate-skeleton` is the `OrderCreateRequest` envelope with the typed
`{OfferingType}CreateOrderAttributes` schema spliced into its free-form `attributes` slot;
the runtime polling lives in waldur-cli's hand-written `src/order.rs`.

## Regenerating locally

```bash
cargo run -- waldur-openapi-schema.yaml ../waldur-cli
```

Both arguments are optional: the schema path defaults to `waldur-openapi-schema.yaml` in the
current directory (matching CI's downloaded artifact name), and the target dir defaults to a
sibling `../waldur-cli`. This overwrites `waldur-cli`'s `src/commands/` and `src/cli.rs`
wholesale -- see that repo's README for which files are hand-written and permanent instead.

## License

MIT, see [LICENSE](LICENSE).

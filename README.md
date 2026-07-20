# waldur-cli-generator

Generates [waldur-cli](https://code.opennodecloud.com/waldur/waldur-cli)'s command surface
by parsing [rs-client](https://code.opennodecloud.com/waldur/rs-client)'s generated
`HttpClient` methods with [`syn`](https://docs.rs/syn), rather than re-parsing the OpenAPI
schema independently -- rs-client stays the single source of truth for what each operation's
real Rust signature looks like.

Mirrors the pattern already used by
[ansible-waldur-generator](https://code.opennodecloud.com/waldur/ansible-waldur-generator) →
`ansible-waldur-module-next` and
[terraform-provider-waldur-generator](https://code.opennodecloud.com/waldur/terraform-provider-waldur-generator)
→ `terraform-provider-waldur`: a generator repo that produces code and pushes it into a
separate target repo.

## What's covered

[`commands.toml`](commands.toml) is the single source of truth for what's in scope --
deliberately a curated subset (~60 commands: `list`/`get`/`create`/`update`/`delete` across
16 OpenStack + team-management resources), not a mechanical 1:1 wrap of rs-client's ~451
operations. See the comment at the top of that file for what's excluded and why (mainly:
OpenStack tenant/instance/volume creation goes through Waldur's marketplace ordering flow,
outside rs-client's own generated surface).

To add a resource or verb: add a `commands.*` entry to `commands.toml` referencing the exact
rs-client method name, then regenerate. The generator classifies each of that method's real
parameters (string/bool/i64/JSON-body-shaped, required or `Option`-wrapped) into a CLI flag
automatically; anything it doesn't recognize (e.g. a required `Vec<SomeEnum>` filter) makes
generation fail loudly for that method rather than silently emit broken code -- extend
`classify_type()` in `src/extract.rs` if you hit one you need to support.

## Regenerating locally

```bash
cargo run -- ../rs-client ../waldur-cli
```

Both paths default to sibling directories of this repo if omitted. This overwrites
`waldur-cli`'s `src/commands/` and `src/cli.rs` wholesale -- see that repo's README for which
files are hand-written and permanent instead.

## License

MIT, see [LICENSE](LICENSE).

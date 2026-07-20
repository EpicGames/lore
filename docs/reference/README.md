# Reference

Authoritative, lookup-oriented descriptions of Lore's commands, file formats, and protocols.

## What this folder is

Reference is the technical description of the machinery and how to operate it. Austere, neutral, factual. No instruction or explanation embedded; if you want to learn, the tutorials folder is the place, and if you want the why, see explanation.

## Reference pages

- [Lore CLI command reference](lore-cli-commands.md) — every `lore` command, subcommand, argument, and flag, generated from `lore --markdown-help`.
- [Lore CLI configuration reference](lore-cli-config.md) — every field in the per-repository `config.toml`, the user-level global `config.toml`, and the user-level `cli.toml`, with each field's type, default, and on-disk location.
- [Lore Server configuration reference](lore-server-config.md) — every `loreserver` CLI flag, config-file layer, and settings field, including the AWS, DynamoDB, Consul, and hook plugin backends.

## Suggested starting points

- **Writing a new Reference page?** Start at the [doc-standards walkthrough](../developing/doc-standards/writing-a-doc.md).
- [Reference template](reference-template.md). Copy this when starting a new Reference.

See [docs/README.md](../README.md) for the full docs structure.

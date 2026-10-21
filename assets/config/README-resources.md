# Predefined configuration resources

The following resources are retained from the .NET source at commit
`5f27a8a92625ee31d8bec094e5c3e09550193781`:

- `predefined_config.system_prompts.yaml`
- `predefined_config.agent_tasks.yaml`
- `predefined_config.profiles.yaml`
- `predefined_config.runtime.yaml`

The active native template resource is
`predefined_config.system_prompts.minijinja.yaml`. It uses MiniJinja for ten
explicit template descriptors; the remaining descriptors use the scalar engine.
There is no general Scriban-to-Jinja translator or automatic rewriting of user
configuration.

These are first-party application resources from the pinned source. No per-file
license header or root source-repository LICENSE was found for these resources;
no license grant is inferred from third-party skill licenses. See
[third-party notices](../../THIRD_PARTY_NOTICES.md) for retained dependency notices.

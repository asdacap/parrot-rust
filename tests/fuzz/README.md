# Bounded fuzz and scheduler regressions

This suite combines fixed regressions with reproducible seeded campaigns. It is
finite, not an exhaustive fuzzer or evidence of application, security, or release
acceptance.

## Run the campaign

From the repository root, run the dedicated test target inside the Nix development
shell:

```sh
nix develop --command cargo test --locked -p parrot-fuzz-regressions --test bounded_fuzz
```

Use `-- --nocapture` to include each domain's seed and completed case count in the
output. A failing generated case includes its domain, seed, and zero-based case
index. The test target includes the scheduler regressions from
`../stress/scheduler.rs` alongside the format and path cases.

## Seeds and budgets

`../fixtures/regressions/T77/campaign.json` is the committed source of seeds and
case counts. `generator.rs` implements SplitMix64 with explicit wrapping
arithmetic. Each domain starts its own generator for each seed. Fixed malformed,
duplicate, and boundary regressions run before generated cases.

The campaign exercises YAML, task JSON, persisted and operational queue JSONL,
history JSON, protobuf, SSE, Responses WebSocket messages, terminal bytes, and
lexical paths. Generated input limits bound the test workload. They are not
application size or depth limits.

Scheduler regressions use controlled execution gates and bounded failure
deadlines. Seeded launch order does not prescribe the operating system's mutex
acquisition order. Assertions check accepted items and legal outcomes rather
than promise global ordering.

## Scope and evidence

The tests call the application's parsing, serialization, queue, stream, terminal,
and scheduling code. Controlled persistence and execution backends isolate
scheduler behavior. Those checks do not establish provider behavior, SQLite
durability, filesystem authorization, or platform sandbox safety.

Keep each discovered counterexample as a deterministic regression with its input,
seed, case index, and observed failure. Do not weaken assertions, silently retry
failures into passes, or treat a compilation or setup correction as behavioral
verification. Passing results must come from executing the current test target.

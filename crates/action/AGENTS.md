# action/ agent rules

The crate map lives in [README.md](README.md).

## Boundaries

- `browser/`, `retrieval/`, and `tasks/` have no workspace
  dependencies. They define seams and vocabulary.
- `action-tasks` is vocabulary-tier. The composition root maps it
  onto the child runtime. Execution layers implement the seam.
- `jobs` and `workflow` may depend on `wavecode-tools` for their
  model tools. Those tools do not import drivers, actors, or
  sessions.
- `workflow` may name `runtime-scheduler` and `action-tasks`. `jobs`
  may name `runtime-child`. They may also name
  `infrastructure-base` and `wavecode-protocol`.
- No action crate names state, operations, or a frontend.
- `action-browser` and `action-retrieval` are unwired reserved seeds.
  Do not cite them as product features. Wiring one updates
  `docs/architecture.md` in the same change.

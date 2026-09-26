## Git conventions

###### Commit messages (Conventional Commits)

Format: `<type>(<scope>): <subject>` — `<scope>` is optional; omit the parentheses when there is none.

- `type`: standard types only — `feat` / `fix` / `docs` / `refactor` / `chore` / `test` / `perf`.
- `subject`: imperative mood, ≤ 50 characters, describes the behavior directly — no internal phase numbers (e.g. P0–P9). One commit does one thing.

Examples: `feat: initialize project repository`, `fix: long-session context overflow`, `feat(parser): support nested generics`, `fix(api): reject expired tokens`

### Branch naming

```
<type>/<kebab-case-description>
```

`type` as above; the description is kebab-case.

Examples: `feat/context-compaction`, `fix/memory-dedup`

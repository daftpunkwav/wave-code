# Git conventions

## Commit messages (Conventional Commits)

```
<type>: <subject>
```

`type` is one of the standard types: `feat` / `fix` / `docs` / `refactor` / `chore` / `test` / `perf`. The subject is written in the imperative mood, kept to 50 characters or fewer, and describes the behavior directly — no internal phase numbers (e.g. P0–P9). One commit does one thing.

Examples: `feat: initialize project repository`, `fix: fix context overflow in long sessions`

## Branch naming

```
<type>/<kebab-case-description>
```

`type` as above; the description is kebab-case.

Examples: `feat/context-compaction`, `fix/memory-dedup`

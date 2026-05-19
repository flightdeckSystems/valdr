# Harness architecture

The canonical doc lives at `../port-harness/docs/ARCHITECTURE.md`.

Quick summary for context here:

- **Two layers**: runtime (how agents are invoked — Claude Code's
  Agent tool interactively, or `claude -p` headless) vs chassis
  (project-agnostic hooks + templates + lib at `../port-harness/`).
- **This project consumes the chassis** via `.claude/hooks/` wrappers
  + `.claude/agents/` rendered templates + `harness/` data.
- **Atomicity**: one agent invocation = one git commit. Bisect-able.
- **Open refactor work** lives in `../port-harness/docs/V2_PRIORITIES.md`
  and the "What's NOT clean" section of `ARCHITECTURE.md`.

See also:
- `HARNESS_MODE_A_VS_B.md` — the task-mode gap (translation vs
  greenfield infra) we identified during Session 1 planning.
- `../port-harness/docs/V2_PRIORITIES.md` — chassis-side v2 roadmap.

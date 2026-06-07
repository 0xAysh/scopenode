## Agent skills

Treat the current code and tests as the source of truth. Before changing
architecture or behavior, read `CONTEXT.md` and the relevant flow in
`ONBOARDING.md`.

### Issue tracker

Issues live in GitHub Issues (`gh` CLI). See `docs/agents/issue-tracker.md`.

### Triage labels

Default five-label vocabulary (needs-triage, needs-info, ready-for-agent, ready-for-human, wontfix). See `docs/agents/triage-labels.md`.

### Domain docs

Living product and architecture documentation:

- `README.md` — operator-facing behavior and supported surface
- `ONBOARDING.md` — current architecture and recommended learning path
- `CONTEXT.md` — canonical domain vocabulary
- `VISION.md` — current product boundary and future direction

Files under `docs/brainstorms/`, `docs/plans/`, `docs/qa/`, and
`docs/superpowers/` are dated records. Their status header determines whether
they describe completed, superseded, or planned work.

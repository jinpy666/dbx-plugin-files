# Agent workflow

This repository is the Files Studio plugin. Follow `.github/agent-flow.yml` as the machine-readable task contract.

## Handoff rules

- Work in a dedicated branch/worktree. Never share a checkout with another agent.
- Read the contract before editing. Keep changes inside the task's `allowed_paths`.
- Do not change versions, lockfiles, release metadata, or unrelated generated files unless the task explicitly owns them.
- Do not use real credentials, production endpoints, or a host checkout. Container tests must use throwaway credentials.
- Finish with the commands in `validation.local` and report changed files, test results, risks, and follow-up work in the PR.
- A task is not complete until CI is green and the handoff checklist is filled in.

## Integration rules

The integrator owns shared files, version bumps, generated `ui/` output, package artifacts, and final cross-target validation. Agents must not merge their own PRs.


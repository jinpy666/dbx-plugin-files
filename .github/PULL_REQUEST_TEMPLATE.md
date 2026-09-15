## Agent handoff

- Task ID:
- Stage: `contract` / `implementation` / `review` / `integration`
- Base SHA:
- Allowed paths respected: yes / no
- Depends on PR:

## Verification

- [ ] `python3 scripts/validate_repo.py`
- [ ] `node scripts/connection-forms/verify.mjs files`
- [ ] frontend typecheck/test/build
- [ ] `cargo test --locked --manifest-path backend/Cargo.toml`
- [ ] Relevant container smoke

## Review notes

- Changed files:
- Risks or known limitations:
- Follow-up task:
- No credentials or production endpoints used: yes


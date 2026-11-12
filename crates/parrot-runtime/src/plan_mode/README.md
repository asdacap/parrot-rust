# Plan mode

Plan mode uses the effective configured profiles and the selected agent's scratch
location. Plan artifacts do not grant general write authority or replace the
session's security policy.

## Plan artifacts and approval

Planning creates correlated Markdown and task artifacts beneath the selected
agent's plan directory. Plan mode opens each artifact with exclusive creation and
private permissions. Preparing a plan again does not truncate existing artifacts.
Failure cleanup removes only artifacts created by that operation.

Plan output must pass strict task parsing. Invalid output produces repair
diagnostics. Valid output can produce an approval dialog, but the planning workflow
does not dispatch implementation work.

## Validation limits

Host, tool, and production security integration still require separate validation.
The previously recorded captured-file approval-widening failure remains unresolved.
Policy substitutes, parser checks, and source-only test results do not establish
production safety or full application acceptance.

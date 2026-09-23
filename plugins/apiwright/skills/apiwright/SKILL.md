---
name: apiwright
description: Inspect, edit, validate, and run saved ApiWright request-v1 API test projects through the ApiWright MCP server. Use for projects with project.json, *.request.json files, assertion or hook sidecars, inherited environments, OpenAPI references, mocks, and API test execution.
---

# ApiWright

Work on the project files ApiWright actually persists. The MCP shares the IDE
and CLI's Rust core; it does not automate GUI widgets or see unsaved editor
buffers.

ApiWright request-v1 is a file-based model, not a conventional collection CRUD
API. Begin at the directory containing `project.json`. Requests live at the
project-relative paths returned by `inspect_project`; each `*.request.json`
may have assertion and hook sidecars, plus environment, OpenAPI, and Jira
selections inherited from parent folders. Use the MCP tools to update those
canonical files rather than inventing a second test store.

## Workflow

1. Find the absolute project root containing `project.json`.
2. Call `inspect_project` before changing anything. Report relevant broken
   references and confirm the target request exists.
3. If the request or project auth uses project code, call
   `read_project_code` for every referenced project-owned executable before
   asking the user to trust it.
4. Call `read_request` before editing. Treat `document`, `assertions`, and
   `hooks` as one logical request, and retain the returned `revision`.
   Edit `document`, not the merged `effectiveDocument`.
5. Use `add_assertion` to create one manual API test or `remove_assertion` with
   its zero-based index to remove one. Both preserve unrelated tests and
   revision-check the request. Use `write_request` when the request document
   or several sidecars need coordinated edits.
6. Save or discard any open IDE buffer before an AI write. Call the chosen
   write tool with the retained revision; for a new request, use
   `write_request`, set `create` to `true`, and use `expectedRevision: "new"`.
   If the revision changed, read the request again instead of overwriting
   another edit.
7. Read the request again after a write so subsequent operations use the new
   revision. Call `validate_request`; validation sends no HTTP.
8. Call `run_request` only when the user authorized execution. Pass the
   validated revision as `expectedRevision`. Prefer its default mock mode;
   set `realHttp: true` only after the user authorizes the real request because
   external HTTP can mutate systems.

To create or change an environment, sequence, or asset, use
`read_project_file` followed by `write_project_file` with the returned
revision; use `expectedRevision: "new"` for creation. Sequence request paths
must come from `inspect_project`. Before deleting resources, read their current
revision and inspect any reference warnings. `delete_request` removes a
request and its sidecars; `delete_project_file` removes environments,
sequences, or assets with reference protection.

After a write, tell the user to reload the request in the IDE. The MCP edits
saved files and cannot merge an unsaved IDE tab.

## Safety

- Keep request paths project-relative and ending in `.request.json`.
- Never request `.env.local` or `*.secrets.json`, and never echo cookies,
  authorization headers, or secret values from a request document. Let
  ApiWright resolve dedicated secret files internally.
- Leave `allowProjectCode` false until the request and any project-auth
  JavaScript were inspected and the user explicitly trusts them.
- Distinguish validation errors, assertion failures, execution errors, and MCP
  transport errors. Do not report an erroring request as a passed test.
- Use the current request-v1 model unless the user explicitly asks about a
  legacy `forge.json` workspace.

## Tool boundaries

- `inspect_project`: inventory and reference diagnostics; no secret values.
- `read_project_code`: bounded reads for indexed JavaScript/TypeScript that
  must be reviewed before trust.
- `read_project_file` / `write_project_file` / `delete_project_file`:
  revision-checked access to environment, sequence, and asset resources.
- `read_request`: saved main document, sidecars, inherited selections, and
  concurrency revision.
- `write_request`: guarded create/update using IDE-compatible sidecars.
- `add_assertion` / `remove_assertion`: focused, revision-checked manual test edits.
- `delete_request`: guarded request and sidecar deletion with reference checks.
- `validate_request`: canonical resolution without HTTP.
- `run_request` / `run_sequence`: mock or explicitly authorized real execution;
  returned results omit raw response bodies.

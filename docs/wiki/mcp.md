# MCP and AI automation

ApiWright includes a local Model Context Protocol (MCP) server so an AI client
can work with the same saved request-v1 projects as the IDE and CLI. The server
is an inbound adapter over `forge-core`; it does not automate GUI widgets,
invent a second request model, or upload a project to an ApiWright service.

The local stdio server and Codex plugin are part of the source-available core.
Premium features remain separately entitled; the current MCP does not expose
Jira, license, or coverage-report tools. See
[Licensing and billing](licensing.md) for the product boundary.

## Start the server

Run from Cargo, or configure an MCP client with the absolute path of a built
binary:

```sh
cargo run --release -p forge-cli -- mcp
```

Example client configuration:

```json
{
  "mcpServers": {
    "apiwright": {
      "command": "/absolute/path/to/apiwright",
      "args": ["mcp"]
    }
  }
}
```

The server uses stdio for JSON-RPC. Do not print wrapper output to stdout;
client and server logs belong on stderr.

## Codex plugin

The repository contains a plugin manifest, MCP configuration, and `$apiwright`
skill under `plugins/apiwright`. A cached plugin cannot depend on the source
checkout or `cargo run`. Users installing a published version can download and
verify the matching host-native CLI without installing Rust:

```sh
python3 plugins/apiwright/scripts/stage_binary.py --download
```

Developers testing the current checkout build and stage it locally instead:

```sh
python3 plugins/apiwright/scripts/stage_binary.py
```

Then install the staged plugin:

```sh
codex plugin marketplace add .
codex plugin add apiwright@personal
```

The first command registers the repository marketplace from
`.agents/plugins/marketplace.json`; the second installs its ApiWright plugin.
Start a new thread after installation.
The staged binary under `plugins/apiwright/bin/` is generated and intentionally
not committed. Download or build it again after moving to another operating
system or CPU architecture; developers must also rebuild it after changing the
MCP implementation. Codex caches plugins by manifest version, so add or replace
a `+codex.local-…` SemVer build suffix in
`plugins/apiwright/.codex-plugin/plugin.json` and reinstall after a local
plugin update. Keep that local suffix uncommitted; CI requires the committed
plugin version to equal the Cargo workspace version.

## Tools

Every tool requires an absolute `root` pointing at a directory with
`project.json`. Request paths are relative to that root and must end in
`.request.json`.

| Tool | Effect | Network |
| --- | --- | --- |
| `inspect_project` | Lists requests, sequences, environments, asset metadata, and broken references | None |
| `read_request` | Reads the saved main document, assertion/hook sidecars, inherited selections, effective document, and revision | None |
| `read_project_code` | Reads one indexed JavaScript/TypeScript asset, capped at 256 KiB, so project code can be reviewed before trust | None |
| `read_project_file` | Reads one environment, sequence, or project asset and returns its revision | None |
| `validate_request` | Runs canonical request resolution and returns diagnostics | None |
| `write_request` | Creates or updates one request and its sidecars after a revision check | None |
| `add_assertion` | Adds one manual test without rewriting the request or hooks | None |
| `remove_assertion` | Removes one manual test by zero-based index and preserves the rest | None |
| `delete_request` | Deletes a request and its sidecars after checking sequence and auth references | None |
| `write_project_file` | Creates or updates an environment, sequence, or asset after a revision check | None |
| `delete_project_file` | Deletes a resource after protecting selected environments and referenced assets | None |
| `run_request` | Runs the configured mock by default; `realHttp: true` sends external HTTP and records its outcome in project history | Mock: none; real mode: external |
| `run_sequence` | Runs the sequence in declared order, using mocks unless `realHttp: true`; real HTTP outcomes go to project history | Mock: none; real mode: external |

`write_request` uses the same sidecar split as an IDE save. Pass the `revision`
from `read_request` as `expectedRevision`; a mismatch is a conflict, not
permission to overwrite. Use `expectedRevision: "new"` with `create: true` for
a new request in an existing project directory.

Assertions are manually maintained API tests stored beside each request.
`read_request` returns the complete assertion sidecar and current revision.
Use `add_assertion` with one entry to add a test, or `remove_assertion` with an
index from the current sidecar to remove one. Both tools revision-check and
preserve the request, hooks, and unrelated assertions. For example, a status
check is represented as:

```json
{
  "formatVersion": 1,
  "kind": "assertions",
  "assertions": [
    {
      "use": "builtin:assert-status@1",
      "with": {"expected": 200},
      "enabled": true
    }
  ]
}
```

After adding or removing a test, call `read_request` again for the new revision,
then validate and run its mock to see whether the test passes. `write_request`
remains available when an AI needs to edit the request document or several
sidecars together. `write_project_file` and `delete_project_file` manage environment,
sequence, and asset files; sequence creation should use request paths returned
by `inspect_project`. Request/resource deletions require the revision from a
read, and reference checks explain when a request or asset is still in use.

Successful and failed real HTTP runs from either execution tool are written to
the same `.forge-local/history.sqlite` database used by the IDE. Each matrix
case and each sequence request is a separate history row, so the IDE and local
coverage/flaky reports can see MCP runs. Mock runs are deliberately excluded
from that history and the tool result reports `history.mode: "mock"` with
`recorded: 0`; they therefore cannot inflate HTTP coverage. MCP history rows
store outcome metadata only and omit request/response headers and bodies. If
history cannot be written after a real request has run, the result still
contains the execution outcome and a `history.error` describing the persistence
failure.

## Safe AI workflow

1. Call `inspect_project` and resolve broken references relevant to the task.
2. Use `read_project_code` to inspect every referenced project-owned
   executable before enabling project code.
3. Call `read_request` and retain its revision.
4. Edit `document`, not the merged `effectiveDocument`; treat `document`,
   `assertions`, and `hooks` as one logical request.
5. Use `add_assertion` or `remove_assertion` for a single manual test change.
   Use `write_request` when the request document or multiple sidecars need to
   change together.
6. Read the request again after each write so the next operation uses its new
   revision.
7. Call `validate_request` and retain the returned revision.
8. Call `run_request` with that `expectedRevision` in its default mock mode.
   Set `realHttp: true` only after
   reviewing the method, URL, auth, hooks, and external effects with the user.

Project-owned JavaScript is disabled by default. Set `allowProjectCode: true`
only after reviewing request hooks and the configured project-auth request.
Validation returns a skipped result instead of executing untrusted code.

The MCP never reads `.env.local` or `*.secrets.json` as user-facing documents.
Execution resolves secrets internally, and run results omit raw response
bodies, cookies, and request headers. `read_request` deliberately returns the
saved request document verbatim so it can be edited; an accidentally committed
literal token or authorization value is therefore visible to the connected AI
client. Use the MCP only with a trusted client and keep sensitive values in
ApiWright's secret scopes.

## Project shape

Request-v1 projects are file trees rather than a conventional collection API.
Start from the absolute directory containing `project.json`; use exact,
project-relative paths returned by `inspect_project`. A request is a
`*.request.json` document with optional assertion, hook, environment, OpenAPI,
and Jira sidecars beside it or inherited from a parent folder. The MCP edits
these canonical files through `forge-core`, so revisions, validation, and
execution match the IDE and CLI. It does not create a parallel test database;
real MCP executions join the shared IDE history while mock runs remain
simulation-only.

## IDE synchronization

The filesystem is the shared source of truth, but an open IDE tab can contain
an unsaved private buffer. The MCP sees only saved files and cannot merge that
buffer. Save or discard IDE changes before an AI write, and reload the request
in the IDE afterward. The IDE also checks the revision captured when opening a request before saving,
including saves of invalid JSON. A conflict retains the local buffer and displays
the saved version for comparison; reload the saved request after preserving any
local edits you need. Revision checks do not make unsaved GUI state visible. A request revision covers the main request and
its assertion/hook sidecars. Referenced assets, environments, `project.json`,
and OpenAPI files remain ordinary project dependencies; review and version
them in the same trusted checkout, and use ApiWright's lockfile checks in CI.

MCP readers and MCP/IDE saves share a non-mutating, project-wide lock on the
existing `project.json`. Each file replacement is staged, synced, and atomic,
and an ordinary commit error triggers rollback from the staged backups. A
request plus its two optional sidecars is nevertheless a three-file
transaction: a sudden process or power loss between renames can leave files
from different generations because ApiWright does not keep a recovery journal.
Keep projects in Git; after an interrupted save, restore the logical request
from version control, reload it, and validate it before execution.

## Advisor versus MCP

The embedded [AI Advisor](advisor.md) sends bounded, redacted context to a
configured model and never edits files. MCP gives an external AI client
explicit project tools and can write or execute when authorized. Use Advisor
for a second opinion inside the request editor; use MCP for auditable,
multi-step automation over saved project files.

# Jira integration

ApiWright ties API tests to the tickets they belong to — and talks to Jira
directly. Editing links and using the Jira API require Pro, Enterprise, or the
60-day commercial trial. Existing links remain visible, copyable, and
openable in every edition. See [Licensing and billing](licensing.md).
The tracked public release workflow currently publishes the Free build; Jira
API features require an artifact from the separate private Pro distribution.

| Action | Free / source-available | Pro, Enterprise, or trial |
| --- | --- | --- |
| See inherited link markers and copy a stored link | Yes | Yes |
| Open a stored full Jira URL in the browser | Yes | Yes |
| Create, override, or remove a link in the IDE | No | Yes |
| Fetch ticket details or post a comment through Jira's API | No | Yes |
| Build/export the ticket coverage report | No | Yes |

## Linking tickets

Right-click a story folder or a single request → **Link Jira ticket…**, then
paste the full `http(s)` ticket URL. Requiring a full URL lets every edition
open the link without Jira connection settings.

Children inherit the nearest ancestor's link, so one link at the story level
covers every request underneath. A child can override that value; removing the
override reveals the inherited value again. Links live in plain `.forge-jira`
files next to the nodes they annotate, so they are reviewable in pull requests
and travel with exported bundles.

## Connecting to Jira

**Settings → Jira**:

| Field | Jira Cloud | Jira Server / Data Center |
|-------|------------|---------------------------|
| Base URL | `https://yourcompany.atlassian.net` | your Jira URL |
| Email | account email | leave empty |
| API token | [API token](https://id.atlassian.com/manage-profile/security/api-tokens) | personal access token |

The connection is stored per user in `~/.config/forge/jira.json`
(owner-readable only) — never in the project, never in Git.

## Ticket details and comments

Right-click a linked node → **Ticket details…** fetches summary, status,
type and assignee live from Jira. From the same dialog you can open the
ticket in the browser or post a comment (for example a run summary) without
leaving ApiWright.

The Jira API client extracts the issue key from the stored value. Existing or
imported bare keys such as `SHOP-42` can still resolve ticket details, but the
current link editor accepts full URLs such as
`https://example.atlassian.net/browse/SHOP-42`.

## Coverage report

**Run → Coverage report…** joins three axes over the execution history:

- **Ticket → tests** — every test grouped under its (inherited) Jira link.
- **Tests → OpenAPI** — which spec operations the tests cover, plus the
  list of operations no test touches.
- **History → health** — per test over the last 50 runs: pass rate,
  median/p95/max runtime, **flaky detection** (pass↔fail flips) and
  **hiccups** (transport errors plus runs slower than 3× the test's
  median).

Flaky and failing tests sort to the top of each section. Export the report
as Markdown or JSON, or post a ticket's section straight into Jira as a
comment ("Comment report to SHOP-42…").

Official Pro builds expose the same report headlessly for CI:

```
apiwright report <project-root> [--ticket SHOP-42] [--json] [--out report.md]
```

Verdicts (assertions passed/failed) are recorded into the run history as
tests execute; history from older ApiWright versions is judged by HTTP status
as a fallback.

## Security and automation boundary

Fetching details is read-only. Posting a comment mutates Jira and happens only
after an explicit action in the ticket or report dialog. The API token stays in
the per-user config file and is never written to `.forge-jira`, an export
bundle, or request files.

The source-available core MCP currently has no Jira or coverage-report tool.
This is intentional: local request editing and execution remain available,
while Jira-backed team workflows remain part of the premium model.

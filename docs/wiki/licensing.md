# Licensing and billing

ApiWright has three plans. The full legal terms live in
[LICENSE](https://github.com/EricVogt93/apiwright/blob/development/LICENSE) and
[COMMERCIAL-LICENSE.md](https://github.com/EricVogt93/apiwright/blob/development/COMMERCIAL-LICENSE.md).

| Plan | Price | For |
|------|-------|-----|
| Free | 0 € | Personal and other noncommercial use (PolyForm Noncommercial 1.0.0). No key, no account. |
| Pro | 12 € per user / month | Any commercial use. Billed monthly, cancel anytime. |
| Enterprise | Custom | Commercial use with a license server hosted in your own infrastructure, volume pricing and invoicing. |

## Source-available core and premium boundary

The local product model separates API-testing capability from organization
governance:

| Capability | Free / source-available | Pro | Enterprise |
| --- | --- | --- | --- |
| Local IDE, CLI, request-v1 model, OpenAPI, mocks, validation and execution | Yes, for noncommercial use | Yes | Yes |
| Local `apiwright mcp` server and Codex plugin | Yes, for noncommercial use | Yes | Yes |
| Existing Jira links remain visible, copyable and openable | Yes | Yes | Yes |
| Create/edit Jira links, fetch ticket details and post comments | No | Yes | Yes |
| Ticket/OpenAPI/history coverage report and Jira report comments | No | Yes | Yes |
| Commercial-use entitlement | No, except during the trial | Yes | Yes |
| Customer-hosted license server, volume terms and invoicing | No | No | Yes |

The MCP stays in the public adapter layer for the same reason as the CLI: it
reuses `forge-core` rather than owning product rules. Paid value sits at the
team and governance boundary. If hosted MCP, central policy/approval, SSO/RBAC,
or organization audit capabilities are introduced later, they belong on the
premium side; this table does not claim those services exist today.

The Pro engine lives in a private `forge-pro` crate. Building this repository
or downloading artifacts from its tracked public release workflow currently
produces the Free edition; the public crates compile without the private
overlay. A separate private distribution pipeline must supply `forge-pro` and
build with `--features pro` to produce Pro or Enterprise artifacts. The local
MCP therefore exposes only public core capabilities and does not bypass or
emulate Pro entitlements.

See [Jira integration](jira.md) for the current premium workflow and
[MCP and AI automation](mcp.md) for the source-available local AI adapter.

## 60-day commercial trial

Commercial teams can evaluate ApiWright on the Free plan for 60 days. Open
**Help → License & Billing** and click **Start 60-day commercial trial** —
no key, no account, nothing leaves the machine. The dialog shows the
remaining days; after expiry, commercial use requires a Pro or Enterprise
license.

## Activating a license

1. Open **Help → License & Billing**.
2. Paste the license key.
3. Enterprise only: enter the base URL of your own license server; Pro keys
   use the hosted server automatically.
4. Click **Activate**.

The key is validated online once and the verdict is cached in
`~/.config/forge/license.json`. ApiWright re-checks in the background at most
once a day; if the license server is unreachable the cached license keeps
working until the end of the already-paid period, so offline work is never
interrupted mid-subscription.

## Free and commercial use

The noncommercial source license is not enforced through telemetry — ApiWright
does not try to infer how a local project is used. Using ApiWright for a
commercial purpose without a paid license or active trial violates the license
terms even when the source-available core binary is technically able to run
the project.

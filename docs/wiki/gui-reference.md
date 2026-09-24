# GUI reference

ApiWright uses a resizable IDE shell. Compact icon controls expose their purpose after a two-second hover; disabled controls explain missing prerequisites where relevant.

## Application shell

From left to right and top to bottom:

- **Menu bar:** File, Run, View, Help, active environment, and Zen mode.
- **Activity bar:** opens Project, collection/history/run tools, legacy Environment, and Settings.
- **Project tool window:** file tree or Git view. Drag its right edge; collapsing it leaves the activity icon available.
- **Request editor:** Form/JSON views, request tests, OpenAPI assistance, and result splitter.
- **Catalog picker:** opens from **Catalog**, **Add test**, **Add preparation**, or **Use project data in this body**.
- **Right tool window:** icon tabs for OpenAPI, contract generation, API generation, k6 generation, and AI Advisor. Drag its left edge or collapse to an icon strip.
- **Bottom tools:** Run, Problems, Terminal, History; **More** contains Log, Console, Cookies, and Variables. Clicking the active tab collapses it.
- **Status bar:** branch/worktree actions, environment/readiness, execution time, and ApiWright version.

At narrow widths, Run, mode, Save, and environment stay visible; Undo, Catalog, Format, and Validate move into the toolbar menu. Less common result tabs remain under **More**.

## Project explorer

The **Files** tab presents ordinary filesystem folders. `assets` and `environments` use distinct icons; request, sequence, and general files are visually separated. Click a directory to make it the destination for new content. Jira icons appear only when enough horizontal space exists; inherited links are muted and own links use the accent color.

Folder/project context actions include **Run project/folder**, **New request**, **New folder**, **Add files**, recursive JSON beautification, ApiWright bundle/cURL export, ApiWright bundle import, Properties, file-manager reveal, Jira link actions, and Git actions. Request menus add Open, export, Properties, and Jira; asset menus can copy a stable reference or run affected requests.

The **Git** tab groups conflicts, untracked files, unstaged changes, partially staged files, and staged changes. Context Git actions stage, revert with confirmation, or open a commit dialog. The status-bar branch menu switches branches and opens **New worktree…**.

Properties show the effective path and request count, then configure inherited environment and OpenAPI source. Request Properties additionally toggles **Regression test** for `apiwright ci --regression`.

## Catalog workflow

Open the catalog from the task you are doing. **Add test** shows validation checks, **Add preparation** shows request and response steps, and **Use project data in this body** shows only project data. General catalog search keeps its source and intent filters when you select an entry. Select an entry, configure it, then choose **Add** or **Replace**. Leaving an entry keeps its unfinished values. Parameter fields default to literal values and can switch to binding, environment, runtime, matrix, or—where valid—secret references.

The preview does not send HTTP. Before-request assets show request differences; after-response assets use the last response and show assertion results, logs, and runtime writes. Project JavaScript remains disabled until **Allow project code** is explicitly enabled.

Tests and preparation steps are edited in the request editor's **Tests** view and saved with the request's sidecars. Each row can be enabled, moved, replaced, removed, or expanded to edit its `with` JSON inline. Row identity and expansion stay with the test through reordering and undo. **Undo** restores the previous request or test edit. Shared project assets show how many requests use them and can run those affected requests. The parameter form configures the current request's reference values; editing an asset file changes shared behavior.

## Request editor

The **Request** view starts with a form for method, URL, query parameters, headers, and body. Text uses a plain text editor, Form uses key/value rows, and Multipart/Binary use native part and file controls. Switching body types keeps each mode's draft for the current editing session. The **JSON** view edits the same document; form edits preserve advanced fields that are not shown in the form. Invalid JSON body drafts remain visible and block save/run until corrected or reset. Timeout is explicit as **Project default**, **Custom**, or **Disabled**. A dirty marker appears beside the request path. The JSON editor provides highlighting, numbered lines, diagnostics, minimap, scrolling, and OpenAPI assistance.

The toolbar runs, saves, formats, validates, overrides the inherited environment, and displays **HTTP** or **Mock** beside Run. Project-code trust and sequence actions remain in the overflow menu. Reopening the active dirty file focuses its current buffer. Opening another request, closing the editor, switching projects, or quitting while dirty offers **Save**, **Discard**, or **Cancel**; failed saves keep the current request open. `Ctrl/Cmd+Enter` runs the open request-v1 editor, and `Ctrl/Cmd+W` closes it through the same decision.

Drag the splitter between request and results. `Ctrl/Cmd+mouse wheel` over either area scales editor/result typography without scaling the Project or right tool windows.

The lower tabs are deliberately separate:

- **Response:** run status, HTTP status/time/bytes, OpenAPI response warnings, formatted JSON/XML/HTML or Raw, and Copy.
- **Tests:** pass/fail details from the last run. Configuration is in the editor's **Tests** view.
- **Auth:** reusable auth request/provider setup and refresh policy.
- **Runtime:** run environment, duration, extracted values, and transport data.
- **Diagnostics:** JSON, reference, OpenAPI, and execution errors.

Matrix and batch runs add a run selector above the tabs and update the active response when a case is selected. The result header records the last run's mode, environment, and age, then marks results out of date when the request, tests, mode, or environment changes.

## Import and export outcomes

Postman, Bruno, OpenAPI, and cURL imports leave a report open after writing. It lists created, skipped, blocked, warning, and quarantined counts; request-v1 entries can be opened directly from the report. Postman/Bruno conversion losses remain listed after the import dialog closes.

Postman/Bruno request export shows its loss report before writing. Choose another destination or cancel there; if a file already exists, **Overwrite file** is a separate explicit action. Completed exports keep the destination and loss details visible until dismissed.

## Menus and shortcuts

**File** creates/opens projects, saves, imports cURL/OpenAPI/Postman/Bruno, opens Settings, and quits. **Run** sends the current request, runs a legacy collection, or opens gRPC. **View** controls tool windows, theme, Zen mode, environment management, and the User tour. **Help** checks for updates and opens About.

| Action | Shortcut |
| --- | --- |
| Save / Save all | `Ctrl/Cmd+S` / `Ctrl/Cmd+Shift+S` |
| Send request | `Ctrl/Cmd+Enter` |
| Close / next / previous tab | `Ctrl/Cmd+W`, `Ctrl/Cmd+Tab`, `Ctrl/Cmd+Shift+Tab` |
| Open project | `Ctrl/Cmd+O` |
| Toggle Collections | `Ctrl/Cmd+1` |
| Zen mode | `Ctrl/Cmd+Shift+F11` |
| Settings | `Ctrl/Cmd+Alt+S` |
| Import cURL | `Ctrl/Cmd+Shift+V` |
| Search actions | `Ctrl/Cmd+Shift+A` |
| Search Everywhere | bare `Shift` twice |

Zen mode hides application chrome. Hover the left, right, or bottom screen edge to reveal its tools temporarily; hover the upper-right corner for **Exit Zen**.

## User tour

Start **View → User tour…** at any time. It turns off Zen mode, reveals the relevant tool window for each step, outlines the live target, and never creates or mutates project data. Navigate with buttons or Left/Right Arrow; close with Esc.

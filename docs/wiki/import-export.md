# Import, export and portability

ApiWright supports lossless project bundles, request-level Postman/Bruno interchange, and code snippets for sharing a request with another language or tool.

## Lossless bundles

Use the project-tree context menu or CLI:

```sh
apiwright export requests/orders/get.request.json --root . --format json -o get-order.forge.json
apiwright export requests/orders --root . --format curl -o orders.forge.sh
apiwright import orders.forge.sh .
```

A request export includes its `.assertions.json` and `.hooks.json` siblings, project settings, selected environment and OpenAPI contract, plus linked data and executable assets (including relative JavaScript imports). Folder exports retain the selected files and add dependencies for requests inside the folder. Bundle paths preserve their project-relative layout, so import into an empty project root to keep aliases and references valid. Importing into an existing project preserves its `project.json` and writes only free paths; any other destination collision aborts before writing. UTF-8 remains readable; binary files alone use Base64. Secret providers (`.env.local`, `*.secrets.json`) and runtime state are excluded. A reference to a secret store or a missing dependency blocks export instead of producing a broken bundle.

JSON bundles identify themselves as `forge.bundle` format version 1. A ApiWright cURL bundle is both a shell representation and a lossless bundle encoded in marked comments. Its transport command runs only when the request is self-contained; unresolved project variables, bindings, pipeline steps, project authentication, referenced bodies and binary paths stay visible as a commented preview, so the script cannot silently send an incomplete request. Enabled query names and values are URL-encoded. Importing either bundle restores the complete ApiWright files. Paths are validated against traversal, duplicate paths are rejected, and conflicting files abort before writing.

## Postman and Bruno request exports

Select a saved request in the project tree and choose **Export → Postman collection** or **Bruno request file**. Postman output is an importable collection with one request; Bruno output is a `.bru` file for an existing Bruno collection. Method, URL, enabled state, headers, query fields, supported inline bodies and descriptions are mapped. Environment and secret references become `{{name}}` placeholders; their values are never included.

The export status includes a loss report for project authentication, assertion and hook sidecars, bindings, scripts, mocks, referenced bodies and other request-v1 features the target cannot represent. The generated Postman request and Bruno request explicitly use no authentication, so configure auth in the target client before sending it.

## Code snippets

The request toolbar can render cURL, HTTPie, JavaScript `fetch`, Axios, Python `requests`, Go and Java. These are transport snippets, not backups: catalog references, assertions, hooks and folder properties are not representable in ordinary cURL or language snippets.

## Imports

- **cURL:** paste a command; ApiWright maps method, URL, headers, query and supported bodies into the active request-v1 project or legacy collection workspace.
- **OpenAPI:** select operations from JSON/YAML and generate request skeletons. Request-v1 imports are grouped under `requests/` and retain a project-relative OpenAPI selection; copy the spec into `specs/` to keep it portable.
- **Postman:** import collections or environments into request-v1 projects or legacy workspaces. Compatible requests are written under `requests/`; unsupported scripts/auth are listed, with script source preserved in the non-executable import quarantine. Public collection variables become a flat environment JSON file. Secret variables use `${secret.NAME}` references and values go to the ignored project `.env.local` file.
- **Bruno:** import folder structure, requests and optionally environments into request-v1 projects or legacy workspaces. Compatible collection variables become a flat environment file; unsupported request features are listed, and scripts are preserved in the non-executable import quarantine. Bruno exports do not contain secret values, so add declared secret values to `.env.local` before running requests. The OpenCollection report separates direct-native, transformed-native, and blocked contract assertions, and reports generated auth-provider, recognized auth-script, and remaining blocked auth-script counts. Recognized Keycloak, CRID Azure, and IPD5 auth flows become ordinary generated requests under `requests/auth/bruno`; unknown network scripts remain blocked. Recognized `validateContract` tests use a project-contained schema bundle reference and do not require project-code trust; unrecognized response transforms fail closed with a source-path diagnostic.
- Postman URLs that embed query parameters in `raw` keep their decoded values during import; exporters encode query names and values into the URL.
- **ApiWright bundle:** restore a lossless JSON or ApiWright-generated cURL bundle.

The importer previews how many requests can be represented by request-v1 and lists blocked items and metadata that cannot be represented before writing. The original export remains the source for blocked Postman/Bruno behavior such as inline scripts. Imported files are ordinary project files; review them before committing, especially authentication values from third-party exports.

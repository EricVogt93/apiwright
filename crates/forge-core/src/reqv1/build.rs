//! Document → canonical IR: resolve bindings (with cycle detection), run
//! built-in generators, interpolate variables, and assemble a
//! [`ResolvedRequest`]. Pure (no network). See §6–§8, §12.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use serde_json::{Map, Value};

use super::catalog::{validate_builtin, BuiltinTarget};
use super::diag::{Code, Diagnostic, Errors};
use super::ir::{
    ResolvedBody, ResolvedHeader, ResolvedMock, ResolvedMultipartData, ResolvedMultipartPart,
    ResolvedPipelineEntry, ResolvedRequest,
};
use super::model::{
    Binding, BodySpec, MockDef, MultipartPart, PipelineEntry, RequestDocument, RequestSpec,
};
use super::refs::{RefResolver, RefScheme};
use super::resolve::DataStore;
use super::vars::{interpolate, sensitive_name, Scopes, SecretSink};

/// Everything the builder needs beyond the document itself.
pub struct BuildInputs<'a> {
    pub resolver: &'a RefResolver,
    pub store: &'a DataStore<'a>,
    /// Directory of the request file (for relative refs).
    pub base_dir: &'a Path,
    pub env: Value,
    /// One matrix case (object), or Null when not a matrix run.
    pub matrix: Value,
    /// Runtime values carried in from earlier requests in a sequence
    /// (`${runtime.*}`). Empty object for a standalone run.
    pub runtime: Value,
    pub secret: &'a (dyn Fn(&str) -> Option<String> + Sync),
}

/// Build the canonical IR from a parsed document. Collects every independent
/// error before failing (§7).
pub fn build_ir(doc: &RequestDocument, inp: &BuildInputs<'_>) -> Result<ResolvedRequest, Errors> {
    let mut sink = SecretSink::default();
    let mut errors: Vec<Diagnostic> = Vec::new();
    if let Some(runtime) = inp.runtime.as_object() {
        for (name, value) in runtime {
            if sensitive_name(name) {
                sink.record_value(value);
            }
        }
    }

    // 1. Resolve bindings (topological, cycle-checked, generators run here).
    let bindings = match resolve_bindings(&doc.bindings, inp, &mut sink) {
        Ok(v) => v,
        Err(mut e) => {
            errors.append(&mut e.0);
            Value::Object(Map::new())
        }
    };

    let scopes = Scopes {
        env: &inp.env,
        bindings: &bindings,
        matrix: &inp.matrix,
        runtime: &inp.runtime,
        secret: inp.secret,
    };

    // 2. Interpolate the request itself.
    let request = build_request(&doc.request, inp, &scopes, &mut sink, &mut errors);

    // 3. Resolve the pipeline (locate assets, interpolate `with`).
    let pipeline = build_pipeline(
        &doc.pipeline,
        request.as_ref(),
        inp,
        &scopes,
        &mut sink,
        &mut errors,
    );

    // 4. Resolve the mock (if any).
    let mock = doc
        .mock
        .as_ref()
        .and_then(|m| build_mock(m, inp, &scopes, &mut sink, &mut errors));

    if !errors.is_empty() {
        for diagnostic in &mut errors {
            mask_diagnostic(diagnostic, &sink.values);
        }
        return Err(Errors(errors));
    }
    let request = request.expect("no errors implies a request");

    Ok(ResolvedRequest {
        id: doc.meta.id.clone(),
        name: doc.meta.name.clone(),
        method: request.method,
        url: request.url,
        headers: request.headers,
        query: request.query,
        body: request.body,
        timeout: match doc.request.settings.timeout_ms {
            Some(0) => None,
            Some(milliseconds) => Some(Duration::from_millis(milliseconds)),
            None => Some(Duration::from_secs(30)),
        },
        follow_redirects: doc.request.settings.follow_redirects.unwrap_or(true),
        max_redirects: doc.request.settings.max_redirects.unwrap_or(10),
        encode_url: doc.request.settings.encode_url.unwrap_or(true),
        pipeline,
        mock,
        bindings,
        environment: sanitize_context_snapshot(&inp.env, &sink.values),
        runtime: sanitize_context_snapshot(&inp.runtime, &sink.values),
        runtime_unmasked: inp.runtime.clone(),
        secret_values: sink.values,
    })
}

fn sanitize_context_snapshot(value: &Value, secrets: &[String]) -> Value {
    match value {
        Value::String(text) => Value::String(mask_text(text, secrets)),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| sanitize_context_snapshot(value, secrets))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .filter(|(name, _)| !sensitive_context_name(name))
                .map(|(name, value)| (name.clone(), sanitize_context_snapshot(value, secrets)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn sensitive_context_name(name: &str) -> bool {
    sensitive_name(name)
}

fn mask_diagnostic(diagnostic: &mut Diagnostic, secrets: &[String]) {
    diagnostic.message = mask_text(&diagnostic.message, secrets);
    if let Some(path) = &mut diagnostic.instance_path {
        *path = mask_text(path, secrets);
    }
    if let Some(asset_ref) = &mut diagnostic.asset_ref {
        *asset_ref = mask_text(asset_ref, secrets);
    }
}

fn mask_text(text: &str, secrets: &[String]) -> String {
    secrets.iter().fold(text.to_string(), |masked, secret| {
        if secret.is_empty() {
            masked
        } else {
            masked.replace(secret, "***")
        }
    })
}

struct ResolvedReqParts {
    method: crate::model::Method,
    url: String,
    headers: Vec<ResolvedHeader>,
    query: Vec<ResolvedHeader>,
    body: ResolvedBody,
}

// ---------------------------------------------------------------------
// Bindings
// ---------------------------------------------------------------------

/// Resolve all bindings into a JSON object. Bindings may reference each other
/// via `${bindings.x}`; resolution order is a topological sort of that
/// dependency graph, and a cycle is a `BINDING_CYCLE` error.
fn resolve_bindings(
    bindings: &BTreeMap<String, Binding>,
    inp: &BuildInputs<'_>,
    sink: &mut SecretSink,
) -> Result<Value, Errors> {
    // Dependency graph over `${bindings.NAME}` references.
    let mut deps: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for (name, binding) in bindings {
        deps.insert(name, binding_deps(binding));
    }

    let order = topo_order(&deps).map_err(|cycle| {
        Errors::one(
            Code::BindingCycle,
            format!("binding cycle: {}", cycle.join(" -> ")),
        )
    })?;

    let mut resolved = Map::new();
    let mut errors = Vec::new();
    for name in order {
        let binding = &bindings[name];
        let partial = Value::Object(resolved.clone());
        match resolve_one_binding(binding, inp, &partial, sink) {
            Ok(v) => {
                resolved.insert(name.to_string(), v);
            }
            Err(mut d) => {
                d.instance_path = Some(format!("/bindings/{name}"));
                errors.push(d);
            }
        }
    }
    if !errors.is_empty() {
        return Err(Errors(errors));
    }
    Ok(Value::Object(resolved))
}

/// Resolve one binding in isolation (no other bindings in scope). Used by
/// matrix resolution, where `${bindings.*}` is not available (§13).
pub fn resolve_single_binding(
    binding: &Binding,
    inp: &BuildInputs<'_>,
) -> Result<Value, Diagnostic> {
    let empty = Value::Object(Map::new());
    let mut sink = SecretSink::default();
    resolve_one_binding(binding, inp, &empty, &mut sink)
}

fn resolve_one_binding(
    binding: &Binding,
    inp: &BuildInputs<'_>,
    resolved_bindings: &Value,
    sink: &mut SecretSink,
) -> Result<Value, Diagnostic> {
    let scopes = Scopes {
        env: &inp.env,
        bindings: resolved_bindings,
        matrix: &inp.matrix,
        runtime: &inp.runtime,
        secret: inp.secret,
    };
    match binding {
        Binding::Value(v) => interpolate(&v.value, &scopes, sink),
        Binding::Ref(r) => {
            let desc = inp.resolver.resolve(&r.reference, inp.base_dir)?;
            let value = inp.store.resolve(&desc, &r.patch)?;
            // §6 step 8: data content is NOT re-scanned for ${...}.
            Ok(value)
        }
        Binding::Use(u) => {
            let desc = inp.resolver.resolve(&u.uses, inp.base_dir)?;
            let input = interpolate(&Value::Object(u.with.clone()), &scopes, sink)?;
            if desc.scheme == RefScheme::Builtin {
                run_generator(&desc.address, desc.version, &input, &u.uses)
            } else {
                // Project generator on the QuickJS host. ctx carries the
                // already-resolved earlier bindings (topological order).
                let ctx = serde_json::json!({ "bindings": resolved_bindings });
                super::jshost::run_js_asset(&desc.address, &ctx, &input)
                    .map_err(|d| d.with_ref(&u.uses))
            }
        }
    }
}

/// Names of other bindings a binding depends on (via `${bindings.NAME}`).
fn binding_deps(binding: &Binding) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    match binding {
        Binding::Value(v) => collect_binding_refs(&v.value, &mut out),
        Binding::Use(u) => collect_binding_refs(&Value::Object(u.with.clone()), &mut out),
        Binding::Ref(_) => {}
    }
    out
}

/// Scan a JSON value for `${bindings.NAME...}` and collect each NAME.
fn collect_binding_refs(node: &Value, out: &mut BTreeSet<String>) {
    match node {
        Value::String(s) => {
            let bytes = s.as_bytes();
            let mut i = 0;
            while let Some(rel) = s[i..].find("${bindings.") {
                let start = i + rel + "${bindings.".len();
                let rest = &s[start..];
                let end = rest.find(['.', '}']).unwrap_or(rest.len());
                if !rest[..end].is_empty() {
                    out.insert(rest[..end].to_string());
                }
                i = start;
                if i >= bytes.len() {
                    break;
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| collect_binding_refs(v, out)),
        Value::Object(map) => map.values().for_each(|v| collect_binding_refs(v, out)),
        _ => {}
    }
}

/// Kahn topological sort. Returns node order, or a cycle path on failure.
/// Edges only to nodes that exist in `deps` (external refs are ignored here;
/// missing bindings surface as MISSING_VARIABLE during interpolation).
fn topo_order<'a>(
    deps: &'a BTreeMap<&'a str, BTreeSet<String>>,
) -> Result<Vec<&'a str>, Vec<String>> {
    // `pending[n]` = how many of n's dependencies are not yet resolved.
    let mut pending: BTreeMap<&str, usize> = deps
        .iter()
        .map(|(k, ds)| {
            (
                *k,
                ds.iter().filter(|d| deps.contains_key(d.as_str())).count(),
            )
        })
        .collect();
    let mut queue: Vec<&str> = pending
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(k, _)| *k)
        .collect();
    queue.sort();
    let mut order = Vec::new();
    while let Some(node) = queue.pop() {
        order.push(node);
        // Anything that depended on `node` loses one pending edge.
        for (other, ds) in deps {
            if ds.contains(node) {
                if let Some(n) = pending.get_mut(other) {
                    *n -= 1;
                    if *n == 0 {
                        queue.push(other);
                        queue.sort();
                    }
                }
            }
        }
    }
    if order.len() == deps.len() {
        Ok(order)
    } else {
        let cycle: Vec<String> = deps
            .keys()
            .filter(|k| !order.contains(k))
            .map(|k| k.to_string())
            .collect();
        Err(cycle)
    }
}

/// Built-in generators (§5). Deterministic sources only.
fn run_generator(
    name: &str,
    version: Option<u32>,
    input: &Value,
    raw: &str,
) -> Result<Value, Diagnostic> {
    validate_builtin(name, version, BuiltinTarget::Binding, input, raw)?;
    match name {
        "uuid" => Ok(Value::String(uuid::Uuid::new_v4().to_string())),
        "now" => Ok(Value::from(chrono::Utc::now().timestamp())),
        other => Err(Diagnostic::new(
            Code::AssetNotFound,
            format!("unknown builtin generator {other:?}"),
        )
        .with_ref(raw)),
    }
}

// ---------------------------------------------------------------------
// Request / pipeline / mock
// ---------------------------------------------------------------------

fn build_request(
    spec: &RequestSpec,
    _inp: &BuildInputs<'_>,
    scopes: &Scopes<'_>,
    sink: &mut SecretSink,
    errors: &mut Vec<Diagnostic>,
) -> Option<ResolvedReqParts> {
    let url = interp_string(&spec.url, scopes, sink, "/request/url", errors)?;
    let headers = resolve_headers(&spec.headers, scopes, sink, "/request/headers", errors);
    let query = resolve_headers(&spec.query, scopes, sink, "/request/query", errors);
    let body = build_body(spec.body.as_ref(), _inp, scopes, sink, errors);
    Some(ResolvedReqParts {
        method: spec.method,
        url,
        headers,
        query,
        body,
    })
}

fn resolve_headers(
    headers: &[super::model::HeaderSpec],
    scopes: &Scopes<'_>,
    sink: &mut SecretSink,
    base_path: &str,
    errors: &mut Vec<Diagnostic>,
) -> Vec<ResolvedHeader> {
    let mut out = Vec::new();
    for (i, h) in headers.iter().enumerate() {
        if !h.enabled {
            continue;
        }
        let path = format!("{base_path}/{i}/value");
        if let Some(value) = interp_string(&h.value, scopes, sink, &path, errors) {
            out.push(ResolvedHeader {
                name: h.name.clone(),
                value,
            });
        }
    }
    out
}

fn build_body(
    body: Option<&BodySpec>,
    inp: &BuildInputs<'_>,
    scopes: &Scopes<'_>,
    sink: &mut SecretSink,
    errors: &mut Vec<Diagnostic>,
) -> ResolvedBody {
    use super::model::BodyType;
    match body {
        None => ResolvedBody::None,
        Some(BodySpec::Multipart(body)) => {
            let mut parts = Vec::new();
            for (index, part) in body.parts.iter().enumerate() {
                let part_path = format!("/request/body/parts/{index}");
                match part {
                    MultipartPart::Text {
                        name,
                        value,
                        filename,
                        content_type,
                        enabled,
                    } => {
                        if !enabled {
                            continue;
                        }
                        let Some(value) = interp_string(
                            value,
                            scopes,
                            sink,
                            &format!("{part_path}/value"),
                            errors,
                        ) else {
                            continue;
                        };
                        parts.push(ResolvedMultipartPart {
                            name: name.clone(),
                            content_type: content_type.clone(),
                            filename: filename.clone(),
                            data: ResolvedMultipartData::Text(value),
                        });
                    }
                    MultipartPart::File {
                        name,
                        file,
                        filename,
                        content_type,
                        enabled,
                    } => {
                        if !enabled {
                            continue;
                        }
                        if let Some(path) =
                            resolve_body_file(file, inp, &format!("{part_path}/file"), errors)
                        {
                            parts.push(ResolvedMultipartPart {
                                name: name.clone(),
                                content_type: content_type.clone(),
                                filename: filename.clone().or_else(|| {
                                    path.file_name()
                                        .map(|name| name.to_string_lossy().into_owned())
                                }),
                                data: ResolvedMultipartData::File(path),
                            });
                        }
                    }
                }
            }
            ResolvedBody::Multipart(parts)
        }
        Some(BodySpec::Binary(body)) => {
            let Some(path) = resolve_body_file(&body.file, inp, "/request/body/file", errors)
            else {
                return ResolvedBody::None;
            };
            match std::fs::read(&path) {
                Ok(data) => ResolvedBody::Binary {
                    content_type: body.content_type.clone(),
                    data,
                },
                Err(error) => {
                    errors.push(
                        Diagnostic::new(
                            Code::AssetNotFound,
                            format!("cannot read body file {}: {error}", path.display()),
                        )
                        .at("/request/body/file")
                        .with_ref(&body.file),
                    );
                    ResolvedBody::None
                }
            }
        }
        Some(BodySpec::Inline(b)) => {
            let value = match &b.value {
                Some(v) => match interpolate(v, scopes, sink) {
                    Ok(v) => v,
                    Err(mut d) => {
                        d.instance_path = Some("/request/body/value".to_string());
                        errors.push(d);
                        return ResolvedBody::None;
                    }
                },
                None => return ResolvedBody::None,
            };
            match b.body_type {
                BodyType::Json => ResolvedBody::Json(value),
                BodyType::Text => ResolvedBody::Text(value.as_str().unwrap_or("").to_string()),
                BodyType::Form => ResolvedBody::Form(value_to_form(&value)),
                BodyType::None => ResolvedBody::None,
            }
        }
        Some(BodySpec::Ref(r)) => {
            let desc = match inp.resolver.resolve(&r.reference, inp.base_dir) {
                Ok(d) => d,
                Err(mut d) => {
                    d.instance_path = Some("/request/body/ref".to_string());
                    errors.push(d);
                    return ResolvedBody::None;
                }
            };
            match inp.store.resolve(&desc, &[]) {
                Ok(v) => ResolvedBody::Json(v),
                Err(mut d) => {
                    d.instance_path = Some("/request/body/ref".to_string());
                    errors.push(d);
                    ResolvedBody::None
                }
            }
        }
    }
}

fn resolve_body_file(
    reference: &str,
    inp: &BuildInputs<'_>,
    instance_path: &str,
    errors: &mut Vec<Diagnostic>,
) -> Option<std::path::PathBuf> {
    let descriptor = match inp.resolver.resolve(reference, inp.base_dir) {
        Ok(descriptor) => descriptor,
        Err(mut diagnostic) => {
            diagnostic.instance_path = Some(instance_path.to_string());
            errors.push(diagnostic);
            return None;
        }
    };
    if descriptor.scheme != RefScheme::File
        || descriptor.pointer.is_some()
        || descriptor.version.is_some()
    {
        errors.push(
            Diagnostic::new(
                Code::InvalidAssetInput,
                "body files must be unversioned project file references without JSON pointers",
            )
            .at(instance_path)
            .with_ref(reference),
        );
        return None;
    }
    let path = std::path::PathBuf::from(&descriptor.address);
    match std::fs::canonicalize(&path) {
        Ok(path) if path.is_file() && path.starts_with(inp.resolver.root()) => Some(path),
        Ok(path) if !path.starts_with(inp.resolver.root()) => {
            errors.push(
                Diagnostic::new(
                    Code::PathEscape,
                    format!(
                        "body file resolves outside the project root: {}",
                        path.display()
                    ),
                )
                .at(instance_path)
                .with_ref(reference),
            );
            None
        }
        Ok(path) => {
            errors.push(
                Diagnostic::new(
                    Code::AssetNotFound,
                    format!("body file is not a regular file: {}", path.display()),
                )
                .at(instance_path)
                .with_ref(reference),
            );
            None
        }
        Err(error) => {
            errors.push(
                Diagnostic::new(
                    Code::AssetNotFound,
                    format!("body file does not exist: {} ({error})", path.display()),
                )
                .at(instance_path)
                .with_ref(reference),
            );
            None
        }
    }
}

fn value_to_form(value: &Value) -> Vec<ResolvedHeader> {
    match value {
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| ResolvedHeader {
                name: k.clone(),
                value: match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                },
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn build_pipeline(
    entries: &[PipelineEntry],
    request: Option<&ResolvedReqParts>,
    inp: &BuildInputs<'_>,
    scopes: &Scopes<'_>,
    sink: &mut SecretSink,
    errors: &mut Vec<Diagnostic>,
) -> Vec<ResolvedPipelineEntry> {
    let mut out = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        if !e.enabled {
            continue;
        }
        let path = format!("/pipeline/{i}");
        let asset = match inp.resolver.resolve(&e.uses, inp.base_dir) {
            Ok(a) => a,
            Err(mut d) => {
                d.instance_path = Some(path);
                errors.push(d);
                continue;
            }
        };
        let mut input = match interpolate(&Value::Object(e.with.clone()), scopes, sink) {
            Ok(v) => v,
            Err(mut d) => {
                d.instance_path = Some(format!("{path}/with"));
                errors.push(d);
                continue;
            }
        };
        if asset.scheme == RefScheme::Builtin && asset.address == "assert-openapi-response" {
            if let Err(mut diagnostic) = prepare_openapi_input(&mut input, request, inp, &e.uses) {
                diagnostic.instance_path = Some(path);
                errors.push(diagnostic);
                continue;
            }
        }
        if asset.scheme == RefScheme::Builtin && asset.address == "assert-schema" {
            if let Err(mut diagnostic) = prepare_schema_input(&mut input, inp, &e.uses) {
                diagnostic.instance_path = Some(path);
                errors.push(diagnostic);
                continue;
            }
        }
        if asset.scheme == RefScheme::Builtin {
            if let Err(mut diagnostic) = validate_builtin(
                &asset.address,
                asset.version,
                BuiltinTarget::Pipeline(e.phase),
                &input,
                &e.uses,
            ) {
                diagnostic.instance_path = Some(path);
                errors.push(diagnostic);
                continue;
            }
        }
        out.push(ResolvedPipelineEntry {
            phase: e.phase,
            asset,
            input,
        });
    }
    out
}

fn prepare_schema_input(
    input: &mut Value,
    inp: &BuildInputs<'_>,
    raw: &str,
) -> Result<(), Diagnostic> {
    let object = input.as_object_mut().ok_or_else(|| {
        Diagnostic::new(Code::InvalidAssetInput, "builtin input must be an object").with_ref(raw)
    })?;
    if let Some(reference) = object.get("schemaRef").and_then(Value::as_str) {
        if object.contains_key("schema") {
            return Err(Diagnostic::new(
                Code::InvalidAssetInput,
                "JSON Schema validation accepts only one of \"schema\" and \"schemaRef\"",
            )
            .with_ref(raw));
        }
        let descriptor = inp.resolver.resolve(reference, inp.base_dir)?;
        if descriptor.pointer.is_some() {
            return Err(Diagnostic::new(
                Code::InvalidAssetInput,
                "JSON Schema file references do not support JSON pointers; use definition instead",
            )
            .with_ref(reference));
        }
        let source = std::fs::read(&descriptor.address).map_err(|error| {
            Diagnostic::new(
                Code::AssetNotFound,
                format!("cannot read JSON Schema {}: {error}", descriptor.address),
            )
            .with_ref(reference)
        })?;
        let schema: Value = serde_json::from_slice(&source).map_err(|error| {
            Diagnostic::new(
                Code::InvalidAssetInput,
                format!("invalid JSON Schema {}: {error}", descriptor.address),
            )
            .with_ref(reference)
        })?;
        object.insert("schema".to_string(), schema);
        object.remove("schemaRef");
    }
    if let Some(definition) = object.get("definition").and_then(Value::as_str) {
        if !object
            .get("schema")
            .and_then(|schema| schema.get("$defs"))
            .and_then(Value::as_object)
            .is_some_and(|definitions| definitions.contains_key(definition))
        {
            return Err(Diagnostic::new(
                Code::InvalidAssetInput,
                format!("unknown JSON Schema definition {definition:?}"),
            )
            .with_ref(raw));
        }
    }
    Ok(())
}

fn prepare_openapi_input(
    input: &mut Value,
    request: Option<&ResolvedReqParts>,
    inp: &BuildInputs<'_>,
    raw: &str,
) -> Result<(), Diagnostic> {
    let object = input.as_object_mut().ok_or_else(|| {
        Diagnostic::new(Code::InvalidAssetInput, "builtin input must be an object").with_ref(raw)
    })?;
    if let Some(reference) = object.get("specRef").and_then(Value::as_str) {
        if object.contains_key("spec") {
            return Err(Diagnostic::new(
                Code::InvalidAssetInput,
                "OpenAPI response validation accepts only one of \"spec\" and \"specRef\"",
            )
            .with_ref(raw));
        }
        let descriptor = inp.resolver.resolve(reference, inp.base_dir)?;
        if descriptor.pointer.is_some() {
            return Err(Diagnostic::new(
                Code::InvalidAssetInput,
                "OpenAPI file references do not support JSON pointers",
            )
            .with_ref(reference));
        }
        let source = std::fs::read_to_string(&descriptor.address).map_err(|error| {
            Diagnostic::new(
                Code::AssetNotFound,
                format!("cannot read OpenAPI spec {}: {error}", descriptor.address),
            )
            .with_ref(reference)
        })?;
        object.insert("spec".to_string(), Value::String(source));
        object.remove("specRef");
    }
    if let Some(request) = request {
        object
            .entry("method".to_string())
            .or_insert_with(|| Value::String(request.method.as_str().to_string()));
        object
            .entry("url".to_string())
            .or_insert_with(|| Value::String(request.url.clone()));
    }
    Ok(())
}

fn build_mock(
    mock: &MockDef,
    inp: &BuildInputs<'_>,
    scopes: &Scopes<'_>,
    sink: &mut SecretSink,
    errors: &mut Vec<Diagnostic>,
) -> Option<ResolvedMock> {
    match mock {
        MockDef::Static(m) => {
            let headers = resolve_headers(&m.headers, scopes, sink, "/mock/headers", errors);
            let body = build_body(m.body.as_ref(), inp, scopes, sink, errors);
            Some(ResolvedMock::Static {
                status: m.status,
                headers,
                body,
                delay_ms: m.delay_ms.unwrap_or(0),
            })
        }
        MockDef::Dynamic(m) => {
            let asset = match inp.resolver.resolve(&m.uses, inp.base_dir) {
                Ok(a) => a,
                Err(mut d) => {
                    d.instance_path = Some("/mock/use".to_string());
                    errors.push(d);
                    return None;
                }
            };
            let input = match interpolate(&Value::Object(m.with.clone()), scopes, sink) {
                Ok(input) => input,
                Err(mut diagnostic) => {
                    diagnostic.instance_path = Some("/mock/with".to_string());
                    errors.push(diagnostic);
                    return None;
                }
            };
            Some(ResolvedMock::Dynamic { asset, input })
        }
    }
}

fn interp_string(
    s: &str,
    scopes: &Scopes<'_>,
    sink: &mut SecretSink,
    path: &str,
    errors: &mut Vec<Diagnostic>,
) -> Option<String> {
    match interpolate(&Value::String(s.to_string()), scopes, sink) {
        Ok(Value::String(v)) => Some(v),
        Ok(other) => Some(other.to_string()),
        Err(mut d) => {
            d.instance_path = Some(path.to_string());
            errors.push(d);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::ProjectConfig;
    use super::*;

    #[test]
    fn failed_builtin_validation_masks_interpolated_secrets() {
        let root = tempfile::tempdir().unwrap();
        let project = ProjectConfig::default();
        let resolver = RefResolver::new(root.path(), &project).unwrap();
        let store = DataStore::new(&resolver);
        let empty = Value::Object(Map::new());
        let secret = |name: &str| (name == "pattern").then(|| "sensitive[".to_string());
        let doc = RequestDocument::parse(
            r#"{
                "formatVersion": 1,
                "kind": "request",
                "meta": {"id": "mask", "name": "Mask"},
                "request": {"method": "GET", "url": "https://example.test"},
                "pipeline": [{
                    "phase": "afterResponse",
                    "use": "builtin:assert-body-regex@1",
                    "with": {"pattern": "${secret.pattern}"}
                }]
            }"#,
        )
        .unwrap();
        let inputs = BuildInputs {
            resolver: &resolver,
            store: &store,
            base_dir: root.path(),
            env: empty.clone(),
            matrix: empty.clone(),
            runtime: empty,
            secret: &secret,
        };

        let errors = build_ir(&doc, &inputs).unwrap_err();

        assert!(errors.0[0].message.contains("***"));
        assert!(!format!("{errors:?}").contains("sensitive["));
    }

    #[test]
    fn openapi_file_reference_loads_source_and_defaults_request_identity() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("api.yaml"),
            "openapi: 3.0.3\ninfo: {title: API, version: '1'}\npaths: {}\n",
        )
        .unwrap();
        let project = ProjectConfig::default();
        let resolver = RefResolver::new(root.path(), &project).unwrap();
        let store = DataStore::new(&resolver);
        let empty = Value::Object(Map::new());
        let secret = |_name: &str| None;
        let doc = RequestDocument::parse(
            r#"{
                "formatVersion": 1,
                "kind": "request",
                "meta": {"id": "openapi", "name": "OpenAPI"},
                "request": {"method": "GET", "url": "https://example.test/pets"},
                "pipeline": [{
                    "phase": "afterResponse",
                    "use": "builtin:assert-openapi-response@1",
                    "with": {"specRef": "api.yaml"}
                }]
            }"#,
        )
        .unwrap();
        let inputs = BuildInputs {
            resolver: &resolver,
            store: &store,
            base_dir: root.path(),
            env: empty.clone(),
            matrix: empty.clone(),
            runtime: empty,
            secret: &secret,
        };

        let ir = build_ir(&doc, &inputs).unwrap();
        let input = &ir.pipeline[0].input;
        assert!(input["spec"].as_str().unwrap().contains("openapi: 3.0.3"));
        assert_eq!(input["method"], "GET");
        assert_eq!(input["url"], "https://example.test/pets");
        assert!(input.get("specRef").is_none());
    }
}

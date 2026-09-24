//! Loss-reported request-level export to common external formats.

use serde_json::{json, Value};

use super::model::{BodySpec, BodyType, RequestDocument};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterchangeFormat {
    Postman,
    Bruno,
}

impl InterchangeFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Postman => "postman_collection.json",
            Self::Bruno => "bru",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterchangeExport {
    pub content: String,
    pub warnings: Vec<String>,
}

/// Render one request for another API client and state every v1 feature that
/// cannot be represented in that format. Secret values are never resolved.
pub fn render_interchange_request(
    document: &RequestDocument,
    format: InterchangeFormat,
) -> Result<InterchangeExport, String> {
    let mut warnings = loss_report(document);
    let content = match format {
        InterchangeFormat::Postman => render_postman(document, &mut warnings)?,
        InterchangeFormat::Bruno => render_bruno(document, &mut warnings),
    };
    warnings.sort();
    warnings.dedup();
    Ok(InterchangeExport { content, warnings })
}

fn loss_report(document: &RequestDocument) -> Vec<String> {
    let mut warnings = Vec::new();
    if !document.bindings.is_empty() || !document.matrix.is_empty() {
        warnings.push("Request bindings and matrix cases were not exported.".to_string());
    }
    if !document.pipeline.is_empty() {
        warnings.push(
            "Request pipeline steps and project-owned scripts were not exported.".to_string(),
        );
    }
    if document.mock.is_some() {
        warnings.push("The ApiWright mock definition was not exported.".to_string());
    }
    if !document.meta.tags.is_empty() {
        warnings.push("ApiWright request tags were not exported.".to_string());
    }
    if matches!(document.request.body.as_ref(), Some(BodySpec::Ref(_))) {
        warnings.push("The referenced request body was not inlined or exported.".to_string());
    }
    if document.request.url.contains("${")
        || document
            .request
            .headers
            .iter()
            .chain(document.request.query.iter())
            .any(|entry| entry.name.contains("${") || entry.value.contains("${"))
        || document
            .request
            .body
            .as_ref()
            .is_some_and(request_body_contains_template)
    {
        warnings.push(
            "Project variables are exported as client placeholders; secret values were not exported."
                .to_string(),
        );
    }
    if document.request.body.as_ref().is_some_and(|body| {
        matches!(body, BodySpec::Binary(_))
            || matches!(body, BodySpec::Multipart(multipart) if multipart.parts.iter().any(|part| matches!(part, super::model::MultipartPart::File { .. })))
    }) {
        warnings.push("Uploaded file paths remain local paths and may need adjustment.".to_string());
    }
    warnings
}

fn render_postman(
    document: &RequestDocument,
    warnings: &mut Vec<String>,
) -> Result<String, String> {
    let mut url_query = Vec::new();
    let (base_url, embedded_query, fragment) = split_url_query(&document.request.url);
    for (key, value) in url::form_urlencoded::parse(embedded_query.as_bytes()) {
        url_query.push(json!({"key": export_variables(&key), "value": export_variables(&value), "disabled": false}));
    }
    for item in &document.request.query {
        url_query.push(json!({
            "key": export_variables(&item.name),
            "value": export_variables(&item.value),
            "disabled": !item.enabled,
        }));
    }

    let mut request = json!({
        "method": document.request.method.as_str(),
        "header": document.request.headers.iter().map(|header| json!({
            "key": export_variables(&header.name),
            "value": export_variables(&header.value),
            "disabled": !header.enabled,
        })).collect::<Vec<_>>(),
        "auth": {"type": "noauth"},
        "url": {
            "raw": export_variables(&format!("{base_url}{fragment}")),
            "query": url_query,
        },
    });
    if let Some(description) = &document.meta.description {
        if !description.trim().is_empty() {
            request["description"] = Value::String(description.clone());
        }
    }
    if let Some(body) = export_postman_body(document.request.body.as_ref(), warnings) {
        request["body"] = body;
    }
    let collection = json!({
        "info": {
            "name": format!("{} (ApiWright export)", document.meta.name),
            "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
        },
        "item": [{
            "name": document.meta.name,
            "request": request,
        }]
    });
    serde_json::to_string_pretty(&collection)
        .map(|mut content| {
            content.push('\n');
            content
        })
        .map_err(|error| format!("cannot render Postman collection: {error}"))
}

fn export_postman_body(body: Option<&BodySpec>, warnings: &mut Vec<String>) -> Option<Value> {
    let body = body?;
    match body {
        BodySpec::Ref(_) => {
            warnings.push("The referenced request body was not inlined or exported.".to_string());
            None
        }
        BodySpec::Binary(binary) => Some(json!({
            "mode": "file",
            "file": {"src": binary.file},
        })),
        BodySpec::Multipart(multipart) => {
            let entries = multipart
                .parts
                .iter()
                .map(|part| match part {
                    super::model::MultipartPart::Text {
                        name,
                        value,
                        filename,
                        content_type,
                        enabled,
                    } => {
                        if filename.is_some() {
                            warnings.push(format!(
                                "Multipart text field '{name}' has a filename that Postman cannot preserve."
                            ));
                        }
                        let mut entry = json!({
                            "key": export_variables(name),
                            "value": export_variables(value),
                            "type": "text",
                            "disabled": !enabled,
                        });
                        if let Some(content_type) = content_type {
                            entry["contentType"] = Value::String(content_type.clone());
                        }
                        entry
                    }
                    super::model::MultipartPart::File {
                        name,
                        file,
                        filename,
                        content_type,
                        enabled,
                    } => {
                        if filename.is_some() {
                            warnings.push(format!(
                                "Multipart file field '{name}' uses a custom filename that may need adjustment after export."
                            ));
                        }
                        let mut entry = json!({
                            "key": export_variables(name),
                            "src": file,
                            "type": "file",
                            "disabled": !enabled,
                        });
                        if let Some(content_type) = content_type {
                            entry["contentType"] = Value::String(content_type.clone());
                        }
                        entry
                    }
                })
                .collect::<Vec<_>>();
            Some(json!({"mode": "formdata", "formdata": entries}))
        }
        BodySpec::Inline(body) => {
            let value = body.value.as_ref();
            match body.body_type {
                BodyType::None => None,
                BodyType::Json | BodyType::Text => {
                    let language = if body.body_type == BodyType::Json {
                        "json"
                    } else {
                        "text"
                    };
                    let raw = value
                        .map(export_value)
                        .map(|value| export_scalar(&value))
                        .unwrap_or_default();
                    Some(json!({
                        "mode": "raw",
                        "raw": export_variables(&raw),
                        "options": {"raw": {"language": language}},
                    }))
                }
                BodyType::Form => {
                    let Some(Value::Object(fields)) = value else {
                        warnings.push("The non-object form body was not exported.".to_string());
                        return None;
                    };
                    let entries = fields
                        .iter()
                        .map(|(key, value)| {
                            json!({
                                "key": export_variables(key),
                                "value": export_variables(&export_scalar(value)),
                                "type": "text",
                                "disabled": false,
                            })
                        })
                        .collect::<Vec<_>>();
                    Some(json!({"mode": "urlencoded", "urlencoded": entries}))
                }
            }
        }
    }
}

fn render_bruno(document: &RequestDocument, warnings: &mut Vec<String>) -> String {
    let (base_url, embedded_query, fragment) = split_url_query(&document.request.url);
    let mut output = format!(
        "meta {{\n  name: {}\n  type: http\n  seq: 1\n}}\n\n{} {{\n  url: {}\n  body: {}\n  auth: none\n}}\n",
        bruno_value(&document.meta.name),
        document.request.method.as_str().to_ascii_lowercase(),
        bruno_value(&export_variables(&format!("{base_url}{fragment}"))),
        bruno_body_type(document.request.body.as_ref()),
    );
    if !document.request.headers.is_empty() {
        output.push_str("\nheaders {\n");
        for header in &document.request.headers {
            output.push_str("  ");
            if !header.enabled {
                output.push('~');
            }
            output.push_str(&bruno_value(&export_variables(&header.name)));
            output.push_str(": ");
            output.push_str(&bruno_value(&export_variables(&header.value)));
            output.push('\n');
        }
        output.push_str("}\n");
    }
    let mut query_entries = url::form_urlencoded::parse(embedded_query.as_bytes())
        .map(|(name, value)| (export_variables(&name), export_variables(&value), true))
        .collect::<Vec<_>>();
    query_entries.extend(document.request.query.iter().map(|entry| {
        (
            export_variables(&entry.name),
            export_variables(&entry.value),
            entry.enabled,
        )
    }));
    if !query_entries.is_empty() {
        output.push_str("\nparams:query {\n");
        for (name, value, enabled) in query_entries {
            output.push_str("  ");
            if !enabled {
                output.push('~');
            }
            output.push_str(&bruno_value(&name));
            output.push_str(": ");
            output.push_str(&bruno_value(&value));
            output.push('\n');
        }
        output.push_str("}\n");
    }
    if let Some(description) = &document.meta.description {
        if !description.trim().is_empty() {
            output.push_str("\ndocs {\n");
            for line in description.lines() {
                output.push_str("  ");
                output.push_str(line);
                output.push('\n');
            }
            output.push_str("}\n");
        }
    }
    match document.request.body.as_ref() {
        Some(BodySpec::Inline(body)) => match body.body_type {
            BodyType::None => {}
            BodyType::Json => {
                if let Some(value) = &body.value {
                    output.push_str("\nbody:json {\n");
                    for line in pretty_body(&export_value(value)).lines() {
                        output.push_str("  ");
                        output.push_str(line);
                        output.push('\n');
                    }
                    output.push_str("}\n");
                }
            }
            BodyType::Text => {
                if let Some(value) = &body.value {
                    output.push_str("\nbody:text {\n");
                    for line in export_variables(&export_scalar(value)).lines() {
                        output.push_str("  ");
                        output.push_str(line);
                        output.push('\n');
                    }
                    output.push_str("}\n");
                }
            }
            BodyType::Form => {
                if let Some(Value::Object(fields)) = &body.value {
                    output.push_str("\nbody:form-urlencoded {\n");
                    for (name, value) in fields {
                        output.push_str("  ");
                        output.push_str(&bruno_value(&export_variables(name)));
                        output.push_str(": ");
                        output.push_str(&bruno_value(&export_variables(&export_scalar(
                            &export_value(value),
                        ))));
                        output.push('\n');
                    }
                    output.push_str("}\n");
                } else if body.value.is_some() {
                    warnings.push("The non-object form body was not exported.".to_string());
                }
            }
        },
        Some(BodySpec::Multipart(multipart)) => {
            output.push_str("\nbody:multipart-form {\n");
            for part in &multipart.parts {
                match part {
                    super::model::MultipartPart::Text {
                        name,
                        value,
                        filename,
                        content_type,
                        enabled,
                    } => {
                        if filename.is_some() {
                            warnings.push(format!(
                                "Multipart text field '{name}' has a filename that Bruno cannot preserve."
                            ));
                        }
                        output.push_str("  ");
                        if !enabled {
                            output.push('~');
                        }
                        output.push_str(&bruno_value(&export_variables(name)));
                        output.push_str(": ");
                        output.push_str(&bruno_value(&export_variables(value)));
                        if let Some(content_type) = content_type {
                            output
                                .push_str(&bruno_value(&format!(" @contentType({content_type})")));
                        }
                        output.push('\n');
                    }
                    super::model::MultipartPart::File {
                        name,
                        file,
                        filename,
                        content_type,
                        enabled,
                    } => {
                        if filename.is_some() {
                            warnings.push(format!(
                                "Multipart file field '{name}' has a custom filename that Bruno cannot preserve."
                            ));
                        }
                        output.push_str("  ");
                        if !enabled {
                            output.push('~');
                        }
                        output.push_str(&bruno_value(&export_variables(name)));
                        output.push_str(": ");
                        output.push_str(&bruno_value(&format!("@file({file})")));
                        if let Some(content_type) = content_type {
                            output
                                .push_str(&bruno_value(&format!(" @contentType({content_type})")));
                        }
                        output.push('\n');
                    }
                }
            }
            output.push_str("}\n");
        }
        Some(BodySpec::Binary(binary)) => {
            output.push_str("\nbody:file {\n  file: ");
            output.push_str(&bruno_value(&format!("@file({})", binary.file)));
            output.push_str("\n}\n");
        }
        Some(BodySpec::Ref(_)) => {
            warnings.push("The referenced request body was not inlined or exported.".to_string());
        }
        None => {}
    }
    output
}

fn bruno_body_type(body: Option<&BodySpec>) -> &'static str {
    match body {
        None
        | Some(BodySpec::Inline(super::model::InlineBody {
            body_type: BodyType::None,
            ..
        })) => "none",
        Some(BodySpec::Ref(_)) => "none",
        Some(BodySpec::Inline(body)) => match body.body_type {
            BodyType::None => "none",
            BodyType::Json => "json",
            BodyType::Text => "text",
            BodyType::Form => "form-urlencoded",
        },
        Some(BodySpec::Binary(_)) => "file",
        Some(BodySpec::Multipart(_)) => "multipart-form",
    }
}

fn split_url_query(url: &str) -> (&str, &str, &str) {
    let fragment_index = url.find('#').unwrap_or(url.len());
    let (before_fragment, fragment) = url.split_at(fragment_index);
    match before_fragment.split_once('?') {
        Some((base, query)) => (base, query, fragment),
        None => (before_fragment, "", fragment),
    }
}

fn export_variables(value: &str) -> String {
    use std::sync::OnceLock;
    static ENV: OnceLock<regex::Regex> = OnceLock::new();
    static SECRET: OnceLock<regex::Regex> = OnceLock::new();
    let env = ENV.get_or_init(|| {
        regex::Regex::new(r"\$\{env\.([A-Za-z0-9_-]+)\}").expect("env variable regex is valid")
    });
    let secret = SECRET.get_or_init(|| {
        regex::Regex::new(r"\$\{secret\.([A-Za-z0-9_-]+)\}")
            .expect("secret variable regex is valid")
    });
    secret
        .replace_all(&env.replace_all(value, "{{$1}}"), "{{$1}}")
        .into_owned()
}

fn value_contains_template(value: &Value) -> bool {
    match value {
        Value::String(value) => has_template(value),
        Value::Array(values) => values.iter().any(value_contains_template),
        Value::Object(values) => values.values().any(value_contains_template),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

fn request_body_contains_template(body: &BodySpec) -> bool {
    match body {
        BodySpec::Inline(inline) => inline.value.as_ref().is_some_and(value_contains_template),
        BodySpec::Binary(binary) => has_template(&binary.file),
        BodySpec::Multipart(multipart) => multipart.parts.iter().any(|part| match part {
            super::model::MultipartPart::Text { value, .. } => has_template(value),
            super::model::MultipartPart::File { file, .. } => has_template(file),
        }),
        BodySpec::Ref(reference) => has_template(&reference.reference),
    }
}

fn has_template(value: &str) -> bool {
    value.contains("${") || value.contains("{{")
}

fn export_scalar(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

fn export_value(value: &Value) -> Value {
    match value {
        Value::String(value) => Value::String(export_variables(value)),
        Value::Array(values) => Value::Array(values.iter().map(export_value).collect()),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (export_variables(key), export_value(value)))
                .collect(),
        ),
        value => value.clone(),
    }
}

fn pretty_body(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn bruno_value(value: &str) -> String {
    value.replace('\n', "\\n").replace('\r', "\\r")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> RequestDocument {
        RequestDocument::parse(
            r#"{
                "formatVersion": 1,
                "kind": "request",
                "meta": {"id": "create-item", "name": "Create item", "description": "Creates one item."},
                "request": {
                    "method": "POST",
                    "url": "https://api.example.test/items?region=eu#summary",
                    "headers": [{"name": "Content-Type", "value": "application/json", "enabled": true}],
                    "query": [{"name": "verbose", "value": "true", "enabled": false}],
                    "body": {"type": "json", "value": {"name": "${env.itemName}", "token": "${secret.token}"}}
                }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn postman_export_roundtrips_supported_request_fields_and_reports_secret_loss() {
        let export = render_interchange_request(&request(), InterchangeFormat::Postman).unwrap();
        let imported = crate::convert::parse_postman(&export.content).unwrap();
        let item = imported.items.first().unwrap();
        let crate::convert::ImportedItem::Request(request) = item else {
            panic!("expected one exported request")
        };

        assert_eq!(request.method.as_str(), "POST");
        assert_eq!(request.headers[0].key, "Content-Type");
        assert_eq!(request.params.len(), 2);
        assert!(request.url.contains("#summary"));
        assert!(export.content.contains("{{token}}"));
        assert!(export
            .warnings
            .iter()
            .any(|warning| warning.contains("secret values were not exported")));
    }

    #[test]
    fn bruno_export_uses_importable_request_blocks_and_reports_v1_features() {
        let mut document = request();
        document.pipeline.push(super::super::model::PipelineEntry {
            phase: super::super::model::PipelinePhase::AfterResponse,
            uses: "builtin:assert-status@1".to_string(),
            with: Default::default(),
            enabled: true,
        });
        let export = render_interchange_request(&document, InterchangeFormat::Bruno).unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("bruno.json"),
            r#"{"version":"1","name":"Export"}"#,
        )
        .unwrap();
        std::fs::write(root.path().join("collection.bru"), "vars {\n}\n").unwrap();
        std::fs::write(root.path().join("create-item.bru"), &export.content).unwrap();
        let imported = crate::convert::import_bruno(root.path()).unwrap();

        assert_eq!(imported.collection.request_count(), 1);
        let crate::convert::ImportedItem::Request(request) = &imported.collection.items[0] else {
            panic!("expected one exported request")
        };
        assert_eq!(request.params.len(), 2);
        assert!(request.url.contains("#summary"));
        assert!(!request.url.contains("?region=eu"));
        assert!(export
            .warnings
            .iter()
            .any(|warning| warning.contains("pipeline steps")));
    }
}

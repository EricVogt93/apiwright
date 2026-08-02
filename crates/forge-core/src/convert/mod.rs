//! Import/export: curl command parsing and generation, Postman collection /
//! environment import, plus code snippets (JS fetch, axios, Python requests,
//! HTTPie, Go, Java HttpClient).

mod bruno;
mod common;
mod curl_in;
mod curl_out;
mod open_collection;
mod postman;
mod quarantine;
mod snippets;

pub use bruno::{import_bruno, parse_bruno_environment, BrunoError, BrunoImport};
pub use curl_in::{parse_curl, CurlParseError};
pub use curl_out::{to_curl, CurlExportOptions};
pub use open_collection::{
    detect_bruno_source, import_bruno_v1, BrunoSourceFormat, BrunoV1ImportError,
    BrunoV1ImportOptions, BrunoV1ImportReport, ImportedEnvironmentSummary,
};
pub use postman::{
    parse_postman, parse_postman_environment, ImportedCollection, ImportedItem, PostmanError,
};
pub use quarantine::{
    load_import_quarantine, merge_import_quarantine, quarantine_path, save_import_quarantine,
    sync_import_quarantine, ImportQuarantineEntry, ImportQuarantineManifest, ImportSourceFormat,
    QuarantineCategory, QuarantineDisposition, IMPORT_QUARANTINE_PATH,
};
pub use snippets::{generate, SnippetLang};

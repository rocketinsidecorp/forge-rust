//! OpenAPI 3.1 documents from typed Forge routes (ADR 0039).
//!
//! Descriptions are opt-in and travel with the route's handler
//! (`forge::openapi::describe`, `forge::openapi::typed`); this crate is the
//! transport-free core: the [`OpenApiSchema`] trait (derived next to
//! `Serialize`), [`Operation`] descriptions, the deterministic
//! [`Document`] and the contract check [`Document::validate_response`]. It
//! has no HTTP dependency.
//!
//! ```
//! use forge_openapi::{Document, Info, OpenApiSchema, Operation, Route};
//!
//! #[derive(serde::Serialize, OpenApiSchema)]
//! #[openapi(crate = "forge_openapi")]
//! struct Status {
//!     service: String,
//!     status: String,
//! }
//!
//! let document = Document::build(
//!     &Info::new("shop", "0.1.0"),
//!     &[Route {
//!         method: "GET".to_owned(),
//!         path: "/v1/status".to_owned(),
//!         operation: Some(Operation::new().summary("Service status").json::<Status>(200).into()),
//!     }],
//! )?;
//! assert_eq!(document.paths(), ["/v1/status"]);
//! document.validate_response(
//!     "GET",
//!     "/v1/status",
//!     200,
//!     Some("application/json"),
//!     br#"{"service":"shop","status":"ok"}"#,
//! )?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

mod document;
mod json;
mod members;
mod operation;
mod schema;
mod status;
mod validate;

pub use document::{
    BuildError, Document, Info, JSON_SCHEMA_DIALECT, Route, SESSION_COOKIE, SESSION_SCHEME,
    VERSION, schema_of,
};
/// `#[derive(OpenApiSchema)]`: the schema of a struct or unit-only enum from
/// its fields and serde attributes.
pub use forge_macros::OpenApiSchema;
pub use members::unknown_member;
pub use operation::{Operation, Param, ParamLocation, Response, TypeRef};
pub use schema::{
    Field, MapKey, ObjectSchema, OpenApiSchema, Schema, SchemaGenerator, UUID_V7_PATTERN, non_null,
    null_schema, nullable, string_enum,
};
pub use validate::ContractError;

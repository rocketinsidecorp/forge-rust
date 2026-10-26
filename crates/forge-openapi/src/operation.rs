//! What one route declares: its operation, parameters, request and
//! responses.

use std::any::type_name;
use std::fmt;

use crate::schema::{OpenApiSchema, Schema, SchemaGenerator};

/// A Rust type whose schema the document includes. The zero value of
/// Forge-Go's `openapi.Type` is `Option<TypeRef>::None` here.
#[derive(Clone, Copy)]
pub struct TypeRef {
    name: &'static str,
    schema: fn(&mut SchemaGenerator) -> Schema,
}

impl TypeRef {
    /// The type `T`.
    #[must_use]
    pub fn of<T: OpenApiSchema + ?Sized>() -> Self {
        Self {
            name: type_name::<T>(),
            schema: T::schema,
        }
    }

    /// The Rust type's name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    pub(crate) fn schema(&self, generator: &mut SchemaGenerator) -> Schema {
        (self.schema)(generator)
    }
}

impl fmt::Debug for TypeRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name)
    }
}

/// Where a parameter is.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ParamLocation {
    /// A request header.
    Header,
    /// A `{name}` segment of the path.
    Path,
    /// A query string parameter.
    Query,
}

impl ParamLocation {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Header => "header",
            Self::Path => "path",
            Self::Query => "query",
        }
    }
}

/// A path, query or header parameter.
#[derive(Clone, Debug)]
pub struct Param {
    pub(crate) location: ParamLocation,
    pub(crate) name: String,
    pub(crate) schema: TypeRef,
    pub(crate) required: bool,
    pub(crate) description: Option<String>,
    pub(crate) example: Option<String>,
    pub(crate) format: Option<String>,
}

impl Param {
    fn new<T: OpenApiSchema + ?Sized>(location: ParamLocation, name: &str) -> Self {
        Self {
            location,
            name: name.to_owned(),
            schema: TypeRef::of::<T>(),
            required: false,
            description: None,
            example: None,
            format: None,
        }
    }

    /// The path segment `{name}` parsed as `T`; always required. A name the
    /// route's path lacks is a build error.
    #[must_use]
    pub fn path<T: OpenApiSchema + ?Sized>(name: &str) -> Self {
        Self::new::<T>(ParamLocation::Path, name)
    }

    /// The query parameter `name` parsed as `T`; optional unless
    /// [`Param::required`].
    #[must_use]
    pub fn query<T: OpenApiSchema + ?Sized>(name: &str) -> Self {
        Self::new::<T>(ParamLocation::Query, name)
    }

    /// The request header `name`; optional unless [`Param::required`].
    #[must_use]
    pub fn header<T: OpenApiSchema + ?Sized>(name: &str) -> Self {
        Self::new::<T>(ParamLocation::Header, name)
    }

    /// The parameter must be present.
    #[must_use]
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// The parameter's description.
    #[must_use]
    pub fn doc(mut self, text: impl Into<String>) -> Self {
        self.description = Some(text.into());
        self
    }

    /// An example value (JSON, or text).
    #[must_use]
    pub fn example(mut self, text: impl Into<String>) -> Self {
        self.example = Some(text.into());
        self
    }

    /// The value's `format`.
    #[must_use]
    pub fn format(mut self, format: impl Into<String>) -> Self {
        self.format = Some(format.into());
        self
    }
}

/// What a response carries.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Body {
    /// `application/json` of a Rust type.
    Json(TypeRef),
    /// `text/plain`: what Forge's transport and guards answer on their own.
    Text,
}

/// One documented response of an operation.
#[derive(Clone, Debug)]
pub struct Response {
    pub(crate) status: u16,
    pub(crate) description: Option<String>,
    pub(crate) body: Option<Body>,
    pub(crate) headers: Vec<String>,
}

impl Response {
    /// A response with a JSON body of type `T`.
    #[must_use]
    pub fn json<T: OpenApiSchema + ?Sized>(status: u16) -> Self {
        Self {
            status,
            description: None,
            body: Some(Body::Json(TypeRef::of::<T>())),
            headers: Vec::new(),
        }
    }

    /// A response without a body, such as `204 No Content`.
    #[must_use]
    pub fn empty(status: u16) -> Self {
        Self {
            status,
            description: None,
            body: None,
            headers: Vec::new(),
        }
    }

    /// A `text/plain` response.
    #[must_use]
    pub fn text(status: u16) -> Self {
        Self {
            status,
            description: None,
            body: Some(Body::Text),
            headers: Vec::new(),
        }
    }

    /// The description; defaults to the status text, such as "Created".
    #[must_use]
    pub fn description(mut self, text: impl Into<String>) -> Self {
        self.description = Some(text.into());
        self
    }

    /// A response header the operation sets, such as `Location`.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>) -> Self {
        self.headers.push(name.into());
        self
    }

    /// The status.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.status
    }
}

/// Describes one route. Build it with the methods below; every field is
/// optional except at least one response.
///
/// ```
/// use forge_openapi::{Operation, Param, Response};
/// # #[derive(forge_openapi::OpenApiSchema)]
/// # #[openapi(crate = "forge_openapi")]
/// # struct Post { title: String }
/// # #[derive(forge_openapi::OpenApiSchema)]
/// # #[openapi(crate = "forge_openapi")]
/// # struct NewPost { title: String }
///
/// let create = Operation::new()
///     .summary("Create a post")
///     .tag("posts")
///     .permission("posts:write")
///     .request::<NewPost>()
///     .response(Response::json::<Post>(201).header("Location"))
///     .errors([409]);
/// let delete = Operation::new()
///     .param(Param::path::<forge_core::Uuid>("id").doc("The post."))
///     .param(Param::query::<i64>("version").required())
///     .response(Response::empty(204));
/// ```
#[derive(Clone, Debug, Default)]
pub struct Operation {
    pub(crate) id: Option<String>,
    pub(crate) summary: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) tags: Vec<String>,
    pub(crate) deprecated: bool,
    pub(crate) permission: Option<String>,
    pub(crate) authenticated: bool,
    pub(crate) bearer: bool,
    pub(crate) params: Vec<Param>,
    pub(crate) request: Option<TypeRef>,
    pub(crate) responses: Vec<Response>,
    pub(crate) errors: Vec<u16>,
    pub(crate) invalid: Option<String>,
}

impl Operation {
    /// An empty description.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The `operationId`; defaults to the method and path, such as
    /// `getV1PostsById`. IDs must be unique.
    #[must_use]
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// A one-line summary.
    #[must_use]
    pub fn summary(mut self, text: impl Into<String>) -> Self {
        self.summary = Some(text.into());
        self
    }

    /// A longer description.
    #[must_use]
    pub fn description(mut self, text: impl Into<String>) -> Self {
        self.description = Some(text.into());
        self
    }

    /// Adds a tag.
    #[must_use]
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.push(tag.into());
        self
    }

    /// Marks the operation deprecated.
    #[must_use]
    pub fn deprecated(mut self) -> Self {
        self.deprecated = true;
        self
    }

    /// The permission the route requires (`x-forge-permission`). It implies
    /// the session cookie and the `401` and `403` refusals.
    #[must_use]
    pub fn permission(mut self, permission: impl Into<String>) -> Self {
        self.permission = Some(permission.into());
        self
    }

    /// The route needs a signed-in session but no specific permission; it
    /// adds the `401` refusal.
    #[must_use]
    pub fn authenticated(mut self) -> Self {
        self.authenticated = true;
        self
    }

    /// The route authenticates an API client's `Authorization: Bearer`
    /// token (security scheme `bearerAuth`) instead of the session cookie,
    /// and needs a signed-in client: it adds the `401` refusal, and with a
    /// [`permission`](Self::permission) the `403`, both as JSON error
    /// reports.
    #[must_use]
    pub fn bearer(mut self) -> Self {
        self.bearer = true;
        self.authenticated = true;
        self
    }

    /// Adds a path, query or header parameter. Path parameters the route
    /// declares are documented as strings without one.
    #[must_use]
    pub fn param(mut self, param: Param) -> Self {
        self.params.push(param);
        self
    }

    /// The JSON request body, decoded with `forge::web::decode_json`. It adds
    /// the `413` (too large), `415` (`unsupported_media_type`: the
    /// `Content-Type` is not `application/json`) and `422` (`invalid_json`)
    /// errors.
    #[must_use]
    pub fn request<T: OpenApiSchema + ?Sized>(mut self) -> Self {
        self.request = Some(TypeRef::of::<T>());
        self
    }

    /// A response with its own body type, or none: successes, redirects, and
    /// errors that are not an `ErrorReport`.
    #[must_use]
    pub fn response(mut self, response: Response) -> Self {
        self.responses.push(response);
        self
    }

    /// `response(Response::json::<T>(status))`.
    #[must_use]
    pub fn json<T: OpenApiSchema + ?Sized>(self, status: u16) -> Self {
        self.response(Response::json::<T>(status))
    }

    /// Error statuses the route answers with `forge::web::error` (an
    /// `ErrorReport`), which every error response and the `default`
    /// response use.
    #[must_use]
    pub fn errors(mut self, statuses: impl IntoIterator<Item = u16>) -> Self {
        self.errors.extend(statuses);
        self
    }

    /// Puts `response` first, replacing any response with its status: the
    /// typed handler's own success.
    #[must_use]
    pub fn with_success(mut self, response: Response) -> Self {
        self.responses
            .retain(|existing| existing.status != response.status);
        self.responses.insert(0, response);
        self
    }

    /// Marks the description as wrong, so building the document fails with
    /// `reason` (for example a typed handler given a status without a body).
    #[must_use]
    pub fn invalid(mut self, reason: impl Into<String>) -> Self {
        self.invalid.get_or_insert_with(|| reason.into());
        self
    }

    /// The request body type, if any.
    #[must_use]
    pub fn request_type(&self) -> Option<TypeRef> {
        self.request
    }
}

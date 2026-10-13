//! Automatic error-response generation.
//!
//! NestJS handlers declare only their success return type; the `400`/`401`/
//! `403`/`404`/… responses come from guards, pipes, validation, and thrown
//! exceptions that never appear in the signature. This module reconstructs them
//! from route-level signals gathered in `routes.rs` plus the `errorResponses`
//! config, in two tiers:
//!
//! - **Tier A** (deterministic, auto-emitted): `401` for guarded routes, `403`
//!   for `@Roles`, `400` for validated request bodies, and pipe-mapped
//!   `404`/`400`. All are visible in the controller signature.
//! - **Tier B** (declared): service-thrown codes a controller can't reveal —
//!   the `@throws` JSDoc tag and config `routes[].add` / `global`.
//!
//! Every error response shares one body shape, the `{ code, message, status }`
//! object emitted by `@algoan`'s global `HttpExceptionFilter`, registered once
//! as the `Error` component and referenced by every error response.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::config::{ErrorResponses, GlobalError, RouteErrors};

/// Component-schema name for the shared error body.
pub const ERROR_SCHEMA_NAME: &str = "Error";

/// The literal error-body schema emitted by the global `HttpExceptionFilter`
/// (`{ code, message, status }`). Not NestJS's stock `{ statusCode, message,
/// error }` — keyed to this filter, confirmed by both the filter source and
/// every in-code `throw` passing this shape.
pub fn error_schema() -> Value {
    json!({
        "type": "object",
        "description": "Error response emitted by the global HttpExceptionFilter.",
        "properties": {
            "code": { "type": "string" },
            "message": { "type": "string" },
            "status": { "type": "integer" }
        },
        "required": ["code", "message", "status"]
    })
}

/// Raw, AST-derived signals for one concrete route path. `routes.rs` gathers
/// these (they need the swc AST); the mapping to status codes lives here so it
/// stays pure and unit-testable.
pub struct RouteSignals<'a> {
    /// Authentication applies to this route (global scope, or a matching
    /// `@UseGuards(AuthGuard)` in per-controller scope) — before opt-out.
    pub auth_active: bool,
    /// The route or its controller carries an auth opt-out decorator (e.g.
    /// `@Unprotected()`).
    pub auth_opted_out: bool,
    /// The route or its controller carries a role decorator (e.g. `@Roles`).
    pub has_roles: bool,
    /// The route has a request-body (`@Body`) parameter.
    pub has_body: bool,
    /// Pipe class names applied to the route's `@Param`/`@Query` parameters.
    pub pipes: &'a [String],
    /// `@throws` entries from the method's JSDoc (status, optional description).
    pub throws: &'a [(u16, Option<String>)],
    /// Status codes found by scanning the handler body for `throw new
    /// <HttpException>(...)` (already mapped via `exception_status`). Applied
    /// only when `scan_throws` is on.
    pub scanned_throws: &'a [u16],
    /// The route's HTTP method, lowercased (e.g. `get`), for `routes[].method`
    /// gating.
    pub method: &'a str,
    /// The route's full OpenAPI path, for `global`/`routes` suffix gating.
    pub path: &'a str,
}

/// Compute the error responses for one route as a `status -> description` map.
/// The map is ordered (`BTreeMap`) so output is deterministic, and dedupes by
/// status: the first source to claim a code keeps its (more specific)
/// description. Sources are inserted most-specific-first for that reason.
pub fn compute(sig: &RouteSignals, cfg: &ErrorResponses) -> BTreeMap<u16, String> {
    let mut errors: BTreeMap<u16, String> = BTreeMap::new();
    if !cfg.enabled {
        return errors;
    }
    let mut insert = |code: u16, desc: &str| {
        errors.entry(code).or_insert_with(|| desc.to_string());
    };

    // Tier A — deterministic from the controller signature.
    if sig.auth_active && !sig.auth_opted_out {
        insert(401, "Authentication required");
    }
    if sig.has_roles {
        insert(403, "Insufficient permissions");
    }
    if cfg.validation && sig.has_body {
        insert(400, "Validation failed");
    }
    for pipe in sig.pipes {
        if let Some(&code) = cfg.pipes.get(pipe) {
            insert(code, status_text(code));
        }
    }

    // Filter-level codes (e.g. 413), optionally gated on a path suffix.
    for g in &cfg.global {
        if !global_matches(g, sig.path) {
            continue;
        }
        let desc = g
            .description
            .as_deref()
            .unwrap_or_else(|| status_text(g.status));
        insert(g.status, desc);
    }

    // Errors thrown directly in the handler body (detected from the controller
    // source). Service-thrown errors are invisible here — those come from
    // `@throws` / config below.
    if cfg.scan_throws {
        for &code in sig.scanned_throws {
            insert(code, status_text(code));
        }
    }

    // Tier B — declared service-thrown errors.
    for (code, desc) in sig.throws {
        insert(*code, desc.as_deref().unwrap_or_else(|| status_text(*code)));
    }
    for ro in &cfg.routes {
        if route_matches(ro, sig.method, sig.path) {
            for &code in &ro.add {
                insert(code, status_text(code));
            }
        }
    }

    errors
}

fn global_matches(g: &GlobalError, path: &str) -> bool {
    g.when_path_ends_with
        .as_ref()
        .is_none_or(|suffix| path_ends_with(path, suffix))
}

fn route_matches(ro: &RouteErrors, method: &str, path: &str) -> bool {
    let path_matches = match (&ro.when_path_ends_with, &ro.path) {
        (Some(suffix), _) => path_ends_with(path, suffix),
        (None, Some(p)) => path == crate::routes::openapi_params(p),
        (None, None) => false,
    };
    path_matches
        && ro
            .method
            .as_ref()
            .is_none_or(|m| m.eq_ignore_ascii_case(method))
}

/// Warn about each `errorResponses` rule that applies to none of `routes`
/// (`(method, path)` pairs), which is usually a path written without the
/// global prefix.
pub fn warn_unmatched<'a>(
    cfg: &ErrorResponses,
    routes: impl Iterator<Item = (&'a str, &'a str)> + Clone,
) {
    if !cfg.enabled {
        return;
    }
    for g in &cfg.global {
        if !routes.clone().any(|(_, path)| global_matches(g, path)) {
            crate::diag::warn(format!(
                "errorResponses.global {} with whenPathEndsWith {:?} matches no route",
                g.status,
                g.when_path_ends_with.as_deref().unwrap_or_default()
            ));
        }
    }
    for ro in &cfg.routes {
        if !routes
            .clone()
            .any(|(method, path)| route_matches(ro, method, path))
        {
            let target = match (&ro.when_path_ends_with, &ro.path) {
                (Some(suffix), _) => format!("whenPathEndsWith {suffix:?}"),
                (None, Some(p)) => format!("path {p:?} (full path, prefix included)"),
                (None, None) => "with neither path nor whenPathEndsWith".to_string(),
            };
            crate::diag::warn(format!(
                "errorResponses.routes entry {target} matches no route"
            ));
        }
    }
}

/// Default human description for a status code, used when no source supplies a
/// more specific one.
pub fn status_text(code: u16) -> &'static str {
    match code {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        410 => "Gone",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

/// NestJS built-in `HttpException` subclass name -> the HTTP status it carries.
/// Used by the handler-body throw scan. `InternalServerErrorException` (500) is
/// deliberately absent: `500` is a processing failure, never emitted by default.
/// A generic `throw new HttpException(body, status)` is not mapped (its status
/// is a runtime value, not the class name).
pub fn exception_status(class_name: &str) -> Option<u16> {
    Some(match class_name {
        "BadRequestException" => 400,
        "UnauthorizedException" => 401,
        "PaymentRequiredException" => 402,
        "ForbiddenException" => 403,
        "NotFoundException" => 404,
        "MethodNotAllowedException" => 405,
        "NotAcceptableException" => 406,
        "RequestTimeoutException" => 408,
        "ConflictException" => 409,
        "GoneException" => 410,
        "PreconditionFailedException" => 412,
        "PayloadTooLargeException" => 413,
        "UnsupportedMediaTypeException" => 415,
        "ImATeapotException" => 418,
        "UnprocessableEntityException" => 422,
        "TooManyRequestsException" => 429,
        "BadGatewayException" => 502,
        "ServiceUnavailableException" => 503,
        "GatewayTimeoutException" => 504,
        _ => return None,
    })
}

/// NestJS's `HttpStatus` enum (`@nestjs/common` 10.x): member name -> status.
/// Used for `@HttpCode(HttpStatus.<MEMBER>)` when `@nestjs/common`'s own
/// declarations are not in the symbol table.
pub fn nest_http_status(member: &str) -> Option<u16> {
    Some(match member {
        "CONTINUE" => 100,
        "SWITCHING_PROTOCOLS" => 101,
        "PROCESSING" => 102,
        "EARLYHINTS" => 103,
        "OK" => 200,
        "CREATED" => 201,
        "ACCEPTED" => 202,
        "NON_AUTHORITATIVE_INFORMATION" => 203,
        "NO_CONTENT" => 204,
        "RESET_CONTENT" => 205,
        "PARTIAL_CONTENT" => 206,
        "AMBIGUOUS" => 300,
        "MOVED_PERMANENTLY" => 301,
        "FOUND" => 302,
        "SEE_OTHER" => 303,
        "NOT_MODIFIED" => 304,
        "TEMPORARY_REDIRECT" => 307,
        "PERMANENT_REDIRECT" => 308,
        "BAD_REQUEST" => 400,
        "UNAUTHORIZED" => 401,
        "PAYMENT_REQUIRED" => 402,
        "FORBIDDEN" => 403,
        "NOT_FOUND" => 404,
        "METHOD_NOT_ALLOWED" => 405,
        "NOT_ACCEPTABLE" => 406,
        "PROXY_AUTHENTICATION_REQUIRED" => 407,
        "REQUEST_TIMEOUT" => 408,
        "CONFLICT" => 409,
        "GONE" => 410,
        "LENGTH_REQUIRED" => 411,
        "PRECONDITION_FAILED" => 412,
        "PAYLOAD_TOO_LARGE" => 413,
        "URI_TOO_LONG" => 414,
        "UNSUPPORTED_MEDIA_TYPE" => 415,
        "REQUESTED_RANGE_NOT_SATISFIABLE" => 416,
        "EXPECTATION_FAILED" => 417,
        "I_AM_A_TEAPOT" => 418,
        "MISDIRECTED" => 421,
        "UNPROCESSABLE_ENTITY" => 422,
        "FAILED_DEPENDENCY" => 424,
        "PRECONDITION_REQUIRED" => 428,
        "TOO_MANY_REQUESTS" => 429,
        "INTERNAL_SERVER_ERROR" => 500,
        "NOT_IMPLEMENTED" => 501,
        "BAD_GATEWAY" => 502,
        "SERVICE_UNAVAILABLE" => 503,
        "GATEWAY_TIMEOUT" => 504,
        "HTTP_VERSION_NOT_SUPPORTED" => 505,
        _ => return None,
    })
}

/// Whether `path` ends with `suffix` on a path-segment boundary (mirrors the
/// `@route-mapping` suffix rule in `routes.rs`). Leading/trailing slashes on
/// the suffix are ignored; an empty suffix never matches.
fn path_ends_with(path: &str, suffix: &str) -> bool {
    let suffix = crate::routes::openapi_params(suffix);
    let suffix = suffix.trim_matches('/');
    if suffix.is_empty() {
        return false;
    }
    let path = path.trim_end_matches('/');
    path == suffix || path.ends_with(&format!("/{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals<'a>(path: &'a str) -> RouteSignals<'a> {
        RouteSignals {
            auth_active: false,
            auth_opted_out: false,
            has_roles: false,
            has_body: false,
            pipes: &[],
            throws: &[],
            scanned_throws: &[],
            method: "get",
            path,
        }
    }

    #[test]
    fn route_override_method_filter() {
        let mut cfg = ErrorResponses::default();
        cfg.routes.push(RouteErrors {
            when_path_ends_with: Some("/customers".into()),
            path: None,
            method: Some("post".into()),
            add: vec![422],
        });
        let post = RouteSignals {
            method: "post",
            ..signals("/v2/customers")
        };
        let get = RouteSignals {
            method: "get",
            ..signals("/v2/customers")
        };
        assert!(compute(&post, &cfg).contains_key(&422));
        assert!(!compute(&get, &cfg).contains_key(&422)); // GET sibling untouched
    }

    #[test]
    fn scanned_throws_add_codes_and_respect_toggle() {
        let scanned = [422u16, 404u16];
        let sig = RouteSignals {
            scanned_throws: &scanned,
            ..signals("/x/{id}/scores")
        };
        let on = compute(&sig, &ErrorResponses::default());
        assert_eq!(on[&422], "Unprocessable Entity");
        assert!(on.contains_key(&404));
        // Toggle off.
        let cfg = ErrorResponses {
            scan_throws: false,
            ..ErrorResponses::default()
        };
        assert!(compute(&sig, &cfg).is_empty());
    }

    #[test]
    fn tier_a_auth_role_validation_and_pipes() {
        let cfg = ErrorResponses::default();
        let pipes = vec!["ParseObjectIdPipe".to_string()];
        let sig = RouteSignals {
            auth_active: true,
            has_roles: true,
            has_body: true,
            pipes: &pipes,
            ..signals("/v2/customers/{id}")
        };
        let errors = compute(&sig, &cfg);
        assert_eq!(
            errors.keys().copied().collect::<Vec<_>>(),
            vec![400, 401, 403, 404]
        );
        assert_eq!(errors[&404], "Not Found"); // ParseObjectIdPipe -> 404, not 400
        assert_eq!(errors[&400], "Validation failed");
    }

    #[test]
    fn opt_out_suppresses_401() {
        let cfg = ErrorResponses::default();
        let sig = RouteSignals {
            auth_active: true,
            auth_opted_out: true,
            ..signals("/health")
        };
        assert!(compute(&sig, &cfg).is_empty());
    }

    #[test]
    fn no_auth_when_scope_inactive() {
        let cfg = ErrorResponses::default();
        // auth_active is false regardless of scope; nothing else present.
        assert!(compute(&signals("/public"), &cfg).is_empty());
    }

    #[test]
    fn enum_pipe_maps_to_400() {
        let cfg = ErrorResponses::default();
        let pipes = vec!["ParseEnumPipe".to_string()];
        let sig = RouteSignals {
            pipes: &pipes,
            ..signals("/x/{name}")
        };
        assert_eq!(
            compute(&sig, &cfg).keys().copied().collect::<Vec<_>>(),
            vec![400]
        );
    }

    #[test]
    fn throws_and_route_overrides_add_tier_b() {
        let mut cfg = ErrorResponses::default();
        cfg.routes.push(RouteErrors {
            when_path_ends_with: Some("/aggregate".into()),
            path: None,
            method: None,
            add: vec![409],
        });
        let throws = vec![(422u16, Some("product disabled".to_string()))];
        let sig = RouteSignals {
            throws: &throws,
            ..signals("/connect/aggregate")
        };
        let errors = compute(&sig, &cfg);
        assert_eq!(errors[&422], "product disabled");
        assert_eq!(errors[&409], "Conflict");
    }

    #[test]
    fn global_error_gated_on_suffix() {
        let mut cfg = ErrorResponses::default();
        cfg.global.push(GlobalError {
            status: 413,
            when_path_ends_with: Some("/logo".into()),
            description: None,
        });
        let hit = RouteSignals {
            has_body: true,
            ..signals("/orgs/{id}/logo")
        };
        let miss = RouteSignals {
            has_body: true,
            ..signals("/orgs/{id}")
        };
        assert!(compute(&hit, &cfg).contains_key(&413));
        assert!(!compute(&miss, &cfg).contains_key(&413));
    }

    #[test]
    fn disabled_emits_nothing() {
        let cfg = ErrorResponses {
            enabled: false,
            ..ErrorResponses::default()
        };
        let sig = RouteSignals {
            auth_active: true,
            has_roles: true,
            ..signals("/x")
        };
        assert!(compute(&sig, &cfg).is_empty());
    }

    #[test]
    fn validation_can_be_turned_off() {
        let cfg = ErrorResponses {
            validation: false,
            ..ErrorResponses::default()
        };
        let sig = RouteSignals {
            has_body: true,
            ..signals("/x")
        };
        assert!(compute(&sig, &cfg).is_empty());
    }

    #[test]
    fn configured_paths_accept_express_params() {
        let ro = RouteErrors {
            when_path_ends_with: None,
            path: Some("/v2/customers/:id".into()),
            method: None,
            add: vec![409],
        };
        assert!(route_matches(&ro, "get", "/v2/customers/{id}"));
        let suffix = RouteErrors {
            when_path_ends_with: Some(":id/aggregate".into()),
            path: None,
            ..ro
        };
        assert!(route_matches(
            &suffix,
            "post",
            "/v2/customers/{id}/aggregate"
        ));
        let in_segment = RouteErrors {
            when_path_ends_with: None,
            path: Some("/files/:name.:ext".into()),
            ..suffix
        };
        assert!(route_matches(&in_segment, "get", "/files/{name}.{ext}"));
    }

    #[test]
    fn rules_that_match_no_route_warn() {
        crate::diag::take_warnings();
        let cfg = ErrorResponses {
            global: vec![GlobalError {
                status: 413,
                when_path_ends_with: Some("/logo".into()),
                description: None,
            }],
            routes: vec![
                RouteErrors {
                    when_path_ends_with: None,
                    path: Some("/customers".into()),
                    method: Some("post".into()),
                    add: vec![409],
                },
                RouteErrors {
                    when_path_ends_with: None,
                    path: Some("/v2/customers".into()),
                    method: Some("post".into()),
                    add: vec![409],
                },
            ],
            ..ErrorResponses::default()
        };
        let routes = [("post", "/v2/customers"), ("get", "/v2/customers")];
        warn_unmatched(&cfg, routes.iter().copied());
        let warnings = crate::diag::take_warnings();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("413"), "{warnings:?}");
        assert!(warnings[1].contains("\"/customers\""), "{warnings:?}");
    }
}

//! `network.http` extension capability adapter.

use crate::client::{Client, ClientError, HttpOutcome, HttpRequest};
use crate::route::{HttpMethod, RouteError};
use std::collections::BTreeMap;
use tea_core::harness::extension::{
    ExtensionCapability, ExtensionCapabilityError, ExtensionCapabilityFuture,
    ExtensionCapabilityRequest, ExtensionCapabilityResponse,
};
use tea_core::scheduler::CancellationToken;
use tea_protocol::{JsonNumber, JsonValue};

/// The explicit generic capability granted to provider-policy extensions. It
/// exposes only `request` and `request_many`; the route table remains owned by
/// trusted host composition.
#[derive(Clone)]
pub struct NetworkHttpCapability {
    client: Client,
}

impl NetworkHttpCapability {
    /// Bind one shared client to the capability surface.
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    /// Stable identities of the host route policies behind this capability.
    pub fn route_identities(&self) -> Vec<String> {
        self.client.route_identities()
    }
}

impl ExtensionCapability for NetworkHttpCapability {
    fn invoke(
        &self,
        request: ExtensionCapabilityRequest,
        cancellation: CancellationToken,
    ) -> ExtensionCapabilityFuture {
        let client = self.client.clone();
        match request.method.as_str() {
            "request" => match parse_request(&request.arguments) {
                Ok(http_request) => Box::pin(async move {
                    client
                        .request(http_request, cancellation)
                        .await
                        .map(|outcome| ExtensionCapabilityResponse {
                            value: outcome_json(outcome),
                        })
                        .map_err(capability_error)
                }),
                Err(error) => Box::pin(async move { Err(error) }),
            },
            "request_many" => match parse_many(&request.arguments) {
                Ok(http_requests) => Box::pin(async move {
                    client
                        .request_many(http_requests, cancellation)
                        .await
                        .map(|outcomes| ExtensionCapabilityResponse {
                            value: JsonValue::Array(
                                outcomes.into_iter().map(outcome_json).collect(),
                            ),
                        })
                        .map_err(capability_error)
                }),
                Err(error) => Box::pin(async move { Err(error) }),
            },
            method => {
                let method = method.to_owned();
                Box::pin(async move {
                    Err(ExtensionCapabilityError::MethodDenied {
                        capability: "network.http".into(),
                        method,
                    })
                })
            }
        }
    }
}

fn parse_many(arguments: &JsonValue) -> Result<Vec<HttpRequest>, ExtensionCapabilityError> {
    let object = object(arguments, "network.http request_many arguments")?;
    exact_fields(object, &["requests", "response"])?;
    response_is_json(object)?;
    let requests = required(object, "requests")?
        .as_array()
        .ok_or_else(|| invalid("requests must be an array"))?;
    if requests.is_empty() {
        return Err(invalid("requests must not be empty"));
    }
    requests.iter().map(parse_request).collect()
}

fn parse_request(arguments: &JsonValue) -> Result<HttpRequest, ExtensionCapabilityError> {
    let object = object(arguments, "network.http request arguments")?;
    exact_fields(object, &["route", "method", "path", "query", "json", "response"])?;
    response_is_json(object)?;
    let route = required_string(object, "route")?;
    let method = required_string(object, "method")?;
    let method = HttpMethod::parse(&method)
        .ok_or_else(|| invalid("method must be the supported literal POST"))?;
    let path = required_string(object, "path")?;
    if path.is_empty() {
        return Err(invalid("path must not be empty"));
    }
    let query = match object.get("query") {
        None => BTreeMap::new(),
        Some(value) => parse_query(value)?,
    };
    let json = object.get("json").cloned();
    match (method, json.is_some()) {
        (HttpMethod::Post, false) => return Err(invalid("POST requests require a JSON body")),
        (HttpMethod::Get, true) => return Err(invalid("GET requests must not include a JSON body")),
        _ => {}
    }
    Ok(HttpRequest {
        route,
        method,
        path,
        query,
        json,
    })
}

fn parse_query(value: &JsonValue) -> Result<BTreeMap<String, String>, ExtensionCapabilityError> {
    let query = value
        .as_object()
        .ok_or_else(|| invalid("query must be an object with string values"))?;
    query
        .iter()
        .map(|(key, value)| {
            value
                .as_str()
                .map(|value| (key.clone(), value.to_owned()))
                .ok_or_else(|| invalid("query values must be strings"))
        })
        .collect()
}

fn response_is_json(
    object: &BTreeMap<String, JsonValue>,
) -> Result<(), ExtensionCapabilityError> {
    if required_string(object, "response")? != "json" {
        return Err(invalid("response must be the supported literal json"));
    }
    Ok(())
}

fn object<'a>(
    value: &'a JsonValue,
    name: &str,
) -> Result<&'a BTreeMap<String, JsonValue>, ExtensionCapabilityError> {
    value
        .as_object()
        .ok_or_else(|| invalid(&format!("{name} must be an object")))
}

fn required<'a>(
    object: &'a BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<&'a JsonValue, ExtensionCapabilityError> {
    object
        .get(field)
        .ok_or_else(|| invalid(&format!("missing required field {field:?}")))
}

fn required_string(
    object: &BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<String, ExtensionCapabilityError> {
    required(object, field)?
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| invalid(&format!("field {field:?} must be a string")))
}

fn exact_fields(
    object: &BTreeMap<String, JsonValue>,
    expected: &[&str],
) -> Result<(), ExtensionCapabilityError> {
    if let Some(field) = object
        .keys()
        .find(|field| !expected.iter().any(|expected| *expected == field.as_str()))
    {
        return Err(invalid(&format!("unexpected field {field:?}")));
    }
    Ok(())
}

fn invalid(message: &str) -> ExtensionCapabilityError {
    ExtensionCapabilityError::InvalidArguments {
        message: message.into(),
    }
}

fn capability_error(error: ClientError) -> ExtensionCapabilityError {
    match error {
        ClientError::Route(RouteError::ForbiddenRequest { method, path }) => {
            ExtensionCapabilityError::MethodDenied {
                capability: "network.http".into(),
                method: format!("{method} {path}"),
            }
        }
        error => ExtensionCapabilityError::InvalidArguments {
            message: error.to_string(),
        },
    }
}

fn outcome_json(outcome: HttpOutcome) -> JsonValue {
    match outcome {
        HttpOutcome::Response {
            status,
            attempts,
            headers,
            json,
        } => JsonValue::object([
            ("kind", JsonValue::String("response".into())),
            (
                "status",
                JsonValue::Number(JsonNumber::Unsigned(status.into())),
            ),
            (
                "attempts",
                JsonValue::Number(JsonNumber::Unsigned(attempts.into())),
            ),
            (
                "headers",
                JsonValue::Object(
                    headers
                        .into_iter()
                        .map(|(name, value)| (name, JsonValue::String(value)))
                        .collect(),
                ),
            ),
            ("json", json),
        ]),
        HttpOutcome::TransportError {
            code,
            attempts,
            message,
        } => JsonValue::object([
            ("kind", JsonValue::String("transport_error".into())),
            ("code", JsonValue::String(code.as_str().into())),
            (
                "attempts",
                JsonValue::Number(JsonNumber::Unsigned(attempts.into())),
            ),
            ("message", JsonValue::String(message)),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_request_shape_rejects_an_arbitrary_origin() {
        let result = parse_request(&JsonValue::object([
            ("route", JsonValue::String("firecrawl".into())),
            ("method", JsonValue::String("POST".into())),
            ("path", JsonValue::String("/v2/search".into())),
            ("url", JsonValue::String("https://outside.example".into())),
            ("json", JsonValue::Object(BTreeMap::new())),
            ("response", JsonValue::String("json".into())),
        ]));
        assert!(matches!(
            result,
            Err(ExtensionCapabilityError::InvalidArguments { .. })
        ));
    }

    #[test]
    fn outcome_keeps_a_non_success_http_response_structured() {
        let json = outcome_json(HttpOutcome::Response {
            status: 429,
            attempts: 2,
            headers: BTreeMap::from([("retry-after".into(), "1".into())]),
            json: JsonValue::Object(BTreeMap::new()),
        });
        assert_eq!(json.get("kind").and_then(JsonValue::as_str), Some("response"));
        assert_eq!(json.get("status").and_then(JsonValue::as_u64), Some(429));
    }

    #[test]
    fn get_request_accepts_only_string_query_parameters_without_a_json_body() {
        let request = parse_request(&JsonValue::object([
            ("route", JsonValue::String("tinyfish-search".into())),
            ("method", JsonValue::String("GET".into())),
            ("path", JsonValue::String("/".into())),
            (
                "query",
                JsonValue::object([("query", JsonValue::String("Rust & HTTP/2".into()))]),
            ),
            ("response", JsonValue::String("json".into())),
        ]))
        .expect("a fixed GET query is valid");
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(request.query.get("query").map(String::as_str), Some("Rust & HTTP/2"));
        assert!(request.json.is_none());

        let invalid = parse_request(&JsonValue::object([
            ("route", JsonValue::String("tinyfish-search".into())),
            ("method", JsonValue::String("GET".into())),
            ("path", JsonValue::String("/".into())),
            ("json", JsonValue::Object(BTreeMap::new())),
            ("response", JsonValue::String("json".into())),
        ]));
        assert!(matches!(invalid, Err(ExtensionCapabilityError::InvalidArguments { .. })));
    }

    #[test]
    fn batch_requests_retain_each_request_response_contract() {
        let requests = parse_many(&JsonValue::object([
            (
                "requests",
                JsonValue::Array(vec![JsonValue::object([
                    ("route", JsonValue::String("firecrawl".into())),
                    ("method", JsonValue::String("POST".into())),
                    ("path", JsonValue::String("/v2/scrape".into())),
                    ("json", JsonValue::Object(BTreeMap::new())),
                    ("response", JsonValue::String("json".into())),
                ])]),
            ),
            ("response", JsonValue::String("json".into())),
        ]))
        .expect("each batched request carries its JSON response contract");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].json, Some(JsonValue::Object(BTreeMap::new())));
    }
}

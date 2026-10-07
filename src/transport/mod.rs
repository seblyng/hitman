use std::{
    collections::HashMap, fs::read_to_string, path::Path, sync::Arc,
    time::Duration,
};

use anyhow::Result;
use reqwest::Client;
use serde_json::Value;

use crate::{
    resolve::{Resolved, ResolvedAs},
    substitute::{self, SubstituteProvider},
};

pub mod graphql;
pub mod grpc;
pub mod http;

#[derive(Clone)]
pub enum PreparedRequest {
    Http(http::HttpRequest),
    GraphQL(graphql::GraphQLRequest),
    Grpc(grpc::GrpcRequest),
}

pub struct ExecutionResult {
    pub elapsed: Duration,
    pub json: Option<Value>,
}

pub struct TransportResponse {
    pub elapsed: Duration,
    response: TransportResponseBody,
}

enum TransportResponseBody {
    Http(reqwest::Response),
    GraphQL(reqwest::Response),
    Grpc(Value),
}

pub fn prepare_request(
    resolved: &Resolved,
    provider: Arc<dyn SubstituteProvider + Send + Sync + 'static>,
) -> Result<PreparedRequest> {
    let input = read_to_string(resolved.http_file())?;
    let working_dir = resolved.http_file().parent().unwrap_or(Path::new("."));
    let (template, local_values) = parse_local_variables(&input)?;
    let rendered = substitute::substitute_with_locals(
        &template,
        provider.clone(),
        working_dir,
        local_values,
    )?;

    match &resolved.resolved_as {
        ResolvedAs::Simple { .. } if grpc::is_grpc_request(&rendered) => Ok(
            PreparedRequest::Grpc(grpc::prepare_request(resolved, &rendered)?),
        ),
        ResolvedAs::Simple { .. } => {
            Ok(PreparedRequest::Http(http::prepare_request(&rendered)?))
        }
        ResolvedAs::GraphQL { .. } => Ok(PreparedRequest::GraphQL(
            graphql::prepare_request(resolved, &rendered, provider)?,
        )),
    }
}

fn parse_local_variables(
    input: &str,
) -> Result<(String, HashMap<String, minijinja::Value>)> {
    let mut values = HashMap::new();
    let mut body_start = 0;
    let mut declarations_started = false;

    for line in input.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let declaration = trimmed.split_once('#').map_or(trimmed, |(before, _)| before).trim_end();
        if declaration.starts_with('@') {
            declarations_started = true;
            let (key, value) = declaration[1..]
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("Invalid variable declaration `{declaration}`; expected `@name = value`"))?;
            let key = key.trim();
            if key.is_empty()
                || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                anyhow::bail!(
                    "Invalid variable name `{key}` in declaration `{declaration}`"
                );
            }
            values.insert(
                key.to_string(),
                minijinja::Value::from(value.trim().to_string()),
            );
            body_start += line.len();
        } else if declarations_started && trimmed.trim().is_empty() {
            body_start += line.len();
        } else {
            break;
        }
    }

    Ok((input[body_start..].to_string(), values))
}

#[cfg(test)]
mod tests {
    use super::parse_local_variables;

    #[test]
    fn parses_leading_local_variables_and_removes_them_from_request() {
        let (body, values) = parse_local_variables(
            "@base_url = https://example.com\n@user_id = 42\n\nGET {{base_url}}/users/{{user_id}} HTTP/1.1\n",
        ).unwrap();

        assert_eq!(body, "GET {{base_url}}/users/{{user_id}} HTTP/1.1\n");
        assert_eq!(values["base_url"].as_str(), Some("https://example.com"));
        assert_eq!(values["user_id"].as_str(), Some("42"));
    }

    #[test]
    fn ignores_trailing_comments_on_local_variable_declarations() {
        let (body, values) = parse_local_variables(
            "@base_url = https://example.com # service URL\n\nGET {{base_url}} HTTP/1.1\n",
        )
        .unwrap();

        assert_eq!(values["base_url"].as_str(), Some("https://example.com"));
        assert_eq!(body, "GET {{base_url}} HTTP/1.1\n");
    }

    #[test]
    fn leaves_request_unchanged_without_leading_declarations() {
        let input =
            "GET https://example.com HTTP/1.1\n@not_a_declaration = value\n";
        let (body, values) = parse_local_variables(input).unwrap();

        assert_eq!(body, input);
        assert!(values.is_empty());
    }

    #[test]
    fn rejects_malformed_leading_declaration() {
        assert!(
            parse_local_variables("@missing_equals\nGET / HTTP/1.1\n").is_err()
        );
    }
}

pub async fn send(
    client: &Client,
    request: &PreparedRequest,
) -> Result<TransportResponse> {
    match request {
        PreparedRequest::Http(req) => {
            let (response, elapsed) = http::send(client, req).await?;
            Ok(TransportResponse {
                elapsed,
                response: TransportResponseBody::Http(response),
            })
        }
        PreparedRequest::GraphQL(req) => {
            let (response, elapsed) = graphql::send(client, req).await?;
            Ok(TransportResponse {
                elapsed,
                response: TransportResponseBody::GraphQL(response),
            })
        }
        PreparedRequest::Grpc(req) => {
            let (json, elapsed) = grpc::send(req).await?;
            Ok(TransportResponse {
                elapsed,
                response: TransportResponseBody::Grpc(json),
            })
        }
    }
}

pub async fn finish_response(
    response: TransportResponse,
) -> Result<ExecutionResult> {
    let json = match response.response {
        TransportResponseBody::Http(response) => {
            http::finish_response(response).await?
        }
        TransportResponseBody::GraphQL(response) => {
            graphql::finish_response(response).await?
        }
        TransportResponseBody::Grpc(json) => grpc::finish_response(&json)?,
    };

    Ok(ExecutionResult {
        elapsed: response.elapsed,
        json,
    })
}

pub fn print_request(request: &PreparedRequest) {
    match request {
        PreparedRequest::Http(req) => http::print_request(req),
        PreparedRequest::GraphQL(req) => graphql::print_request(req),
        PreparedRequest::Grpc(req) => grpc::print_request(req),
    }
}

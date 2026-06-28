use std::{fs::read_to_string, sync::Arc, time::Duration};

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
    let rendered = substitute::substitute(&input, provider.clone())?;

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

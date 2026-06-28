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

pub async fn execute(
    client: &Client,
    request: &PreparedRequest,
) -> Result<ExecutionResult> {
    match request {
        PreparedRequest::Http(req) => http::execute(client, req).await,
        PreparedRequest::GraphQL(req) => graphql::execute(client, req).await,
        PreparedRequest::Grpc(req) => grpc::execute(req).await,
    }
}

pub fn print_request(request: &PreparedRequest) {
    match request {
        PreparedRequest::Http(req) => http::print_request(req),
        PreparedRequest::GraphQL(req) => graphql::print_request(req),
        PreparedRequest::Grpc(req) => grpc::print_request(req),
    }
}

use std::{fmt::Display, path::Path, sync::Arc};

use anyhow::{bail, Result};
use graphql_parser::query::{
    Definition, OperationDefinition, VariableDefinition,
};
use reqwest::{header::CONTENT_TYPE, Client};
use serde_json::{json, Value};

use crate::{
    resolve::{Resolved, ResolvedAs},
    substitute::{SubstituteProvider, SubstituteValue},
};

use super::http;

#[derive(Clone)]
pub struct GraphQLRequest {
    pub http: http::HttpRequest,
}

pub struct GraphQLVariable {
    pub name: String,
    pub list: bool,
}

pub fn prepare_request(
    resolved: &Resolved,
    rendered_wrapper: &str,
    provider: Arc<dyn SubstituteProvider + Send + Sync + 'static>,
) -> Result<GraphQLRequest> {
    let ResolvedAs::GraphQL { graphql_path, .. } = &resolved.resolved_as else {
        bail!("Resolved request is not GraphQL");
    };

    let query = std::fs::read_to_string(graphql_path)?;
    let args = find_args(graphql_path)?;
    let body = if args.is_empty() {
        json!({"query": query}).to_string()
    } else {
        json!({
            "query": query,
            "variables": resolve_variables(args, provider)?,
        })
        .to_string()
    };

    let mut http = http::prepare_request(rendered_wrapper)?;
    http.body = Some(http::HttpBody::new(body));

    Ok(GraphQLRequest { http })
}

pub async fn send(
    client: &Client,
    req: &GraphQLRequest,
) -> Result<(reqwest::Response, std::time::Duration)> {
    http::do_request(client, &req.http).await
}

pub async fn finish_response(
    response: reqwest::Response,
) -> Result<Option<Value>> {
    http::print_response(&response)?;

    if let Some(content_type) = response.headers().get(CONTENT_TYPE) {
        if content_type.to_str()?.contains("text/event-stream") {
            parse_stream_output(response).await?;
            return Ok(None);
        }
    }

    let json = response.json::<Value>().await.ok();
    if let Some(json) = &json {
        println!("{}", serde_json::to_string_pretty(json)?);
    }

    Ok(json)
}

pub fn print_request(req: &GraphQLRequest) {
    http::print_request(&req.http);
}

async fn parse_stream_output(response: reqwest::Response) -> Result<()> {
    use futures::StreamExt;
    use std::str::FromStr;

    let mut stream = response.bytes_stream();
    while let Some(Ok(item)) = stream.next().await {
        let s = std::str::from_utf8(&item)?;
        if let (Some(start), Some(end)) = (s.find("data:"), s.find("\n\n")) {
            let data_str = s[start + 5..end].trim();
            let output = match serde_json::Value::from_str(data_str) {
                Ok(json) => &serde_json::to_string_pretty(&json)?,
                Err(_) => data_str,
            };
            println!("{output}");
        }
    }
    Ok(())
}

fn resolve_variables(
    args: Vec<GraphQLVariable>,
    provider: Arc<dyn SubstituteProvider + Send + Sync + 'static>,
) -> Result<Value> {
    let mut map = serde_json::Map::new();

    for key in args {
        let value = match provider.lookup_value(&key.name) {
            None => serde_json::to_value(provider.prompt(&key.name, None)?),
            Some(SubstituteValue::Single(value)) => serde_json::to_value(value),
            Some(SubstituteValue::Multiple(values)) => {
                serde_json::to_value(values)
            }
        }?;

        map.insert(key.name, value);
    }

    Ok(Value::Object(map))
}

pub fn find_args<P>(path: P) -> Result<Vec<GraphQLVariable>>
where
    P: AsRef<Path>,
{
    let file = std::fs::read_to_string(path)?;
    let doc = graphql_parser::parse_query::<String>(&file)?;

    let variables = |vars: &[VariableDefinition<String>]| {
        vars.iter()
            .map(|d| GraphQLVariable {
                name: d.name.clone(),
                list: matches!(
                    d.var_type,
                    graphql_parser::query::Type::ListType(_)
                ),
            })
            .collect::<Vec<_>>()
    };

    let args = match doc.definitions.first() {
        Some(Definition::Operation(ref op)) => match op {
            OperationDefinition::Query(q) => variables(&q.variable_definitions),
            OperationDefinition::Mutation(m) => {
                variables(&m.variable_definitions)
            }
            OperationDefinition::SelectionSet(_) => bail!("Not supported"),
            OperationDefinition::Subscription(s) => {
                variables(&s.variable_definitions)
            }
        },
        _ => bail!("Not supported"),
    };

    Ok(args)
}

impl Display for GraphQLRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.http.fmt(f)
    }
}

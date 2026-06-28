use std::{
    fmt::Display,
    path::{Component, PathBuf},
    str::FromStr,
    time::Instant,
};

use anyhow::{bail, Context, Result};
use http::uri::PathAndQuery;
use log::{info, log_enabled, Level};
use prost::Message;
use prost_reflect::{
    DescriptorPool, DynamicMessage, MessageDescriptor, MethodDescriptor,
    SerializeOptions,
};
use serde_json::Value;
use tonic::{
    codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder},
    metadata::{AsciiMetadataKey, MetadataValue},
    transport::Endpoint,
    Request, Status,
};

use crate::{resolve::Resolved, util::truncate};

const PROTO_HEADER: &str = "proto";

#[derive(Clone)]
pub struct GrpcRequest {
    pub endpoint: String,
    pub path: String,
    pub proto_path: PathBuf,
    pub metadata: Vec<(String, String)>,
    pub body: String,
}

impl Display for GrpcRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "GRPC {}{}", self.endpoint, self.path)?;
        writeln!(f, "Proto: {}", self.proto_path.display())?;
        for (key, val) in &self.metadata {
            writeln!(f, "{key}: {val}")?;
        }

        if !self.body.trim().is_empty() {
            writeln!(f)?;
            for line in self.body.lines() {
                writeln!(f, "{line}")?;
            }
        }

        Ok(())
    }
}

pub fn is_grpc_request(rendered: &str) -> bool {
    rendered
        .lines()
        .find(|line| !line.trim().is_empty())
        .is_some_and(|line| line.trim_start().starts_with("GRPC "))
}

pub fn prepare_request(
    resolved: &Resolved,
    rendered: &str,
) -> Result<GrpcRequest> {
    let (head, body) = split_head_body(rendered);
    let mut lines = head.lines().filter(|line| !line.trim().is_empty());
    let request_line = lines
        .next()
        .context("Invalid gRPC request: missing request line")?;
    let target = request_line
        .trim()
        .strip_prefix("GRPC")
        .context("Invalid gRPC request: request must start with GRPC")?
        .trim();

    if target.is_empty() {
        bail!("Invalid gRPC request: missing target");
    }

    let (endpoint, path) = parse_target(target)?;

    let mut proto_path = None;
    let mut metadata = Vec::new();

    for line in lines {
        let (key, value) = line
            .split_once(':')
            .with_context(|| format!("Invalid gRPC metadata line: {line}"))?;
        let key = key.trim();
        let value = value.trim();

        if key.eq_ignore_ascii_case(PROTO_HEADER) {
            proto_path = Some(resolve_proto_path(resolved, value));
        } else {
            metadata.push((key.to_ascii_lowercase(), value.to_string()));
        }
    }

    let proto_path = proto_path.context(
        "Invalid gRPC request: missing Proto header pointing to a .proto file",
    )?;

    Ok(GrpcRequest {
        endpoint,
        path,
        proto_path,
        metadata,
        body: body.to_string(),
    })
}

pub async fn send(req: &GrpcRequest) -> Result<(Value, std::time::Duration)> {
    let method = load_method(req)?;
    let request_desc = method.input();
    let response_desc = method.output();

    let body = if req.body.trim().is_empty() {
        "{}"
    } else {
        req.body.trim()
    };
    let mut deserializer = serde_json::Deserializer::from_str(body);
    let message = DynamicMessage::deserialize(request_desc, &mut deserializer)?;
    deserializer.end()?;

    let endpoint = Endpoint::from_shared(req.endpoint.clone())?;
    let channel = endpoint.connect().await?;
    let mut client = tonic::client::Grpc::new(channel);
    client
        .ready()
        .await
        .map_err(|err| anyhow::anyhow!("gRPC service was not ready: {err}"))?;

    let path = PathAndQuery::from_str(&req.path)?;
    let mut request = Request::new(message);
    for (key, value) in &req.metadata {
        let key = AsciiMetadataKey::from_str(key)?;
        let value = MetadataValue::try_from(value.as_str())?;
        request.metadata_mut().insert(key, value);
    }

    let t = Instant::now();
    let response = client
        .unary(request, path, DynamicCodec::new(response_desc))
        .await?;
    let elapsed = t.elapsed();

    let response_message = response.into_inner();
    let json = serialize_message(&response_message)?;

    Ok((json, elapsed))
}

pub fn finish_response(json: &Value) -> Result<Option<Value>> {
    println!("{}", serde_json::to_string_pretty(json)?);
    Ok(Some(json.clone()))
}

struct DynamicCodec {
    response: MessageDescriptor,
}

impl DynamicCodec {
    fn new(response: MessageDescriptor) -> Self {
        Self { response }
    }
}

impl Codec for DynamicCodec {
    type Encode = DynamicMessage;
    type Decode = DynamicMessage;
    type Encoder = DynamicEncoder;
    type Decoder = DynamicDecoder;

    fn encoder(&mut self) -> Self::Encoder {
        DynamicEncoder
    }

    fn decoder(&mut self) -> Self::Decoder {
        DynamicDecoder {
            response: self.response.clone(),
        }
    }
}

struct DynamicEncoder;

impl Encoder for DynamicEncoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn encode(
        &mut self,
        item: Self::Item,
        dst: &mut EncodeBuf<'_>,
    ) -> Result<(), Self::Error> {
        item.encode(dst)
            .map_err(|err| Status::internal(err.to_string()))
    }
}

struct DynamicDecoder {
    response: MessageDescriptor,
}

impl Decoder for DynamicDecoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn decode(
        &mut self,
        src: &mut DecodeBuf<'_>,
    ) -> Result<Option<Self::Item>, Self::Error> {
        DynamicMessage::decode(self.response.clone(), src)
            .map(Some)
            .map_err(|err| Status::internal(err.to_string()))
    }
}

pub fn print_request(req: &GrpcRequest) {
    if log_enabled!(Level::Info) {
        for line in req.to_string().lines() {
            info!("> {}", truncate(line));
        }
    }
}

fn split_head_body(input: &str) -> (&str, &str) {
    if let Some((head, body)) = input.split_once("\r\n\r\n") {
        (head, body)
    } else if let Some((head, body)) = input.split_once("\n\n") {
        (head, body)
    } else {
        (input, "")
    }
}

fn parse_target(target: &str) -> Result<(String, String)> {
    let normalized = if let Some(rest) = target.strip_prefix("grpc://") {
        format!("http://{rest}")
    } else if let Some(rest) = target.strip_prefix("grpcs://") {
        format!("https://{rest}")
    } else if target.starts_with("http://") || target.starts_with("https://") {
        target.to_string()
    } else {
        format!("http://{target}")
    };

    let uri = http::Uri::from_str(&normalized)?;
    let scheme = uri
        .scheme_str()
        .context("Invalid gRPC target: missing scheme")?;
    let authority = uri
        .authority()
        .context("Invalid gRPC target: missing host")?;
    let path = uri.path();

    if path == "/" || path.is_empty() {
        bail!("Invalid gRPC target: missing /package.Service/Method path");
    }

    Ok((format!("{scheme}://{authority}"), path.to_string()))
}

fn resolve_proto_path(resolved: &Resolved, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        normalize_path(
            resolved
                .original_path()
                .parent()
                .unwrap_or(&resolved.root_dir)
                .join(path),
        )
    }
}

fn normalize_path(path: PathBuf) -> PathBuf {
    let mut out = PathBuf::new();

    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            _ => out.push(component.as_os_str()),
        }
    }

    out
}

fn load_method(req: &GrpcRequest) -> Result<MethodDescriptor> {
    let import_path = req
        .proto_path
        .parent()
        .context("Proto file must have a parent directory")?;
    let file_descriptors =
        protox::compile([req.proto_path.as_path()], [import_path])?;
    let pool = DescriptorPool::from_file_descriptor_set(file_descriptors)?;

    let (service_name, method_name) = req
        .path
        .trim_start_matches('/')
        .rsplit_once('/')
        .context("Invalid gRPC path: expected /package.Service/Method")?;
    let service = pool
        .get_service_by_name(service_name)
        .with_context(|| format!("gRPC service not found: {service_name}"))?;
    let method = service
        .methods()
        .find(|method| method.name() == method_name)
        .with_context(|| format!("gRPC method not found: {method_name}"))?;

    Ok(method)
}

fn serialize_message(message: &DynamicMessage) -> Result<Value> {
    let mut buf = Vec::new();
    let mut serializer = serde_json::Serializer::new(&mut buf);
    let options = SerializeOptions::new();
    message.serialize_with_options(&mut serializer, &options)?;
    Ok(serde_json::from_slice(&buf)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::{Resolved, ResolvedAs};
    use mktemp::Temp;
    use std::fs;

    fn resolved(path: &str) -> Resolved {
        Resolved {
            root_dir: PathBuf::from("/tmp/project").into_boxed_path(),
            resolved_as: ResolvedAs::Simple {
                path: PathBuf::from(path).into_boxed_path(),
            },
        }
    }

    #[test]
    fn detects_grpc_request() {
        assert!(is_grpc_request("GRPC localhost:50051/foo.Bar/Baz\n"));
        assert!(!is_grpc_request("GET http://example.com\n"));
    }

    #[test]
    fn parses_grpc_request() {
        let request = prepare_request(
            &resolved("/tmp/project/requests/get_user.http"),
            r#"GRPC localhost:50051/example.UserService/GetUser
Proto: ../proto/user.proto
Authorization: Bearer abc

{"id":"123"}
"#,
        )
        .unwrap();

        assert_eq!(request.endpoint, "http://localhost:50051");
        assert_eq!(request.path, "/example.UserService/GetUser");
        assert_eq!(
            request.proto_path,
            PathBuf::from("/tmp/project/proto/user.proto")
        );
        assert_eq!(
            request.metadata,
            vec![("authorization".into(), "Bearer abc".into())]
        );
        assert_eq!(request.body.trim(), r#"{"id":"123"}"#);
    }

    #[test]
    fn parses_grpcs_target() {
        let (endpoint, path) =
            parse_target("grpcs://example.com/package.Service/Method").unwrap();

        assert_eq!(endpoint, "https://example.com");
        assert_eq!(path, "/package.Service/Method");
    }

    #[test]
    fn loads_method_from_proto() {
        let tmp = Temp::new_dir().unwrap();
        let proto = tmp.join("user.proto");
        fs::write(
            &proto,
            r#"
                syntax = "proto3";
                package example;

                service UserService {
                    rpc GetUser (GetUserRequest) returns (GetUserResponse);
                }

                message GetUserRequest {
                    string id = 1;
                }

                message GetUserResponse {
                    string name = 1;
                }
            "#,
        )
        .unwrap();

        let request = GrpcRequest {
            endpoint: "http://localhost:50051".into(),
            path: "/example.UserService/GetUser".into(),
            proto_path: proto,
            metadata: vec![],
            body: r#"{"id":"123"}"#.into(),
        };

        let method = load_method(&request).unwrap();

        assert_eq!(method.name(), "GetUser");
        assert_eq!(method.input().full_name(), "example.GetUserRequest");
        assert_eq!(method.output().full_name(), "example.GetUserResponse");
    }
}

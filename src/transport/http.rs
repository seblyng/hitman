use std::{
    fmt::Display, path::Path, str, str::FromStr, sync::Arc, time::Duration,
};

use anyhow::{Context, Result};
use httparse::Status;
use log::{info, log_enabled, Level};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Client, Method, Response, Url,
};
use serde_json::Value;
use std::fmt::Write;

use crate::{env::HitmanCookieJar, util::truncate};

#[derive(Clone)]
pub struct HttpBody {
    body: String,
}

impl HttpBody {
    pub fn new(body: String) -> Self {
        Self { body }
    }

    pub fn into_string(self) -> String {
        self.body
    }
}

impl Display for HttpBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.body)
    }
}

#[derive(Clone)]
pub struct HttpRequest {
    pub headers: HeaderMap,
    pub url: Url,
    pub method: Method,
    pub body: Option<HttpBody>,
}

impl Display for HttpRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{} {}", self.method.as_str(), self.url.as_str())?;
        for (key, val) in &self.headers {
            writeln!(
                f,
                "{}: {}",
                key.as_str(),
                val.to_str().unwrap_or("unknown value")
            )?;
        }

        if let Some(ref body) = self.body {
            writeln!(f)?;
            for line in body.to_string().lines() {
                writeln!(f, "{line}")?;
            }
        }
        write!(f, "")
    }
}

static USER_AGENT: &str =
    concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"),);

pub fn build_client(root_dir: &Path) -> Result<Client> {
    let client = Client::builder()
        .user_agent(USER_AGENT)
        .cookie_provider(Arc::new(HitmanCookieJar::new(root_dir)))
        .build()?;
    Ok(client)
}

pub fn prepare_request(input: &str) -> Result<HttpRequest> {
    let mut headers_buf = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers_buf);

    let parse_result = req
        .parse(input.as_bytes())
        .context("Invalid input: malformed request")?;

    let method = req.method.context("Invalid input: HTTP method not found")?;
    let url = req.path.context("Invalid input: URL not found")?;

    let method = Method::from_str(method)?;
    let url = Url::parse(url)?;

    let body = match parse_result {
        Status::Complete(offset) => {
            Some(HttpBody::new(input[offset..].to_string()))
        }
        Status::Partial => None,
    };

    let headers = parse_headers(&req)?;

    Ok(HttpRequest {
        headers,
        url,
        method,
        body,
    })
}

pub fn parse_headers(req: &httparse::Request<'_, '_>) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();

    for header in req.headers.iter() {
        if header.name.is_empty() {
            break;
        }
        let value = str::from_utf8(header.value)?;
        let header_name = HeaderName::from_str(header.name)?;
        let header_value = HeaderValue::from_str(value)?;
        headers.insert(header_name, header_value);
    }

    Ok(headers)
}

pub async fn send(
    client: &Client,
    req: &HttpRequest,
) -> Result<(Response, Duration)> {
    do_request(client, req).await
}

pub async fn finish_response(response: Response) -> Result<Option<Value>> {
    print_response(&response)?;

    let json = response.json::<Value>().await.ok();
    if let Some(json) = &json {
        println!("{}", serde_json::to_string_pretty(json)?);
    }

    Ok(json)
}

pub async fn do_request(
    client: &Client,
    req: &HttpRequest,
) -> Result<(Response, Duration)> {
    let mut builder = client.request(req.method.clone(), req.url.clone());
    builder = builder.headers(req.headers.clone());
    if let Some(ref body) = req.body {
        builder = builder.body(body.clone().into_string());
    }

    let t = std::time::Instant::now();
    let response = builder.send().await?;

    let elapsed = t.elapsed();

    Ok((response, elapsed))
}

pub fn print_request(req: &HttpRequest) {
    if log_enabled!(Level::Info) {
        for line in req.to_string().lines() {
            info!("> {}", truncate(line));
        }
    }
}

pub fn print_response(res: &Response) -> Result<()> {
    if log_enabled!(Level::Info) {
        let status = res.status();
        info!(
            "< HTTP/1.1 {} {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("")
        );

        let mut head = String::new();
        for (name, value) in res.headers() {
            writeln!(head, "{}: {}", name, value.to_str()?)?;
        }

        for line in head.lines() {
            info!("< {}", truncate(line));
        }

        info!("");
    }

    Ok(())
}

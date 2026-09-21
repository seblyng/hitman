use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    net::{IpAddr, SocketAddr},
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use minijinja::Environment;
use oauth2::{
    basic::{BasicClient, BasicTokenResponse},
    reqwest, AuthType, AuthUrl, AuthorizationCode, ClientId, ClientSecret,
    CsrfToken, PkceCodeChallenge, RedirectUrl, RefreshToken, Scope,
    TokenResponse, TokenUrl,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    time::timeout,
};
use toml::{Table, Value};

use crate::{
    env::{read_and_merge_config, read_data, update_data},
    prompt::is_interactive_mode,
    resolve::{Resolved, ResolvedAs},
    scope::{Replacement, Scope as HitmanScope},
};

const OAUTH_CONFIG_KEY: &str = "_oauth";
const OAUTH_STATE_KEY: &str = "_oauth_tokens";
const EXPIRY_MARGIN_SECONDS: i64 = 30;
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Flow {
    AuthorizationCode,
    ClientCredentials,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OAuthProfile {
    flow: Flow,
    token_url: String,
    client_id: String,
    client_secret: Option<String>,
    authorization_url: Option<String>,
    redirect_uri: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    authorization_params: BTreeMap<String, String>,
    #[serde(default)]
    token_params: BTreeMap<String, String>,
    #[serde(default)]
    client_auth_method: ClientAuthMethod,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ClientAuthMethod {
    #[default]
    Basic,
    RequestBody,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TokenState {
    access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    token_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scopes: Vec<String>,
}

impl TokenState {
    fn is_valid(&self) -> bool {
        self.expires_at.is_none_or(|expiry| {
            expiry > unix_timestamp() + EXPIRY_MARGIN_SECONDS
        })
    }
}

pub async fn resolve_scope(
    resolved: &Resolved,
    target: &str,
    scope: &HitmanScope,
) -> Result<HitmanScope> {
    let profiles = load_profiles(&resolved.root_dir)?;
    if profiles.is_empty() {
        return Ok(scope.clone());
    }

    let mut referenced = referenced_variables(resolved.http_file())?;
    if let ResolvedAs::GraphQL { graphql_path, .. } = &resolved.resolved_as {
        referenced.extend(
            crate::transport::graphql::find_args(graphql_path)?
                .into_iter()
                .map(|variable| variable.name),
        );
    }
    let requested = profiles
        .iter()
        .filter(|(name, _)| referenced.contains(name.as_str()))
        .collect::<Vec<_>>();

    if requested.is_empty() {
        return Ok(scope.clone());
    }

    let http_client = reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut resolved_scope = scope.clone();

    for (variable, profile) in requested {
        if !matches!(scope.lookup(variable)?, Replacement::ValueNotFound { .. })
        {
            continue;
        }

        let token = resolve_token(
            &resolved.root_dir,
            target,
            variable,
            profile,
            &http_client,
        )
        .await
        .with_context(|| format!("OAuth profile `{variable}`"))?;
        resolved_scope.insert(variable.clone(), token.access_token);
    }

    Ok(resolved_scope)
}

fn load_profiles(root_dir: &Path) -> Result<HashMap<String, OAuthProfile>> {
    let config = read_and_merge_config(root_dir)?;
    let profiles = match config.get(OAUTH_CONFIG_KEY) {
        None => return Ok(HashMap::new()),
        Some(Value::Table(profiles)) => profiles,
        Some(_) => bail!("`{OAUTH_CONFIG_KEY}` must be a table"),
    };

    profiles
        .iter()
        .map(|(name, value)| {
            let profile = value
                .clone()
                .try_into::<OAuthProfile>()
                .with_context(|| format!("Invalid OAuth profile `{name}`"))?;
            Ok((name.clone(), profile))
        })
        .collect()
}

fn referenced_variables(path: &Path) -> Result<HashSet<String>> {
    let base_dir = path.parent().unwrap_or(Path::new("."));
    let mut visited = HashSet::new();
    referenced_variables_in(path, base_dir, &mut visited)
}

fn referenced_variables_in(
    path: &Path,
    base_dir: &Path,
    visited: &mut HashSet<std::path::PathBuf>,
) -> Result<HashSet<String>> {
    if !visited.insert(path.to_path_buf()) {
        return Ok(HashSet::new());
    }

    let input = fs::read_to_string(path)
        .with_context(|| format!("When reading template {path:?}"))?;
    let env = Environment::new();
    let template = env.template_from_str(&input)?;
    let mut variables = template.undeclared_variables(false);

    for include in static_includes(&input) {
        variables.extend(referenced_variables_in(
            &base_dir.join(include),
            base_dir,
            visited,
        )?);
    }

    Ok(variables)
}

fn static_includes(input: &str) -> Vec<&str> {
    let mut includes = Vec::new();
    let mut remaining = input;
    while let Some(start) = remaining.find("{%").map(|index| index + 2) {
        remaining = &remaining[start..];
        let Some(end) = remaining.find("%}") else {
            break;
        };
        let tag = remaining[..end].trim();
        remaining = &remaining[end + 2..];

        let Some(value) = tag.strip_prefix("include") else {
            continue;
        };
        let value = value.trim_start();
        let Some(quote @ ('\'' | '"')) = value.chars().next() else {
            continue;
        };
        let quoted = &value[quote.len_utf8()..];
        if let Some(end) = quoted.find(quote) {
            includes.push(&quoted[..end]);
        }
    }
    includes
}

async fn resolve_token(
    root_dir: &Path,
    target: &str,
    variable: &str,
    profile: &OAuthProfile,
    http_client: &reqwest::Client,
) -> Result<TokenState> {
    if let Some(token) = load_token(root_dir, target, variable)? {
        if token.is_valid() {
            return Ok(token);
        }

        if let Some(refresh_token) = &token.refresh_token {
            match refresh(profile, refresh_token, http_client).await {
                Ok(response) => {
                    let refreshed = token_state(&response, Some(refresh_token));
                    save_token(root_dir, target, variable, &refreshed)?;
                    return Ok(refreshed);
                }
                Err(error) => {
                    log::warn!("# OAuth refresh failed: {error}");
                }
            }
        }
    }

    let token = match profile.flow {
        Flow::AuthorizationCode => authorize(profile, http_client).await?,
        Flow::ClientCredentials => {
            client_credentials(profile, http_client).await?
        }
    };
    save_token(root_dir, target, variable, &token)?;
    Ok(token)
}

async fn authorize(
    profile: &OAuthProfile,
    http_client: &reqwest::Client,
) -> Result<TokenState> {
    if !is_interactive_mode() {
        bail!("login required, but Hitman is running non-interactively");
    }

    let authorization_url = profile
        .authorization_url
        .as_ref()
        .context("`authorization_url` is required for authorization_code")?;
    let redirect_uri = profile
        .redirect_uri
        .as_ref()
        .context("`redirect_uri` is required for authorization_code")?;
    let redirect =
        reqwest::Url::parse(redirect_uri).context("Invalid redirect_uri")?;
    let listener = bind_callback(&redirect).await?;

    let mut client = BasicClient::new(ClientId::new(profile.client_id.clone()))
        .set_auth_uri(AuthUrl::new(authorization_url.clone())?)
        .set_token_uri(TokenUrl::new(profile.token_url.clone())?)
        .set_redirect_uri(RedirectUrl::new(redirect_uri.clone())?)
        .set_auth_type(profile.client_auth_method.auth_type());
    if let Some(secret) = &profile.client_secret {
        client = client.set_client_secret(ClientSecret::new(secret.clone()));
    }

    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let mut authorization = client
        .authorize_url(CsrfToken::new_random)
        .set_pkce_challenge(challenge);
    for scope in &profile.scopes {
        authorization = authorization.add_scope(Scope::new(scope.clone()));
    }
    for (key, value) in &profile.authorization_params {
        authorization = authorization.add_extra_param(key, value);
    }
    let (url, expected_state) = authorization.url();

    log::warn!("# Opening OAuth login in your browser...");
    log::warn!("# {url}");
    if let Err(error) = open::that(url.as_str()) {
        log::warn!("# Could not open browser: {error}");
    }

    let code = wait_for_callback(listener, &redirect, &expected_state).await?;
    let mut request = client.exchange_code(code).set_pkce_verifier(verifier);
    for (key, value) in &profile.token_params {
        request = request.add_extra_param(key, value);
    }
    let response = request.request_async(http_client).await?;
    Ok(token_state(&response, None))
}

async fn client_credentials(
    profile: &OAuthProfile,
    http_client: &reqwest::Client,
) -> Result<TokenState> {
    let mut client = BasicClient::new(ClientId::new(profile.client_id.clone()))
        .set_token_uri(TokenUrl::new(profile.token_url.clone())?)
        .set_auth_type(profile.client_auth_method.auth_type());
    if let Some(secret) = &profile.client_secret {
        client = client.set_client_secret(ClientSecret::new(secret.clone()));
    }

    let mut request = client.exchange_client_credentials();
    for scope in &profile.scopes {
        request = request.add_scope(Scope::new(scope.clone()));
    }
    for (key, value) in &profile.token_params {
        request = request.add_extra_param(key, value);
    }
    let response = request.request_async(http_client).await?;
    Ok(token_state(&response, None))
}

async fn refresh(
    profile: &OAuthProfile,
    refresh_token: &str,
    http_client: &reqwest::Client,
) -> Result<BasicTokenResponse> {
    let mut client = BasicClient::new(ClientId::new(profile.client_id.clone()))
        .set_token_uri(TokenUrl::new(profile.token_url.clone())?)
        .set_auth_type(profile.client_auth_method.auth_type());
    if let Some(secret) = &profile.client_secret {
        client = client.set_client_secret(ClientSecret::new(secret.clone()));
    }

    let refresh_token = RefreshToken::new(refresh_token.to_string());
    let mut request = client.exchange_refresh_token(&refresh_token);
    for (key, value) in &profile.token_params {
        request = request.add_extra_param(key, value);
    }
    Ok(request.request_async(http_client).await?)
}

impl ClientAuthMethod {
    fn auth_type(&self) -> AuthType {
        match self {
            Self::Basic => AuthType::BasicAuth,
            Self::RequestBody => AuthType::RequestBody,
        }
    }
}

async fn bind_callback(redirect: &reqwest::Url) -> Result<TcpListener> {
    if redirect.scheme() != "http" {
        bail!("OAuth redirect_uri must use http on a loopback address");
    }
    let host = redirect.host_str().context("redirect_uri has no host")?;
    let ip = if host.eq_ignore_ascii_case("localhost") {
        IpAddr::from([127, 0, 0, 1])
    } else {
        host.parse::<IpAddr>()
            .context("redirect_uri host must be localhost or a loopback IP")?
    };
    if !ip.is_loopback() {
        bail!("OAuth redirect_uri must use a loopback address");
    }
    let port = redirect
        .port_or_known_default()
        .context("redirect_uri has no port")?;
    Ok(TcpListener::bind(SocketAddr::new(ip, port)).await?)
}

async fn wait_for_callback(
    listener: TcpListener,
    redirect: &reqwest::Url,
    expected_state: &CsrfToken,
) -> Result<AuthorizationCode> {
    timeout(CALLBACK_TIMEOUT, async {
        loop {
            let (mut stream, _) = listener.accept().await?;
            let mut request_line = String::new();
            BufReader::new(&mut stream)
                .read_line(&mut request_line)
                .await?;
            let Some(path) = request_line.split_whitespace().nth(1) else {
                write_callback_response(
                    &mut stream,
                    400,
                    "Invalid OAuth callback",
                )
                .await?;
                continue;
            };
            let callback = redirect.join(path)?;
            if callback.path() != redirect.path() {
                write_callback_response(&mut stream, 404, "Not found").await?;
                continue;
            }

            let params = callback.query_pairs().collect::<HashMap<_, _>>();
            if let Some(error) = params.get("error") {
                write_callback_response(&mut stream, 400, "OAuth login failed")
                    .await?;
                bail!("authorization server returned `{error}`");
            }
            let state = params
                .get("state")
                .context("OAuth callback omitted state")?;
            if state.as_ref() != expected_state.secret() {
                write_callback_response(
                    &mut stream,
                    400,
                    "Invalid OAuth state",
                )
                .await?;
                bail!("OAuth state did not match");
            }
            let code =
                params.get("code").context("OAuth callback omitted code")?;
            write_callback_response(
                &mut stream,
                200,
                "OAuth login completed. You can close this window.",
            )
            .await?;
            return Ok(AuthorizationCode::new(code.to_string()));
        }
    })
    .await
    .context("Timed out waiting for OAuth callback")?
}

async fn write_callback_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    message: &str,
) -> Result<()> {
    let reason = if status == 200 { "OK" } else { "Error" };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{message}",
        message.len()
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

fn token_state(
    response: &BasicTokenResponse,
    fallback_refresh_token: Option<&str>,
) -> TokenState {
    TokenState {
        access_token: response.access_token().secret().clone(),
        refresh_token: response
            .refresh_token()
            .map(|token| token.secret().clone())
            .or_else(|| fallback_refresh_token.map(ToOwned::to_owned)),
        token_type: response.token_type().as_ref().to_string(),
        expires_at: response
            .expires_in()
            .map(|duration| unix_timestamp() + duration.as_secs() as i64),
        scopes: response
            .scopes()
            .map(|scopes| {
                scopes
                    .iter()
                    .map(|scope| scope.as_ref().to_string())
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn load_token(
    root_dir: &Path,
    target: &str,
    variable: &str,
) -> Result<Option<TokenState>> {
    let data = read_data(root_dir)?;
    let token = data
        .get(OAUTH_STATE_KEY)
        .and_then(Value::as_table)
        .and_then(|targets| targets.get(target))
        .and_then(Value::as_table)
        .and_then(|tokens| tokens.get(variable));
    token
        .cloned()
        .map(|value| value.try_into().context("Invalid stored OAuth token"))
        .transpose()
}

fn save_token(
    root_dir: &Path,
    target: &str,
    variable: &str,
    token: &TokenState,
) -> Result<()> {
    let data = read_data(root_dir)?;
    let mut oauth = data
        .get(OAUTH_STATE_KEY)
        .and_then(Value::as_table)
        .cloned()
        .unwrap_or_default();
    let mut target_tokens = oauth
        .get(target)
        .and_then(Value::as_table)
        .cloned()
        .unwrap_or_default();
    target_tokens.insert(variable.to_string(), Value::try_from(token)?);
    oauth.insert(target.to_string(), Value::Table(target_tokens));

    let mut update = Table::new();
    update.insert(OAUTH_STATE_KEY.to_string(), Value::Table(oauth));
    update_data(root_dir, &update)
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use mktemp::Temp;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::{resolve::resolve_path, scope::Replacement};

    #[test]
    fn profile_name_is_the_managed_variable_name() {
        let temp = Temp::new_dir().unwrap();
        fs::write(
            temp.join("hitman.toml"),
            r#"
[_oauth.github_access_token]
flow = "authorization_code"
authorization_url = "https://github.com/login/oauth/authorize"
token_url = "https://github.com/login/oauth/access_token"
client_id = "client"
redirect_uri = "http://127.0.0.1:8765/callback"
scopes = ["repo"]
"#,
        )
        .unwrap();

        let profiles = load_profiles(&temp).unwrap();
        assert!(profiles.contains_key("github_access_token"));
        assert_eq!(
            profiles["github_access_token"].flow,
            Flow::AuthorizationCode
        );
    }

    #[test]
    fn finds_referenced_template_variables() {
        let temp = Temp::new_dir().unwrap();
        let request = temp.join("request.http");
        fs::write(
            &request,
            "GET {{base_url}}\nAuthorization: Bearer {{github_access_token}}\n",
        )
        .unwrap();

        let variables = referenced_variables(&request).unwrap();
        assert!(variables.contains("base_url"));
        assert!(variables.contains("github_access_token"));
    }

    #[test]
    fn finds_oauth_variable_in_static_include() {
        let temp = Temp::new_dir().unwrap();
        let request = temp.join("request.http");
        fs::write(
            &request,
            "{% include \"headers.http\" %}\nGET {{base_url}}\n",
        )
        .unwrap();
        fs::write(
            temp.join("headers.http"),
            "Authorization: Bearer {{github_access_token}}\n",
        )
        .unwrap();

        let variables = referenced_variables(&request).unwrap();
        assert!(variables.contains("github_access_token"));
    }

    #[test]
    fn token_state_is_scoped_by_target_and_variable() {
        let temp = Temp::new_dir().unwrap();
        let token = TokenState {
            access_token: "secret".into(),
            refresh_token: Some("refresh".into()),
            token_type: "bearer".into(),
            expires_at: Some(unix_timestamp() + 3600),
            scopes: vec!["repo".into()],
        };

        save_token(&temp, "development", "github_access_token", &token)
            .unwrap();

        let loaded = load_token(&temp, "development", "github_access_token")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.access_token, "secret");
        assert!(load_token(&temp, "production", "github_access_token")
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn acquires_referenced_client_credentials_token() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let size = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.contains("grant_type=client_credentials"));

            let body = r#"{"access_token":"managed-token","token_type":"Bearer","expires_in":3600}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let temp = Temp::new_dir().unwrap();
        fs::write(
            temp.join("hitman.toml"),
            format!(
                r#"
[_oauth.service_access_token]
flow = "client_credentials"
token_url = "http://{address}/token"
client_id = "client"
client_secret = "secret"
scopes = ["read"]
"#
            ),
        )
        .unwrap();
        let request = temp.join("request.http");
        fs::write(
            &request,
            "GET https://example.com\nAuthorization: Bearer {{service_access_token}}\n",
        )
        .unwrap();
        let resolved = resolve_path(&request).unwrap();
        let scope = Table::new().into();

        let scope = resolve_scope(&resolved, "development", &scope)
            .await
            .unwrap();
        assert_eq!(
            scope.lookup("service_access_token").unwrap(),
            Replacement::Value("managed-token".into())
        );
        assert_eq!(
            load_token(&temp, "development", "service_access_token")
                .unwrap()
                .unwrap()
                .access_token,
            "managed-token"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn explicit_value_skips_oauth_acquisition() {
        let temp = Temp::new_dir().unwrap();
        fs::write(
            temp.join("hitman.toml"),
            r#"
[_oauth.service_access_token]
flow = "client_credentials"
token_url = "http://127.0.0.1:1/token"
client_id = "client"
"#,
        )
        .unwrap();
        let request = temp.join("request.http");
        fs::write(
            &request,
            "GET https://example.com\nAuthorization: Bearer {{service_access_token}}\n",
        )
        .unwrap();
        let resolved = resolve_path(&request).unwrap();
        let mut values = Table::new();
        values.insert(
            "service_access_token".into(),
            Value::String("explicit-token".into()),
        );

        let scope = resolve_scope(&resolved, "development", &values.into())
            .await
            .unwrap();
        assert_eq!(
            scope.lookup("service_access_token").unwrap(),
            Replacement::Value("explicit-token".into())
        );
        assert!(load_token(&temp, "development", "service_access_token")
            .unwrap()
            .is_none());
    }
}

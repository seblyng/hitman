use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tower_lsp::{
    async_trait,
    jsonrpc::Result as LspResult,
    lsp_types::{
        CodeAction, CodeActionKind, CodeActionOptions, CodeActionOrCommand,
        CodeActionParams, CodeActionProviderCapability, Command,
        DidChangeTextDocumentParams, DidOpenTextDocumentParams,
        ExecuteCommandOptions, ExecuteCommandParams, InitializeParams,
        InitializeResult, MessageType, Position, Range, ServerCapabilities,
        TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit, Url,
        WorkspaceEdit,
    },
    Client, LanguageServer, LspService, Server,
};

use crate::{
    env::get_target,
    resolve::{find_root_dir, Resolved, ResolvedAs},
    scope::Replacement,
    transport::grpc::{
        list_methods, list_services, message_template, DescriptorSource,
    },
};

const GENERATE_TEMPLATE: &str = "hitman.generateMessageTemplate";
const LIST_GRPC_METHODS: &str = "hitman.listGrpcMethods";
const SET_GRPC_METHOD: &str = "hitman.setGrpcMethod";
const SELECT_GRPC_METHOD: &str = "hitman.selectGrpcMethod";

pub async fn serve() -> Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(|client| Backend {
        client,
        documents: Arc::new(Mutex::new(HashMap::new())),
    });

    Server::new(stdin, stdout, socket).serve(service).await;
    Ok(())
}

#[derive(Clone)]
struct Backend {
    client: Client,
    documents: Arc<Mutex<HashMap<Url, String>>>,
}

#[async_trait]
impl LanguageServer for Backend {
    async fn initialize(
        &self,
        _: InitializeParams,
    ) -> LspResult<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                code_action_provider: Some(
                    CodeActionProviderCapability::Options(CodeActionOptions {
                        code_action_kinds: Some(vec![CodeActionKind::REFACTOR]),
                        ..Default::default()
                    }),
                ),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![
                        GENERATE_TEMPLATE.to_string(),
                        LIST_GRPC_METHODS.to_string(),
                        SET_GRPC_METHOD.to_string(),
                    ],
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: tower_lsp::lsp_types::InitializedParams) {}

    async fn shutdown(&self) -> LspResult<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.documents
            .lock()
            .await
            .insert(params.text_document.uri, params.text_document.text);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(change) = params.content_changes.into_iter().last() {
            self.documents
                .lock()
                .await
                .insert(params.text_document.uri, change.text);
        }
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> LspResult<Option<Vec<CodeActionOrCommand>>> {
        let uri = params.text_document.uri;
        let Some(text) = self.documents.lock().await.get(&uri).cloned() else {
            return Ok(None);
        };

        match code_actions_for_document(&uri, &text) {
            Ok(actions) => Ok(Some(actions)),
            Err(err) => {
                self.client
                    .log_message(MessageType::ERROR, err.to_string())
                    .await;
                Ok(None)
            }
        }
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> LspResult<Option<serde_json::Value>> {
        match params.command.as_str() {
            LIST_GRPC_METHODS => {
                let Ok(ListGrpcMethodsArgs { uri }) =
                    parse_command_args(params.arguments)
                else {
                    self.client
                        .log_message(
                            MessageType::ERROR,
                            "Invalid hitman.listGrpcMethods arguments",
                        )
                        .await;
                    return Ok(None);
                };

                let Some(text) = self.documents.lock().await.get(&uri).cloned()
                else {
                    return Ok(None);
                };

                match grpc_methods_for_document(&uri, &text) {
                    Ok(methods) => Ok(serde_json::to_value(methods).ok()),
                    Err(err) => {
                        self.client
                            .show_message(MessageType::ERROR, err.to_string())
                            .await;
                        Ok(None)
                    }
                }
            }
            GENERATE_TEMPLATE => {
                let Ok(GenerateTemplateArgs { uri, method }) =
                    parse_command_args(params.arguments)
                else {
                    self.client
                        .log_message(
                            MessageType::ERROR,
                            "Invalid hitman.generateMessageTemplate arguments",
                        )
                        .await;
                    return Ok(None);
                };

                let Some(text) = self.documents.lock().await.get(&uri).cloned()
                else {
                    return Ok(None);
                };

                match template_edit(&uri, &text, &method) {
                    Ok(edit) => {
                        let applied = self
                            .client
                            .apply_edit(edit)
                            .await
                            .map(|response| response.applied)
                            .unwrap_or(false);
                        if !applied {
                            self.client
                                .show_message(
                                    MessageType::ERROR,
                                    "Editor rejected hitman template edit",
                                )
                                .await;
                        }
                    }
                    Err(err) => {
                        self.client
                            .show_message(MessageType::ERROR, err.to_string())
                            .await;
                    }
                }

                Ok(None)
            }
            SET_GRPC_METHOD => {
                let Ok(SetGrpcMethodArgs { uri, method }) =
                    parse_command_args(params.arguments)
                else {
                    self.client
                        .log_message(
                            MessageType::ERROR,
                            "Invalid hitman.setGrpcMethod arguments",
                        )
                        .await;
                    return Ok(None);
                };

                let Some(text) = self.documents.lock().await.get(&uri).cloned()
                else {
                    return Ok(None);
                };

                match set_grpc_method_edit(&uri, &text, &method) {
                    Ok(edit) => {
                        let applied = self
                            .client
                            .apply_edit(edit)
                            .await
                            .map(|response| response.applied)
                            .unwrap_or(false);
                        if !applied {
                            self.client
                                .show_message(
                                    MessageType::ERROR,
                                    "Editor rejected hitman method edit",
                                )
                                .await;
                        }
                    }
                    Err(err) => {
                        self.client
                            .show_message(MessageType::ERROR, err.to_string())
                            .await;
                    }
                }

                Ok(None)
            }
            _ => Ok(None),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct GenerateTemplateArgs {
    uri: Url,
    method: String,
}

#[derive(Serialize, Deserialize)]
struct SetGrpcMethodArgs {
    uri: Url,
    method: String,
}

#[derive(Serialize, Deserialize)]
struct ListGrpcMethodsArgs {
    uri: Url,
}

#[derive(Debug, PartialEq, Serialize)]
struct GrpcMethodItem {
    label: String,
    method: String,
}

fn parse_command_args<T: for<'de> Deserialize<'de>>(
    mut args: Vec<serde_json::Value>,
) -> Result<T> {
    let value = args.pop().context("Missing hitman command arguments")?;

    Ok(serde_json::from_value(value)?)
}

fn grpc_methods_for_document(
    uri: &Url,
    text: &str,
) -> Result<Vec<GrpcMethodItem>> {
    let path = uri_to_path(uri)?;
    let descriptor_source = descriptor_source(&path, text)?;
    let service = proto_service(&path, text, &descriptor_source)?;
    let current_method = grpc_method_name(text);
    let mut methods = list_methods(&descriptor_source, &service)?
        .into_iter()
        .map(|method| GrpcMethodItem {
            label: method.name().to_string(),
            method: method.full_name().to_string(),
        })
        .collect::<Vec<_>>();

    if let Some(current_method) = current_method {
        methods.sort_by_key(|method| method.label != current_method);
    }

    Ok(methods)
}

fn code_actions_for_document(
    uri: &Url,
    text: &str,
) -> Result<Vec<CodeActionOrCommand>> {
    let methods = grpc_methods_for_document(uri, text)?;
    if methods.is_empty() {
        return Ok(Vec::new());
    }

    let current_method = grpc_method_name(text);
    let mut actions = Vec::new();

    if let Some(current_method) = current_method {
        if let Some(method) =
            methods.iter().find(|method| method.label == current_method)
        {
            let title = "Generate message template".to_string();
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: title.clone(),
                kind: Some(CodeActionKind::REFACTOR),
                command: Some(Command {
                    title,
                    command: GENERATE_TEMPLATE.to_string(),
                    arguments: Some(vec![serde_json::json!({
                        "uri": uri,
                        "method": method.method,
                    })]),
                }),
                ..Default::default()
            }));
        }
    }

    let title = "Select gRPC method".to_string();
    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
        title: title.clone(),
        kind: Some(CodeActionKind::REFACTOR),
        command: Some(Command {
            title,
            command: SELECT_GRPC_METHOD.to_string(),
            arguments: Some(vec![serde_json::json!({
                "uri": uri,
            })]),
        }),
        ..Default::default()
    }));

    Ok(actions)
}

fn template_edit(
    uri: &Url,
    text: &str,
    full_method: &str,
) -> Result<WorkspaceEdit> {
    let path = uri_to_path(uri)?;
    let descriptor_source = descriptor_source(&path, text)?;
    let (service, method_name) = full_method
        .rsplit_once('.')
        .context("Invalid gRPC method")?;
    let method = list_methods(&descriptor_source, service)?
        .into_iter()
        .find(|method| method.name() == method_name)
        .with_context(|| format!("gRPC method not found: {full_method}"))?;
    let json = message_template(&method.input())?;
    let body = replacement_text(text, &serde_json::to_string_pretty(&json)?);
    let edit = TextEdit {
        range: replacement_range(text),
        new_text: body,
    };
    let mut changes = HashMap::new();
    changes.insert(uri.clone(), vec![edit]);

    Ok(WorkspaceEdit {
        changes: Some(changes),
        ..Default::default()
    })
}

fn set_grpc_method_edit(
    uri: &Url,
    text: &str,
    full_method: &str,
) -> Result<WorkspaceEdit> {
    let path = uri_to_path(uri)?;
    let descriptor_source = descriptor_source(&path, text)?;
    let (service_name, method_name) = full_method
        .rsplit_once('.')
        .context("Invalid gRPC method")?;
    let method = list_methods(&descriptor_source, service_name)?
        .into_iter()
        .find(|method| method.name() == method_name)
        .with_context(|| format!("gRPC method not found: {full_method}"))?;
    let (line_index, line) = text
        .lines()
        .enumerate()
        .find_map(|(i, line)| {
            line.trim_start().starts_with("GRPC ").then_some((i, line))
        })
        .context("Missing GRPC request line")?;
    let new_line = replace_grpc_method(line, service_name, method_name)?;
    let method_edit = TextEdit {
        range: Range {
            start: Position::new(line_index as u32, 0),
            end: Position::new(line_index as u32, line.len() as u32),
        },
        new_text: new_line,
    };
    let json = message_template(&method.input())?;
    let body_edit = TextEdit {
        range: replacement_range(text),
        new_text: replacement_text(text, &serde_json::to_string_pretty(&json)?),
    };
    let mut changes = HashMap::new();
    changes.insert(uri.clone(), vec![method_edit, body_edit]);

    Ok(WorkspaceEdit {
        changes: Some(changes),
        ..Default::default()
    })
}

fn replace_grpc_method(
    line: &str,
    service_name: &str,
    method_name: &str,
) -> Result<String> {
    let prefix_len = line
        .find("GRPC ")
        .map(|index| index + "GRPC ".len())
        .context("Missing GRPC request line")?;
    let prefix = &line[..prefix_len];
    let target = line[prefix_len..].trim_end();
    let trailing = &line[prefix_len + target.len()..];
    let target = if let Some((base, current)) = target.rsplit_once('/') {
        if current.is_empty()
            || current.contains('}')
            || current == service_name
        {
            format!("{target}/{method_name}")
        } else {
            format!("{base}/{method_name}")
        }
    } else {
        format!("{target}/{method_name}")
    };

    Ok(format!("{prefix}{target}{trailing}"))
}

fn descriptor_source(file_path: &Path, text: &str) -> Result<DescriptorSource> {
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if key.trim().eq_ignore_ascii_case("proto") {
            return Ok(DescriptorSource::Proto(resolve_relative(
                file_path, value,
            )));
        }
        if key.trim().eq_ignore_ascii_case("protoset") {
            return Ok(DescriptorSource::Protoset(resolve_relative(
                file_path, value,
            )));
        }
    }

    anyhow::bail!("Missing Proto or Protoset header")
}

fn proto_service(
    file_path: &Path,
    text: &str,
    source: &DescriptorSource,
) -> Result<String> {
    let target = grpc_target(text).context("Missing GRPC request line")?;
    let resolved_target = if let Some(var) = template_variable(&target) {
        resolve_hitman_variable(file_path, &var)?
    } else {
        target
    };
    let services = list_services(source)?;

    services
        .into_iter()
        .find(|service| {
            resolved_target == *service || resolved_target.contains(service)
        })
        .context("API not found in descriptors")
}

fn grpc_target(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.trim_start().strip_prefix("GRPC "))
        .map(str::trim)
        .map(str::to_string)
}

fn grpc_method_name(text: &str) -> Option<String> {
    grpc_target(text)?.rsplit('/').next().map(str::to_string)
}

fn template_variable(target: &str) -> Option<String> {
    let start = target.find("{{")? + 2;
    let end = target[start..].find("}}")? + start;
    Some(target[start..end].trim().to_string())
}

fn resolve_hitman_variable(file_path: &Path, key: &str) -> Result<String> {
    let root =
        find_root_dir(file_path)?.context("Could not find hitman root")?;
    let resolved = Resolved {
        root_dir: root.clone(),
        resolved_as: ResolvedAs::Simple {
            path: file_path.into(),
        },
    };
    let target = get_target(&root);
    let scope = crate::env::load_env(&target, &resolved, &[])?;

    match scope.lookup(key)? {
        Replacement::Value(value) => Ok(value),
        Replacement::ValueNotFound { key } => {
            anyhow::bail!("No value found for {{{{{key}}}}}")
        }
        Replacement::MultipleValuesFound { key, .. } => {
            anyhow::bail!("Multiple values found for {{{{{key}}}}}")
        }
    }
}

fn replacement_range(text: &str) -> Range {
    let lines: Vec<_> = text.lines().collect();
    let start_line = lines
        .iter()
        .position(|line| line.trim_start().starts_with('{'))
        .unwrap_or_else(|| {
            lines
                .iter()
                .rposition(|line| !line.trim().is_empty())
                .map(|line| line + 1)
                .unwrap_or(0)
        });

    Range {
        start: Position::new(start_line as u32, 0),
        end: Position::new(lines.len() as u32, 0),
    }
}

fn replacement_text(text: &str, body: &str) -> String {
    if text.lines().any(|line| line.trim_start().starts_with('{')) {
        body.to_string()
    } else {
        format!("\n{body}")
    }
}

fn resolve_relative(file_path: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        file_path.parent().unwrap_or(Path::new(".")).join(path)
    }
}

fn uri_to_path(uri: &Url) -> Result<PathBuf> {
    uri.to_file_path()
        .map_err(|_| anyhow::anyhow!("Only file:// URIs are supported"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mktemp::Temp;
    use prost::Message;
    use std::fs;

    fn grpc_fixture() -> (Temp, PathBuf, String) {
        let tmp = Temp::new_dir().unwrap();
        fs::write(
            tmp.join("add.proto"),
            r#"
                syntax = "proto3";
                package addsvc;

                service Add {
                    rpc Sum (SumRequest) returns (SumResponse);
                    rpc Concat (ConcatRequest) returns (ConcatResponse);
                }

                message SumRequest {
                    int32 a = 1;
                    int32 b = 2;
                }

                message SumResponse { int32 result = 1; }
                message ConcatRequest { string a = 1; string b = 2; }
                message ConcatResponse { string result = 1; }
            "#,
        )
        .unwrap();
        let descriptors =
            protox::compile([tmp.join("add.proto")], [tmp.as_path()]).unwrap();
        fs::write(tmp.join("protoset.bin"), descriptors.encode_to_vec())
            .unwrap();

        let request = tmp.join("sum.http");
        let text = "GRPC grpcb.in:9001/addsvc.Add/Sum\nProtoset: ./protoset.bin\n\n{}\n".to_string();
        fs::write(&request, &text).unwrap();

        (tmp, request, text)
    }

    #[test]
    fn offers_action_for_current_grpc_method() {
        let (_tmp, request, text) = grpc_fixture();
        let uri = Url::from_file_path(request).unwrap();

        let actions = code_actions_for_document(&uri, &text).unwrap();

        assert_eq!(actions.len(), 2);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected code action");
        };
        assert_eq!(action.title, "Generate message template");
        assert_eq!(
            action.command.as_ref().unwrap().command,
            "hitman.generateMessageTemplate"
        );
        let CodeActionOrCommand::CodeAction(action) = &actions[1] else {
            panic!("expected code action");
        };
        assert_eq!(action.title, "Select gRPC method");
        assert_eq!(
            action.command.as_ref().unwrap().command,
            "hitman.selectGrpcMethod"
        );
    }

    #[test]
    fn offers_single_picker_action_without_current_method() {
        let (_tmp, request, _) = grpc_fixture();
        let text = "GRPC grpcb.in:9001/addsvc.Add\nProtoset: ./protoset.bin\n";
        fs::write(&request, text).unwrap();
        let uri = Url::from_file_path(request).unwrap();

        let actions = code_actions_for_document(&uri, text).unwrap();

        assert_eq!(actions.len(), 1);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected code action");
        };
        assert_eq!(action.title, "Select gRPC method");
        assert_eq!(
            action.command.as_ref().unwrap().command,
            "hitman.selectGrpcMethod"
        );
    }

    #[test]
    fn creates_workspace_edit_for_message_template() {
        let (_tmp, request, text) = grpc_fixture();
        let uri = Url::from_file_path(request).unwrap();

        let edit = template_edit(&uri, &text, "addsvc.Add.Sum").unwrap();
        let changes = edit.changes.unwrap();
        let edit = &changes[&uri][0];

        assert_eq!(edit.range.start.line, 3);
        assert_eq!(edit.new_text, "{\n  \"a\": 0,\n  \"b\": 0\n}");
    }

    #[test]
    fn inserts_blank_line_before_new_message_body() {
        let (_tmp, request, _) = grpc_fixture();
        let text =
            "GRPC grpcb.in:9001/addsvc.Add/Sum\nProtoset: ./protoset.bin\n";
        fs::write(&request, text).unwrap();
        let uri = Url::from_file_path(request).unwrap();

        let edit = template_edit(&uri, text, "addsvc.Add.Sum").unwrap();
        let changes = edit.changes.unwrap();
        let edit = &changes[&uri][0];

        assert_eq!(edit.range.start.line, 2);
        assert_eq!(edit.new_text, "\n{\n  \"a\": 0,\n  \"b\": 0\n}");
    }

    #[test]
    fn updates_existing_grpc_method_in_request_line() {
        let (_tmp, request, text) = grpc_fixture();
        let uri = Url::from_file_path(request).unwrap();

        let edit =
            set_grpc_method_edit(&uri, &text, "addsvc.Add.Concat").unwrap();
        let changes = edit.changes.unwrap();
        let method_edit = &changes[&uri][0];
        let body_edit = &changes[&uri][1];

        assert_eq!(method_edit.range.start.line, 0);
        assert_eq!(
            method_edit.new_text,
            "GRPC grpcb.in:9001/addsvc.Add/Concat"
        );
        assert_eq!(body_edit.range.start.line, 3);
        assert_eq!(body_edit.new_text, "{\n  \"a\": \"\",\n  \"b\": \"\"\n}");
    }

    #[test]
    fn appends_grpc_method_when_request_line_only_has_service() {
        let (_tmp, request, _) = grpc_fixture();
        let text = "GRPC grpcb.in:9001/addsvc.Add\nProtoset: ./protoset.bin\n";
        fs::write(&request, text).unwrap();
        let uri = Url::from_file_path(request).unwrap();

        let edit =
            set_grpc_method_edit(&uri, text, "addsvc.Add.Concat").unwrap();
        let changes = edit.changes.unwrap();
        let method_edit = &changes[&uri][0];
        let body_edit = &changes[&uri][1];

        assert_eq!(
            method_edit.new_text,
            "GRPC grpcb.in:9001/addsvc.Add/Concat"
        );
        assert_eq!(body_edit.new_text, "\n{\n  \"a\": \"\",\n  \"b\": \"\"\n}");
    }

    #[test]
    fn parses_execute_command_arguments() {
        let uri = Url::parse("file:///tmp/sum.http").unwrap();

        let args: GenerateTemplateArgs =
            parse_command_args(vec![serde_json::json!({
                "uri": uri,
                "method": "addsvc.Add.Sum",
            })])
            .unwrap();

        assert_eq!(args.uri, Url::parse("file:///tmp/sum.http").unwrap());
        assert_eq!(args.method, "addsvc.Add.Sum");
    }

    #[test]
    fn lists_grpc_methods_for_picker() {
        let (_tmp, request, text) = grpc_fixture();
        let uri = Url::from_file_path(request).unwrap();

        let methods = grpc_methods_for_document(&uri, &text).unwrap();

        assert_eq!(
            methods,
            vec![
                GrpcMethodItem {
                    label: "Sum".to_string(),
                    method: "addsvc.Add.Sum".to_string(),
                },
                GrpcMethodItem {
                    label: "Concat".to_string(),
                    method: "addsvc.Add.Concat".to_string(),
                },
            ]
        );
    }
}

use std::{
    collections::HashMap,
    fs,
    ops::Range as ByteRange,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use toml::{Table as TomlTable, Value as TomlValue};
use toml_edit::{ImDocument, Item as TomlEditItem, Table as TomlEditTable};
use tower_lsp::{
    async_trait,
    jsonrpc::Result as LspResult,
    lsp_types::{
        CodeAction, CodeActionKind, CodeActionOptions, CodeActionOrCommand,
        CodeActionParams, CodeActionProviderCapability, Command,
        CompletionItem, CompletionItemKind, CompletionOptions,
        CompletionTextEdit,
        CompletionParams, CompletionResponse, DidChangeTextDocumentParams,
        DidOpenTextDocumentParams, ExecuteCommandOptions, ExecuteCommandParams,
        GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents,
        HoverParams, HoverProviderCapability, InitializeParams,
        InitializeResult, InsertTextFormat, Location, MarkedString, MessageType,
        OneOf, Position, Range, ReferenceParams, ServerCapabilities,
        TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit, Url,
        WorkspaceEdit,
    },
    Client, LanguageServer, LspService, Server,
};

use prost_reflect::{Cardinality, FieldDescriptor, Kind, MessageDescriptor};

use crate::{
    env::get_target,
    resolve::{find_root_dir, resolve_path, Resolved, ResolvedAs},
    scope::Replacement,
    transport::grpc::{
        list_methods, list_services, load_descriptor_pool, message_template,
        DescriptorSource,
    },
};

const GENERATE_TEMPLATE: &str = "hitman.generateMessageTemplate";
const LIST_GRPC_METHODS: &str = "hitman.listGrpcMethods";
const SET_GRPC_METHOD: &str = "hitman.setGrpcMethod";
const SELECT_GRPC_METHOD: &str = "hitman.selectGrpcMethod";
const CONFIG_FILE: &str = "hitman.toml";
const LOCAL_CONFIG_FILE: &str = "hitman.local.toml";
const DATA_FILE: &str = ".hitman-data.toml";

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
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        "\"".to_string(),
                    ]),
                    ..Default::default()
                }),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
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

    async fn hover(&self, params: HoverParams) -> LspResult<Option<Hover>> {
        let params = params.text_document_position_params;
        let uri = params.text_document.uri;
        let Some(text) = self.documents.lock().await.get(&uri).cloned() else {
            return Ok(None);
        };

        match hover_for_position(&uri, &text, params.position) {
            Ok(hover) => Ok(hover),
            Err(err) => {
                self.client
                    .log_message(MessageType::ERROR, err.to_string())
                    .await;
                Ok(None)
            }
        }
    }

    async fn completion(
        &self,
        params: CompletionParams,
    ) -> LspResult<Option<CompletionResponse>> {
        let params = params.text_document_position;
        let uri = params.text_document.uri;
        let Some(text) = self.documents.lock().await.get(&uri).cloned() else {
            return Ok(None);
        };

        match completion_for_position(&uri, &text, params.position) {
            Ok(completion) => Ok(completion),
            Err(err) => {
                self.client
                    .log_message(MessageType::ERROR, err.to_string())
                    .await;
                Ok(None)
            }
        }
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> LspResult<Option<GotoDefinitionResponse>> {
        let params = params.text_document_position_params;
        let uri = params.text_document.uri;
        let Some(text) = self.documents.lock().await.get(&uri).cloned() else {
            return Ok(None);
        };

        match definition_for_position(&uri, &text, params.position) {
            Ok(definition) => Ok(definition.map(GotoDefinitionResponse::Array)),
            Err(err) => {
                self.client
                    .log_message(MessageType::ERROR, err.to_string())
                    .await;
                Ok(None)
            }
        }
    }

    async fn references(
        &self,
        params: ReferenceParams,
    ) -> LspResult<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri;
        let Some(text) = self.documents.lock().await.get(&uri).cloned() else {
            return Ok(None);
        };

        match references_for_position(
            &uri,
            &text,
            params.text_document_position.position,
            params.context.include_declaration,
        ) {
            Ok(references) => Ok(references),
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
                "methods": methods,
            })]),
        }),
        ..Default::default()
    }));

    Ok(actions)
}

#[derive(Debug, Clone, PartialEq)]
struct VariableOccurrence {
    key: String,
    range: Range,
}

struct VariableDefinition {
    location: Location,
    value: Option<TomlValue>,
}

struct TomlKeyDefinition {
    range: Range,
    value: Option<TomlValue>,
}

fn hover_for_position(
    uri: &Url,
    text: &str,
    position: Position,
) -> Result<Option<Hover>> {
    let Some(occurrence) = variable_at_position(text, position) else {
        return Ok(None);
    };
    let definitions = variable_definitions(uri, &occurrence.key)?;
    if definitions.len() != 1 {
        return Ok(None);
    }
    let Some(value) =
        definitions.into_iter().next().and_then(|item| item.value)
    else {
        return Ok(None);
    };

    Ok(Some(Hover {
        contents: HoverContents::Scalar(MarkedString::String(format!(
            "`{}`",
            toml_value_display(&value)
        ))),
        range: Some(occurrence.range),
    }))
}

fn definition_for_position(
    uri: &Url,
    text: &str,
    position: Position,
) -> Result<Option<Vec<Location>>> {
    let Some(occurrence) = variable_at_position(text, position) else {
        return Ok(None);
    };
    let definitions = variable_definition_locations(uri, &occurrence.key)?;
    if definitions.is_empty() {
        return Ok(None);
    }

    Ok(Some(definitions))
}

fn references_for_position(
    uri: &Url,
    text: &str,
    position: Position,
    include_declaration: bool,
) -> Result<Option<Vec<Location>>> {
    let Some(occurrence) = variable_at_position(text, position) else {
        return Ok(None);
    };
    let path = uri_to_path(uri)?;
    let resolved = resolve_path(&path)?;
    let mut locations = Vec::new();

    for file_path in request_files(&resolved.root_dir)? {
        let Ok(file_text) = fs::read_to_string(&file_path) else {
            continue;
        };
        let file_uri = Url::from_file_path(&file_path)
            .map_err(|_| anyhow::anyhow!("Invalid file path"))?;
        locations.extend(
            variable_occurrences(&file_text)
                .into_iter()
                .filter(|found| found.key == occurrence.key)
                .map(|found| Location {
                    uri: file_uri.clone(),
                    range: found.range,
                }),
        );
    }

    if include_declaration {
        locations.extend(variable_definition_locations(uri, &occurrence.key)?);
    }

    locations.sort_by(|a, b| {
        a.uri
            .as_str()
            .cmp(b.uri.as_str())
            .then(a.range.start.line.cmp(&b.range.start.line))
            .then(a.range.start.character.cmp(&b.range.start.character))
    });
    locations.dedup_by(|a, b| a.uri == b.uri && a.range == b.range);

    Ok(Some(locations))
}

#[derive(Debug, Default, PartialEq)]
struct JsonCompletionContext {
    path: Vec<String>,
    field: Option<String>,
    existing_fields: Vec<String>,
    key_start: Option<usize>,
    value_start: Option<usize>,
    expecting_key: bool,
    in_string: bool,
}

#[derive(Debug)]
struct JsonFrame {
    field: Option<String>,
    kind: JsonFrameKind,
    fields: Vec<String>,
}

#[derive(Debug, PartialEq)]
enum JsonFrameKind {
    Object,
    Array,
}

fn completion_for_position(
    uri: &Url,
    text: &str,
    position: Position,
) -> Result<Option<CompletionResponse>> {
    let Some(body_start) = body_start_offset(text) else {
        return Ok(None);
    };
    let cursor = position_to_byte_offset(text, position);
    if cursor < body_start {
        return Ok(None);
    }

    let path = uri_to_path(uri)?;
    let descriptor_source = descriptor_source(&path, text)?;
    let service_name = proto_service(&path, text, &descriptor_source)?;
    let method_name = grpc_method_name(text).context("Missing gRPC method")?;
    let pool = load_descriptor_pool(&descriptor_source)?;
    let service = pool
        .get_service_by_name(&service_name)
        .with_context(|| format!("gRPC service not found: {service_name}"))?;
    let method = service
        .methods()
        .find(|method| method.name() == method_name)
        .with_context(|| format!("gRPC method not found: {method_name}"))?;
    let mut context = json_completion_context(&text[body_start..cursor]);
    context.existing_fields = existing_fields_for_path(
        &text[body_start..],
        &context.path,
    );
    let Some(message) = message_for_path(method.input(), &context.path) else {
        return Ok(None);
    };

    let items = if context.expecting_key {
        field_completion_items(
            &message,
            &context.existing_fields,
            field_completion_range(
                text,
                body_start,
                cursor,
                context.key_start,
            ),
        )
    } else if let Some(field_name) = context.field.as_deref() {
        enum_completion_items(
            &message,
            field_name,
            context.in_string,
            enum_completion_edit(
                text,
                body_start,
                cursor,
                context.value_start,
            ),
        )
    } else {
        Vec::new()
    };

    if items.is_empty() {
        Ok(None)
    } else {
        Ok(Some(CompletionResponse::Array(items)))
    }
}

fn field_completion_items(
    message: &MessageDescriptor,
    existing_fields: &[String],
    replacement_range: Range,
) -> Vec<CompletionItem> {
    message
        .fields()
        .filter(|field| {
            !existing_fields.iter().any(|existing| {
                existing == field.json_name() || existing == field.name()
            })
        })
        .map(|field| {
            let name = field.json_name().to_string();
            CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::FIELD),
                detail: Some(field_type_detail(&field)),
                filter_text: Some(format!("\"{name}\"")),
                insert_text: Some(field_completion_text(&field)),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: replacement_range,
                    new_text: field_completion_text(&field),
                })),
                command: field_completion_command(&field),
                ..Default::default()
            }
        })
        .collect()
}

fn field_completion_range(
    text: &str,
    body_start: usize,
    cursor: usize,
    key_start: Option<usize>,
) -> Range {
    let start = body_start + key_start.unwrap_or(cursor - body_start);
    let end = if key_start.is_some() && text[cursor..].starts_with('"') {
        cursor + 1
    } else {
        cursor
    };
    Range {
        start: byte_position(text, start),
        end: byte_position(text, end),
    }
}

fn field_completion_text(field: &FieldDescriptor) -> String {
    let name = field.json_name();
    format!("\"{name}\": {}", field_value_snippet(field))
}

fn field_value_snippet(field: &FieldDescriptor) -> &'static str {
    if field.cardinality() == Cardinality::Repeated {
        return "[$0]";
    }
    if field.is_map() {
        return "{$0}";
    }

    match field.kind() {
        Kind::String | Kind::Enum(_) => "\"$0\"",
        Kind::Message(_) => "{$0}",
        _ => "$0",
    }
}

fn field_completion_command(field: &FieldDescriptor) -> Option<Command> {
    if matches!(singular_field_kind(field), Kind::Enum(_)) {
        Some(Command {
            title: "Suggest".to_string(),
            command: "editor.action.triggerSuggest".to_string(),
            arguments: None,
        })
    } else {
        None
    }
}

fn enum_completion_items(
    message: &MessageDescriptor,
    field_name: &str,
    in_string: bool,
    edit: Option<(Range, Option<TextEdit>)>,
) -> Vec<CompletionItem> {
    let Some(field) = field_by_json_name(message, field_name) else {
        return Vec::new();
    };
    let Kind::Enum(en) = singular_field_kind(&field) else {
        return Vec::new();
    };

    en.values()
        .map(|value| {
            let name = value.name().to_string();
            let mut item = CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::ENUM_MEMBER),
                detail: Some(value.number().to_string()),
                insert_text: Some(if in_string {
                    format!("{name}$0")
                } else {
                    name.clone()
                }),
                insert_text_format: Some(if in_string {
                    InsertTextFormat::SNIPPET
                } else {
                    InsertTextFormat::PLAIN_TEXT
                }),
                ..Default::default()
            };
            if let Some((range, delete_closing_quote)) = edit.clone() {
                item.text_edit = Some(CompletionTextEdit::Edit(TextEdit {
                    range,
                    new_text: if delete_closing_quote.is_some() {
                        format!("{name}\"$0")
                    } else {
                        format!("{name}$0")
                    },
                }));
                item.additional_text_edits = delete_closing_quote.map(|edit| vec![edit]);
            }
            item
        })
        .collect()
}

fn enum_completion_edit(
    text: &str,
    body_start: usize,
    cursor: usize,
    value_start: Option<usize>,
) -> Option<(Range, Option<TextEdit>)> {
    let value_start = value_start?;
    let start = body_start + value_start;
    let delete_closing_quote = if text[cursor..].starts_with('"') {
        Some(TextEdit {
            range: Range {
                start: byte_position(text, cursor),
                end: byte_position(text, cursor + 1),
            },
            new_text: String::new(),
        })
    } else {
        None
    };

    Some((
        Range {
            start: byte_position(text, start),
            end: byte_position(text, cursor),
        },
        delete_closing_quote,
    ))
}

fn field_type_detail(field: &FieldDescriptor) -> String {
    if field.is_map() {
        return "object".to_string();
    }

    let ty = kind_type_detail(&field.kind());
    if field.cardinality() == Cardinality::Repeated {
        format!("array<{ty}>")
    } else {
        ty
    }
}

fn kind_type_detail(kind: &Kind) -> String {
    match kind {
        Kind::Double | Kind::Float => "number".to_string(),
        Kind::Int32
        | Kind::Sint32
        | Kind::Sfixed32
        | Kind::Int64
        | Kind::Sint64
        | Kind::Sfixed64
        | Kind::Uint32
        | Kind::Fixed32
        | Kind::Uint64
        | Kind::Fixed64 => "integer".to_string(),
        Kind::Bool => "boolean".to_string(),
        Kind::String => "string".to_string(),
        Kind::Bytes => "bytes".to_string(),
        Kind::Message(message) => message.full_name().to_string(),
        Kind::Enum(en) => en.full_name().to_string(),
    }
}

fn message_for_path(
    mut message: MessageDescriptor,
    path: &[String],
) -> Option<MessageDescriptor> {
    for field_name in path {
        let field = field_by_json_name(&message, field_name)?;
        let Kind::Message(next) = singular_field_kind(&field) else {
            return None;
        };
        message = next;
    }

    Some(message)
}

fn field_by_json_name(
    message: &MessageDescriptor,
    field_name: &str,
) -> Option<FieldDescriptor> {
    message.fields().find(|field| {
        field.json_name() == field_name || field.name() == field_name
    })
}

fn singular_field_kind(field: &FieldDescriptor) -> Kind {
    if field.is_map() {
        return field.kind();
    }

    field.kind()
}

fn json_completion_context(prefix: &str) -> JsonCompletionContext {
    let mut frames: Vec<JsonFrame> = Vec::new();
    let mut pending_field: Option<String> = None;
    let mut last_string: Option<String> = None;
    let mut in_string = false;
    let mut string_is_value = false;
    let mut string_is_key = false;
    let mut key_start = None;
    let mut value_start = None;
    let mut escape = false;
    let mut string = String::new();
    let mut saw_colon_since_string = false;
    let mut last_non_ws = None;

    for (offset, ch) in prefix.char_indices() {
        if in_string {
            if escape {
                string.push(ch);
                escape = false;
                continue;
            }
            match ch {
                '\\' => escape = true,
                '"' => {
                    in_string = false;
                    last_string = Some(std::mem::take(&mut string));
                    if string_is_value {
                        pending_field = None;
                    }
                    string_is_value = false;
                    string_is_key = false;
                    saw_colon_since_string = false;
                    last_non_ws = Some('"');
                }
                _ => string.push(ch),
            }
            continue;
        }

        if ch.is_whitespace() {
            continue;
        }

        match ch {
            '"' => {
                in_string = true;
                string_is_value = pending_field.is_some()
                    && matches!(last_non_ws, Some(':') | Some('[') | Some(','));
                string_is_key = frames
                    .last()
                    .is_some_and(|frame| frame.kind == JsonFrameKind::Object)
                    && pending_field.is_none()
                    && matches!(last_non_ws, Some('{') | Some(','));
                if string_is_key {
                    key_start = Some(offset);
                }
                if string_is_value {
                    value_start = Some(offset + 1);
                }
                string.clear();
                last_non_ws = Some(ch);
            }
            ':' => {
                if let Some(key) = last_string.take() {
                    if let Some(frame) = frames
                        .last_mut()
                        .filter(|frame| frame.kind == JsonFrameKind::Object)
                    {
                        frame.fields.push(key.clone());
                    }
                    pending_field = Some(key);
                }
                saw_colon_since_string = true;
                last_non_ws = Some(ch);
            }
            ',' => {
                pending_field = None;
                last_string = None;
                key_start = None;
                value_start = None;
                saw_colon_since_string = false;
                last_non_ws = Some(ch);
            }
            '{' => {
                let field = pending_field.take().or_else(|| {
                    frames.last().and_then(|frame| {
                        (frame.kind == JsonFrameKind::Array)
                            .then(|| frame.field.clone())
                            .flatten()
                    })
                });
                frames.push(JsonFrame {
                    field,
                    kind: JsonFrameKind::Object,
                    fields: Vec::new(),
                });
                last_string = None;
                key_start = None;
                value_start = None;
                saw_colon_since_string = false;
                last_non_ws = Some(ch);
            }
            '[' => {
                frames.push(JsonFrame {
                    field: pending_field.take(),
                    kind: JsonFrameKind::Array,
                    fields: Vec::new(),
                });
                last_string = None;
                key_start = None;
                value_start = None;
                saw_colon_since_string = false;
                last_non_ws = Some(ch);
            }
            '}' | ']' => {
                frames.pop();
                pending_field = None;
                last_string = None;
                key_start = None;
                value_start = None;
                saw_colon_since_string = false;
                last_non_ws = Some(ch);
            }
            _ => {
                last_non_ws = Some(ch);
            }
        }
    }

    let path = frames
        .iter()
        .filter(|frame| frame.kind == JsonFrameKind::Object)
        .filter_map(|frame| frame.field.clone())
        .collect::<Vec<_>>();
    let in_array_field = frames
        .last()
        .and_then(|frame| {
            (frame.kind == JsonFrameKind::Array).then(|| frame.field.clone())
        })
        .flatten();
    let field = pending_field.clone().or(in_array_field);
    let current_object = frames
        .last()
        .is_some_and(|frame| frame.kind == JsonFrameKind::Object);
    let existing_fields = frames
        .last()
        .filter(|frame| frame.kind == JsonFrameKind::Object)
        .map(|frame| frame.fields.clone())
        .unwrap_or_default();
    let expecting_key = current_object
        && field.is_none()
        && if in_string {
            string_is_key
        } else {
            matches!(last_non_ws, Some('{') | Some(','))
        }
        && !saw_colon_since_string;

    JsonCompletionContext {
        path,
        field,
        existing_fields,
        key_start,
        value_start,
        expecting_key,
        in_string,
    }
}

fn existing_fields_for_path(body: &str, target_path: &[String]) -> Vec<String> {
    let mut frames: Vec<JsonFrame> = Vec::new();
    let mut last_string: Option<String> = None;
    let mut pending_field: Option<String> = None;
    let mut in_string = false;
    let mut escape = false;
    let mut string = String::new();
    let mut fields = Vec::new();

    for ch in body.chars() {
        if in_string {
            if escape {
                string.push(ch);
                escape = false;
                continue;
            }
            match ch {
                '\\' => escape = true,
                '"' => {
                    in_string = false;
                    last_string = Some(std::mem::take(&mut string));
                }
                _ => string.push(ch),
            }
            continue;
        }

        match ch {
            '"' => {
                in_string = true;
                string.clear();
            }
            ':' => {
                if let Some(key) = last_string.take() {
                    if object_path(&frames) == target_path {
                        fields.push(key.clone());
                    }
                    pending_field = Some(key);
                }
            }
            ',' => {
                last_string = None;
                pending_field = None;
            }
            '{' => {
                let field = pending_field.take().or_else(|| {
                    frames.last().and_then(|frame| {
                        (frame.kind == JsonFrameKind::Array)
                            .then(|| frame.field.clone())
                            .flatten()
                    })
                });
                frames.push(JsonFrame {
                    field,
                    kind: JsonFrameKind::Object,
                    fields: Vec::new(),
                });
                last_string = None;
            }
            '[' => {
                frames.push(JsonFrame {
                    field: pending_field.take(),
                    kind: JsonFrameKind::Array,
                    fields: Vec::new(),
                });
                last_string = None;
            }
            '}' | ']' => {
                frames.pop();
                last_string = None;
                pending_field = None;
            }
            _ => {}
        }
    }

    fields.sort();
    fields.dedup();
    fields
}

fn object_path(frames: &[JsonFrame]) -> Vec<String> {
    frames
        .iter()
        .filter(|frame| frame.kind == JsonFrameKind::Object)
        .filter_map(|frame| frame.field.clone())
        .collect()
}

fn body_start_offset(text: &str) -> Option<usize> {
    text.find("\r\n\r\n")
        .map(|offset| offset + 4)
        .or_else(|| text.find("\n\n").map(|offset| offset + 2))
}

fn position_to_byte_offset(text: &str, position: Position) -> usize {
    let mut line = 0;
    let mut character = 0;

    for (offset, ch) in text.char_indices() {
        if line == position.line && character == position.character {
            return offset;
        }
        if ch == '\n' {
            line += 1;
            character = 0;
        } else {
            character += 1;
        }
    }

    text.len()
}

fn variable_definition_locations(
    request_uri: &Url,
    key: &str,
) -> Result<Vec<Location>> {
    Ok(variable_definitions(request_uri, key)?
        .into_iter()
        .map(|definition| definition.location)
        .collect())
}

fn variable_definitions(
    request_uri: &Url,
    key: &str,
) -> Result<Vec<VariableDefinition>> {
    let request_path = uri_to_path(request_uri)?;
    let resolved = resolve_path(&request_path)?;
    let target = get_target(&resolved.root_dir);
    let mut definitions = Vec::new();

    for (path, target) in [
        (resolved.root_dir.join(CONFIG_FILE), Some(target.as_str())),
        (
            resolved.root_dir.join(LOCAL_CONFIG_FILE),
            Some(target.as_str()),
        ),
        (resolved.toml_path(), None),
        (resolved.root_dir.join(DATA_FILE), None),
    ] {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let uri = Url::from_file_path(&path)
            .map_err(|_| anyhow::anyhow!("Invalid file path"))?;
        definitions.extend(
            find_toml_key_definitions(&text, key, target)
                .into_iter()
                .map(|definition| VariableDefinition {
                    location: Location {
                        uri: uri.clone(),
                        range: definition.range,
                    },
                    value: definition.value,
                }),
        );
    }

    Ok(definitions)
}

fn variable_at_position(
    text: &str,
    position: Position,
) -> Option<VariableOccurrence> {
    variable_occurrences(text).into_iter().find(|occurrence| {
        occurrence.range.start.line == position.line
            && occurrence.range.start.character <= position.character
            && position.character <= occurrence.range.end.character
    })
}

fn variable_occurrences(text: &str) -> Vec<VariableOccurrence> {
    let mut occurrences = Vec::new();

    for (line_index, line) in text.lines().enumerate() {
        let mut offset = 0;
        while let Some(start) = line[offset..].find("{{") {
            let start = offset + start;
            let Some(end) = line[start + 2..].find("}}") else {
                break;
            };
            let end = start + 2 + end;
            let key_start = start + 2;
            let key_end = end;
            let expression = &line[key_start..key_end];
            if let Some((key, key_offset)) = template_expression_key(expression)
            {
                let key_len = key.len();
                occurrences.push(VariableOccurrence {
                    key,
                    range: Range {
                        start: Position::new(
                            line_index as u32,
                            (key_start + key_offset) as u32,
                        ),
                        end: Position::new(
                            line_index as u32,
                            (key_start + key_offset + key_len) as u32,
                        ),
                    },
                });
            }
            offset = end + 2;
        }
    }

    occurrences
}

fn template_expression_key(expression: &str) -> Option<(String, usize)> {
    let before_filter = expression.split('|').next()?;
    let key = before_filter.trim();
    if key.is_empty()
        || !key.chars().all(|c| {
            c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'
        })
    {
        return None;
    }

    Some((key.to_string(), before_filter.find(key).unwrap_or_default()))
}

fn find_toml_key_definitions(
    text: &str,
    key: &str,
    target: Option<&str>,
) -> Vec<TomlKeyDefinition> {
    let mut definitions = Vec::new();

    let Ok(document) = ImDocument::parse(text) else {
        return definitions;
    };
    let root = document.as_table();

    push_toml_value_definition(text, root, key, &mut definitions);

    if let Some(table) = target
        .and_then(|target| root.get(target))
        .and_then(TomlEditItem::as_table)
    {
        push_toml_value_definition(text, table, key, &mut definitions);
    }

    if let Some(TomlEditItem::ArrayOfTables(tables)) = root.get(key) {
        for table in tables.iter() {
            if let Some(range) = table
                .span()
                .and_then(|span| array_table_key_range(text, span, key))
            {
                definitions.push(TomlKeyDefinition { range, value: None });
            }
        }
    }

    definitions
}

fn push_toml_value_definition(
    text: &str,
    table: &TomlEditTable,
    key: &str,
    definitions: &mut Vec<TomlKeyDefinition>,
) {
    let Some((key, item)) = table.get_key_value(key) else {
        return;
    };
    let TomlEditItem::Value(value) = item else {
        return;
    };
    let Some(range) = key.span().map(|span| byte_range(text, span)) else {
        return;
    };

    definitions.push(TomlKeyDefinition {
        range,
        value: parse_toml_edit_value(value),
    });
}

fn parse_toml_edit_value(value: &toml_edit::Value) -> Option<TomlValue> {
    toml::from_str::<TomlTable>(&format!("value = {value}"))
        .ok()?
        .remove("value")
}

fn byte_range(text: &str, range: ByteRange<usize>) -> Range {
    Range {
        start: byte_position(text, range.start),
        end: byte_position(text, range.end),
    }
}

fn array_table_key_range(
    text: &str,
    table_range: ByteRange<usize>,
    key: &str,
) -> Option<Range> {
    let header = text.get(table_range.clone())?.lines().next()?;
    let key_start = header.find(key)?;
    let start = table_range.start + key_start;

    Some(byte_range(text, start..start + key.len()))
}

fn byte_position(text: &str, offset: usize) -> Position {
    let mut line = 0;
    let mut line_start = 0;

    for (index, byte) in text.bytes().enumerate() {
        if index >= offset {
            break;
        }
        if byte == b'\n' {
            line += 1;
            line_start = index + 1;
        }
    }

    Position::new(line, (offset - line_start) as u32)
}

fn request_files(root_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(root_dir) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        if is_request_file(entry.path()) {
            files.push(entry.path().to_path_buf());
        }
    }

    Ok(files)
}

fn is_request_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "http" | "gql" | "graphql"
            )
        })
        .unwrap_or(false)
}

fn toml_value_display(value: &TomlValue) -> String {
    match value {
        TomlValue::String(value) => value.clone(),
        TomlValue::Integer(value) => value.to_string(),
        TomlValue::Float(value) => value.to_string(),
        TomlValue::Boolean(value) => value.to_string(),
        TomlValue::Array(_) => value.to_string().replace('\n', " "),
        _ => value.to_string(),
    }
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

    fn grpc_completion_fixture(body: &str) -> (Temp, PathBuf, String) {
        let tmp = Temp::new_dir().unwrap();
        fs::write(
            tmp.join("orders.proto"),
            r#"
                syntax = "proto3";
                package orders;

                service Orders {
                    rpc Create (CreateOrderRequest) returns (CreateOrderResponse);
                }

                enum State {
                    STATE_UNSPECIFIED = 0;
                    ACTIVE = 1;
                    DISABLED = 2;
                }

                message CreateOrderRequest {
                    string user_id = 1;
                    State state = 2;
                    Nested nested = 3;
                    repeated State states = 4;
                }

                message Nested {
                    State state = 1;
                    string note = 2;
                }

                message CreateOrderResponse { string id = 1; }
            "#,
        )
        .unwrap();
        let descriptors =
            protox::compile([tmp.join("orders.proto")], [tmp.as_path()])
                .unwrap();
        fs::write(tmp.join("protoset.bin"), descriptors.encode_to_vec())
            .unwrap();

        let request = tmp.join("create.http");
        let text = format!(
            "GRPC localhost:50051/orders.Orders/Create\nProtoset: ./protoset.bin\n\n{body}"
        );
        fs::write(&request, &text).unwrap();

        (tmp, request, text)
    }

    fn variable_fixture() -> (Temp, PathBuf, String) {
        let tmp = Temp::new_dir().unwrap();
        fs::write(
            tmp.join("hitman.toml"),
            "url = \"global\"\ntoken = \"config-token\"\n\n[dev]\nurl = \"dev\"\nbase_url = \"https://dev.example.com\"\ntarget_only = \"dev-only\"\n\n[alternative]\nbase_url = \"https://alternative.example.com\"\n\n[[label]]\nvalue = \"bug\"\nname = \"bug\"\n\n[[label]]\nvalue = \"documentation\"\nname = \"documentation\"\n",
        )
        .unwrap();
        fs::write(
            tmp.join("hitman.local.toml"),
            "[dev]\nurl = \"local-dev\"\n",
        )
        .unwrap();
        fs::write(tmp.join(".hitman-target"), "dev").unwrap();
        fs::write(tmp.join(".hitman-data.toml"), "token = \"data-token\"\n")
            .unwrap();
        fs::write(
            tmp.join("query.graphql"),
            "query { field(arg: \"{{url}}\") }",
        )
        .unwrap();

        let request = tmp.join("request.http");
        let text = "GET {{url}}\nPOST {{base_url}}/repos\nAuthorization: Bearer {{token}}\nGRPC {{target_only}}/addsvc.Add/Sum\n".to_string();
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
        assert_eq!(
            action.command.as_ref().unwrap().arguments.as_ref().unwrap()[0]
                ["methods"]
                .as_array()
                .unwrap()
                .len(),
            2
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
        assert_eq!(
            action.command.as_ref().unwrap().arguments.as_ref().unwrap()[0]
                ["methods"]
                .as_array()
                .unwrap()
                .len(),
            2
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

    #[test]
    fn completion_offers_grpc_body_fields() {
        let (_tmp, request, text) = grpc_completion_fixture("{\n  \n}\n");
        let uri = Url::from_file_path(request).unwrap();

        let completion =
            completion_for_position(&uri, &text, Position::new(4, 2))
                .unwrap()
                .unwrap();
        let CompletionResponse::Array(items) = completion else {
            panic!("expected completion array");
        };
        let labels =
            items.into_iter().map(|item| item.label).collect::<Vec<_>>();

        assert_eq!(labels, vec!["userId", "state", "nested", "states"]);
    }

    #[test]
    fn completion_omits_existing_grpc_body_fields() {
        let (_tmp, request, text) =
            grpc_completion_fixture("{\n  \"state\": \"ACTIVE\",\n  \n}\n");
        let uri = Url::from_file_path(request).unwrap();

        let completion =
            completion_for_position(&uri, &text, Position::new(5, 2))
                .unwrap()
                .unwrap();
        let CompletionResponse::Array(items) = completion else {
            panic!("expected completion array");
        };
        let labels =
            items.into_iter().map(|item| item.label).collect::<Vec<_>>();

        assert_eq!(labels, vec!["userId", "nested", "states"]);
    }

    #[test]
    fn completion_omits_existing_grpc_body_fields_below_cursor() {
        let (_tmp, request, text) =
            grpc_completion_fixture("{\n  \n  \"state\": \"ACTIVE\"\n}\n");
        let uri = Url::from_file_path(request).unwrap();

        let completion =
            completion_for_position(&uri, &text, Position::new(4, 2))
                .unwrap()
                .unwrap();
        let CompletionResponse::Array(items) = completion else {
            panic!("expected completion array");
        };
        let labels =
            items.into_iter().map(|item| item.label).collect::<Vec<_>>();

        assert_eq!(labels, vec!["userId", "nested", "states"]);
    }

    #[test]
    fn completion_shows_repeated_grpc_body_fields_as_arrays() {
        let (_tmp, request, text) = grpc_completion_fixture("{\n  \n}\n");
        let uri = Url::from_file_path(request).unwrap();

        let completion =
            completion_for_position(&uri, &text, Position::new(4, 2))
                .unwrap()
                .unwrap();
        let CompletionResponse::Array(items) = completion else {
            panic!("expected completion array");
        };
        let states = items
            .into_iter()
            .find(|item| item.label == "states")
            .unwrap();

        assert_eq!(states.detail.as_deref(), Some("array<orders.State>"));
        assert_eq!(states.insert_text.as_deref(), Some("\"states\": [$0]"));
    }

    #[test]
    fn completion_inserts_grpc_body_field_name_inside_key_string() {
        let (_tmp, request, text) = grpc_completion_fixture("{\n  \"user\n}\n");
        let uri = Url::from_file_path(request).unwrap();

        let completion =
            completion_for_position(&uri, &text, Position::new(4, 7))
                .unwrap()
                .unwrap();
        let CompletionResponse::Array(items) = completion else {
            panic!("expected completion array");
        };
        let user_id = items
            .into_iter()
            .find(|item| item.label == "userId")
            .unwrap();

        assert_eq!(user_id.filter_text.as_deref(), Some("\"userId\""));
        assert_eq!(user_id.insert_text.as_deref(), Some("\"userId\": \"$0\""));
        assert_eq!(
            user_id.insert_text_format,
            Some(InsertTextFormat::SNIPPET)
        );
        let Some(CompletionTextEdit::Edit(edit)) = user_id.text_edit else {
            panic!("expected completion text edit");
        };
        assert_eq!(edit.range.start, Position::new(4, 2));
        assert_eq!(edit.range.end, Position::new(4, 7));
        assert_eq!(edit.new_text, "\"userId\": \"$0\"");
    }

    #[test]
    fn completion_replaces_auto_paired_key_quote() {
        let (_tmp, request, text) = grpc_completion_fixture("{\n  \"user\"\n}\n");
        let uri = Url::from_file_path(request).unwrap();

        let completion =
            completion_for_position(&uri, &text, Position::new(4, 7))
                .unwrap()
                .unwrap();
        let CompletionResponse::Array(items) = completion else {
            panic!("expected completion array");
        };
        let user_id = items
            .into_iter()
            .find(|item| item.label == "userId")
            .unwrap();
        let Some(CompletionTextEdit::Edit(edit)) = user_id.text_edit else {
            panic!("expected completion text edit");
        };

        assert_eq!(edit.range.start, Position::new(4, 2));
        assert_eq!(edit.range.end, Position::new(4, 8));
        assert_eq!(edit.new_text, "\"userId\": \"$0\"");
    }

    #[test]
    fn completion_acceptance_opens_grpc_string_field_value_quote() {
        let (_tmp, request, text) = grpc_completion_fixture("{\n  \n}\n");
        let uri = Url::from_file_path(request).unwrap();

        let completion =
            completion_for_position(&uri, &text, Position::new(4, 2))
                .unwrap()
                .unwrap();
        let CompletionResponse::Array(items) = completion else {
            panic!("expected completion array");
        };
        let user_id = items
            .into_iter()
            .find(|item| item.label == "userId")
            .unwrap();

        assert_eq!(user_id.label, "userId");
        assert_eq!(user_id.filter_text.as_deref(), Some("\"userId\""));
        assert_eq!(user_id.insert_text.as_deref(), Some("\"userId\": \"$0\""));
        assert_eq!(
            user_id.insert_text_format,
            Some(InsertTextFormat::SNIPPET)
        );
        let Some(CompletionTextEdit::Edit(edit)) = user_id.text_edit else {
            panic!("expected completion text edit");
        };
        assert_eq!(edit.range.start, Position::new(4, 2));
        assert_eq!(edit.range.end, Position::new(4, 2));
        assert_eq!(edit.new_text, "\"userId\": \"$0\"");
    }

    #[test]
    fn completion_retriggers_after_grpc_enum_field_acceptance() {
        let (_tmp, request, text) = grpc_completion_fixture("{\n  \n}\n");
        let uri = Url::from_file_path(request).unwrap();

        let completion =
            completion_for_position(&uri, &text, Position::new(4, 2))
                .unwrap()
                .unwrap();
        let CompletionResponse::Array(items) = completion else {
            panic!("expected completion array");
        };
        let mut items = items.into_iter();
        let state = items.find(|item| item.label == "state").unwrap();

        let command = state.command.unwrap();
        assert_eq!(command.title, "Suggest");
        assert_eq!(command.command, "editor.action.triggerSuggest");
    }

    #[test]
    fn completion_does_not_retrigger_after_grpc_string_field_acceptance() {
        let (_tmp, request, text) = grpc_completion_fixture("{\n  \n}\n");
        let uri = Url::from_file_path(request).unwrap();

        let completion =
            completion_for_position(&uri, &text, Position::new(4, 2))
                .unwrap()
                .unwrap();
        let CompletionResponse::Array(items) = completion else {
            panic!("expected completion array");
        };
        let mut items = items.into_iter();
        let user_id = items.find(|item| item.label == "userId").unwrap();

        assert!(user_id.command.is_none());
    }

    #[test]
    fn completion_offers_grpc_enum_variants() {
        let (_tmp, request, text) =
            grpc_completion_fixture("{\n  \"state\": \"\n}\n");
        let uri = Url::from_file_path(request).unwrap();

        let completion =
            completion_for_position(&uri, &text, Position::new(3, 12))
                .unwrap()
                .unwrap();
        let CompletionResponse::Array(items) = completion else {
            panic!("expected completion array");
        };
        let labels =
            items.into_iter().map(|item| item.label).collect::<Vec<_>>();

        assert_eq!(labels, vec!["STATE_UNSPECIFIED", "ACTIVE", "DISABLED"]);
    }

    #[test]
    fn completion_moves_cursor_after_grpc_enum_variant_without_comma() {
        let (_tmp, request, text) =
            grpc_completion_fixture("{\n  \"state\": \"\n}\n");
        let uri = Url::from_file_path(request).unwrap();

        let completion =
            completion_for_position(&uri, &text, Position::new(3, 12))
                .unwrap()
                .unwrap();
        let CompletionResponse::Array(items) = completion else {
            panic!("expected completion array");
        };
        let active = items
            .into_iter()
            .find(|item| item.label == "ACTIVE")
            .unwrap();

        assert_eq!(active.insert_text.as_deref(), Some("ACTIVE$0"));
        assert_eq!(
            active.insert_text_format,
            Some(InsertTextFormat::SNIPPET)
        );
    }

    #[test]
    fn completion_moves_cursor_after_grpc_enum_value_closing_quote() {
        let (_tmp, request, text) =
            grpc_completion_fixture("{\n  \"state\": \"\"\n}\n");
        let uri = Url::from_file_path(request).unwrap();
        let cursor = byte_position(
            &text,
            text.find("\"state\": \"").unwrap() + "\"state\": \"".len(),
        );

        let completion = completion_for_position(&uri, &text, cursor)
            .unwrap()
            .unwrap();
        let CompletionResponse::Array(items) = completion else {
            panic!("expected completion array");
        };
        let active = items
            .into_iter()
            .find(|item| item.label == "ACTIVE")
            .unwrap();
        let Some(CompletionTextEdit::Edit(edit)) = active.text_edit else {
            panic!("expected completion text edit");
        };

        assert_eq!(edit.range.start, cursor);
        assert_eq!(edit.range.end, cursor);
        assert_eq!(edit.new_text, "ACTIVE\"$0");
        let additional = active.additional_text_edits.unwrap();
        assert_eq!(additional.len(), 1);
        assert_eq!(additional[0].range.start, cursor);
        assert_eq!(additional[0].range.end.character, cursor.character + 1);
        assert_eq!(additional[0].new_text, "");
    }

    #[test]
    fn completion_does_not_offer_enum_variants_after_completed_value() {
        let (_tmp, request, text) = grpc_completion_fixture(
            "{\n  \"state\": \"ACTIVE\"\n  \"\n}\n",
        );
        let uri = Url::from_file_path(request).unwrap();

        let completion =
            completion_for_position(&uri, &text, Position::new(5, 3)).unwrap();

        assert!(completion.is_none());
    }

    #[test]
    fn completion_follows_nested_grpc_messages() {
        let (_tmp, request, text) =
            grpc_completion_fixture("{\n  \"nested\": {\n    \n  }\n}\n");
        let uri = Url::from_file_path(request).unwrap();

        let completion =
            completion_for_position(&uri, &text, Position::new(5, 4))
                .unwrap()
                .unwrap();
        let CompletionResponse::Array(items) = completion else {
            panic!("expected completion array");
        };
        let labels =
            items.into_iter().map(|item| item.label).collect::<Vec<_>>();

        assert_eq!(labels, vec!["state", "note"]);
    }

    #[test]
    fn finds_variable_before_filters() {
        let text =
            r#""labels": ["{{ label | select_multiple | join('", "') }}"]"#;

        let occurrences = variable_occurrences(text);

        assert_eq!(occurrences.len(), 1);
        assert_eq!(occurrences[0].key, "label");
        assert_eq!(occurrences[0].range.start, Position::new(0, 15));
        assert_eq!(occurrences[0].range.end, Position::new(0, 20));
    }

    #[test]
    fn definition_supports_array_table_variables() {
        let (tmp, _request, _) = variable_fixture();
        let request = tmp.join("create_issue.http");
        let text =
            r#""labels": ["{{ label | select_multiple | join('", "') }}"]"#;
        fs::write(&request, text).unwrap();
        let uri = Url::from_file_path(request).unwrap();

        let definitions =
            definition_for_position(&uri, text, Position::new(0, 17))
                .unwrap()
                .unwrap();

        assert_eq!(definitions.len(), 2);
        assert!(definitions.iter().all(|definition| {
            definition.uri
                == Url::from_file_path(tmp.join("hitman.toml")).unwrap()
        }));
        assert_eq!(definitions[0].range.start, Position::new(11, 2));
        assert_eq!(definitions[1].range.start, Position::new(15, 2));
    }

    #[test]
    fn definition_returns_all_config_declarations() {
        let (tmp, request, text) = variable_fixture();
        let uri = Url::from_file_path(request).unwrap();

        let definitions =
            definition_for_position(&uri, &text, Position::new(0, 7))
                .unwrap()
                .unwrap();

        assert_eq!(definitions.len(), 3);
        assert!(definitions.iter().any(|definition| {
            definition.uri
                == Url::from_file_path(tmp.join("hitman.toml")).unwrap()
                && definition.range.start == Position::new(0, 0)
        }));
        assert!(definitions.iter().any(|definition| {
            definition.uri
                == Url::from_file_path(tmp.join("hitman.toml")).unwrap()
                && definition.range.start == Position::new(4, 0)
        }));
        assert!(definitions.iter().any(|definition| {
            definition.uri
                == Url::from_file_path(tmp.join("hitman.local.toml")).unwrap()
                && definition.range.start == Position::new(1, 0)
        }));
    }

    #[test]
    fn definition_returns_config_and_data_declarations() {
        let (tmp, request, text) = variable_fixture();
        let uri = Url::from_file_path(request).unwrap();

        let definitions =
            definition_for_position(&uri, &text, Position::new(2, 25))
                .unwrap()
                .unwrap();

        assert_eq!(definitions.len(), 2);
        assert!(definitions.iter().any(|definition| {
            definition.uri
                == Url::from_file_path(tmp.join("hitman.toml")).unwrap()
                && definition.range.start == Position::new(1, 0)
        }));
        assert!(definitions.iter().any(|definition| {
            definition.uri
                == Url::from_file_path(tmp.join(".hitman-data.toml")).unwrap()
                && definition.range.start == Position::new(0, 0)
        }));
    }

    #[test]
    fn definition_ignores_other_targets() {
        let (tmp, request, text) = variable_fixture();
        let uri = Url::from_file_path(request).unwrap();

        let definitions =
            definition_for_position(&uri, &text, Position::new(1, 9))
                .unwrap()
                .unwrap();

        assert_eq!(definitions.len(), 1);
        assert_eq!(
            definitions[0].uri,
            Url::from_file_path(tmp.join("hitman.toml")).unwrap()
        );
        assert_eq!(definitions[0].range.start, Position::new(5, 0));
    }

    #[test]
    fn hover_is_empty_for_multiple_declarations() {
        let (_tmp, request, text) = variable_fixture();
        let uri = Url::from_file_path(request).unwrap();

        let hover =
            hover_for_position(&uri, &text, Position::new(0, 7)).unwrap();

        assert!(hover.is_none());
    }

    #[test]
    fn hover_shows_value_for_single_declaration() {
        let (_tmp, request, text) = variable_fixture();
        let uri = Url::from_file_path(request).unwrap();

        let hover = hover_for_position(&uri, &text, Position::new(3, 8))
            .unwrap()
            .unwrap();

        let HoverContents::Scalar(MarkedString::String(contents)) =
            hover.contents
        else {
            panic!("expected string hover");
        };
        assert_eq!(contents, "`dev-only`");
    }

    #[test]
    fn hover_uses_current_target() {
        let (_tmp, request, text) = variable_fixture();
        let uri = Url::from_file_path(request).unwrap();

        let hover = hover_for_position(&uri, &text, Position::new(1, 9))
            .unwrap()
            .unwrap();

        let HoverContents::Scalar(MarkedString::String(contents)) =
            hover.contents
        else {
            panic!("expected string hover");
        };
        assert_eq!(contents, "`https://dev.example.com`");
    }

    #[test]
    fn references_include_http_graphql_and_declarations() {
        let (tmp, request, text) = variable_fixture();
        let uri = Url::from_file_path(request).unwrap();

        let references =
            references_for_position(&uri, &text, Position::new(0, 7), true)
                .unwrap()
                .unwrap();

        assert!(references.iter().any(|location| location.uri == uri));
        assert!(references.iter().any(|location| {
            location.uri
                == Url::from_file_path(tmp.join("query.graphql")).unwrap()
        }));
        assert!(references.iter().any(|location| {
            location.uri
                == Url::from_file_path(tmp.join("hitman.local.toml")).unwrap()
        }));
    }
}

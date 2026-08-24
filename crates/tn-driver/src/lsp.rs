use lsp_server::{Connection, ErrorCode, Message, Response};
use lsp_types::{
    Diagnostic as LspDiagnostic, DiagnosticSeverity, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, Position, PublishDiagnosticsParams,
    Range, ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind,
    notification::{
        DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification,
    },
};
use std::collections::HashMap;
use std::path::PathBuf;
use tn_diagnostics::Severity;
use tn_syntax::{IncrementalDocument, TextEdit};

type LspResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Runs the syntax-aware language server over standard input and output.
///
/// # Errors
///
/// Returns an error when JSON-RPC transport, protocol deserialization, or I/O fails.
pub fn run_lsp() -> LspResult<()> {
    let (connection, io_threads) = Connection::stdio();
    let capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(
            TextDocumentSyncKind::INCREMENTAL,
        )),
        ..ServerCapabilities::default()
    };
    connection.initialize(serde_json::to_value(capabilities)?)?;
    let mut documents = HashMap::new();

    for message in &connection.receiver {
        match message {
            Message::Request(request) => {
                if connection.handle_shutdown(&request)? {
                    break;
                }
                let response = Response::new_err(
                    request.id.clone(),
                    ErrorCode::MethodNotFound as i32,
                    format!("unsupported request: {}", request.method),
                );
                connection.sender.send(Message::Response(response))?;
            }
            Message::Response(_) => {}
            Message::Notification(notification) => match notification.method.as_str() {
                DidOpenTextDocument::METHOD => {
                    let params: DidOpenTextDocumentParams =
                        serde_json::from_value(notification.params.clone())?;
                    let document = params.text_document;
                    let uri = document.uri.clone();
                    let state = IncrementalDocument::new(uri.as_str(), document.text);
                    publish(&connection, &uri, document.version, &state)?;
                    documents.insert(uri.as_str().to_owned(), state);
                }
                DidChangeTextDocument::METHOD => {
                    let params: DidChangeTextDocumentParams =
                        serde_json::from_value(notification.params.clone())?;
                    let uri = params.text_document.uri;
                    if let Some(document) = documents.get_mut(uri.as_str()) {
                        apply_changes(document, params.content_changes)?;
                        publish(&connection, &uri, params.text_document.version, document)?;
                    }
                }
                DidCloseTextDocument::METHOD => {
                    let params: DidCloseTextDocumentParams =
                        serde_json::from_value(notification.params.clone())?;
                    documents.remove(params.text_document.uri.as_str());
                    let notification = lsp_server::Notification::new(
                        lsp_types::notification::PublishDiagnostics::METHOD.into(),
                        PublishDiagnosticsParams::new(params.text_document.uri, Vec::new(), None),
                    );
                    connection
                        .sender
                        .send(Message::Notification(notification))?;
                }
                _ => {}
            },
        }
    }
    drop(connection);
    io_threads.join()?;
    Ok(())
}

fn apply_changes(
    document: &mut IncrementalDocument,
    changes: Vec<lsp_types::TextDocumentContentChangeEvent>,
) -> LspResult<()> {
    for change in changes {
        if let Some(range) = change.range {
            let start = byte_offset(document.source(), range.start)?;
            let end = byte_offset(document.source(), range.end)?;
            document.apply_edit(TextEdit {
                range: start..end,
                replacement: change.text,
            })?;
        } else {
            *document = IncrementalDocument::new("<lsp>", change.text);
        }
    }
    Ok(())
}

fn publish(
    connection: &Connection,
    uri: &lsp_types::Uri,
    version: i32,
    document: &IncrementalDocument,
) -> LspResult<()> {
    let mut diagnostics: Vec<LspDiagnostic> = document
        .parse()
        .diagnostics()
        .iter()
        .map(|diagnostic| {
            let start = position(
                document.source(),
                diagnostic.primary.span.byte_start as usize,
            );
            let end = position(document.source(), diagnostic.primary.span.byte_end as usize);
            LspDiagnostic {
                range: Range::new(start, end),
                severity: Some(match diagnostic.severity {
                    Severity::Error => DiagnosticSeverity::ERROR,
                    Severity::Warning => DiagnosticSeverity::WARNING,
                    Severity::Note => DiagnosticSeverity::INFORMATION,
                }),
                code: Some(lsp_types::NumberOrString::String(
                    diagnostic.condition.as_str().into(),
                )),
                source: Some("tn".into()),
                message: diagnostic.message.clone(),
                ..LspDiagnostic::default()
            }
        })
        .collect();
    if diagnostics.is_empty() {
        diagnostics.extend(semantic_diagnostics(document.source()));
    }
    let notification = lsp_server::Notification::new(
        lsp_types::notification::PublishDiagnostics::METHOD.into(),
        PublishDiagnosticsParams::new(uri.clone(), diagnostics, Some(version)),
    );
    connection
        .sender
        .send(Message::Notification(notification))?;
    Ok(())
}

fn semantic_diagnostics(source: &str) -> Vec<LspDiagnostic> {
    let Ok(directory) = tempfile::tempdir() else {
        return Vec::new();
    };
    let entry = directory.path().join("main.tn");
    if std::fs::write(&entry, source).is_err() {
        return Vec::new();
    }
    let project = crate::Project {
        root: directory.path().to_path_buf(),
        entry: entry.clone(),
        config: crate::ProjectConfig {
            entry: PathBuf::from("main.tn"),
            out_dir: PathBuf::from("build"),
            target: crate::Target::host().unwrap_or(crate::Target::Aarch64AppleDarwin),
            profile: crate::Profile::Debug,
            emit: crate::Emit::Executable,
            sanitizers: Vec::new(),
            link: crate::LinkConfig::default(),
            support_mode: super::project::SupportMode::None,
        },
        config_path: None,
    };
    crate::check_project(&project)
        .diagnostics
        .into_iter()
        .filter(|diagnostic| {
            diagnostic.primary.span.byte_end as usize <= source.len()
                && diagnostic.primary.span.byte_start <= diagnostic.primary.span.byte_end
        })
        .map(|diagnostic| {
            let start = position(source, diagnostic.primary.span.byte_start as usize);
            let end = position(source, diagnostic.primary.span.byte_end as usize);
            LspDiagnostic {
                range: Range::new(start, end),
                severity: Some(match diagnostic.severity {
                    Severity::Error => DiagnosticSeverity::ERROR,
                    Severity::Warning => DiagnosticSeverity::WARNING,
                    Severity::Note => DiagnosticSeverity::INFORMATION,
                }),
                code: Some(lsp_types::NumberOrString::String(
                    diagnostic.condition.as_str().into(),
                )),
                source: Some("tn".into()),
                message: diagnostic.message,
                ..LspDiagnostic::default()
            }
        })
        .collect()
}

fn byte_offset(source: &str, target: Position) -> LspResult<usize> {
    let mut offset = 0;
    for (line, segment) in source.split_inclusive('\n').enumerate() {
        if line == usize::try_from(target.line).unwrap_or(usize::MAX) {
            let content = segment.strip_suffix('\n').unwrap_or(segment);
            let mut utf16_column = 0_u32;
            for (byte, character) in content.char_indices() {
                if utf16_column == target.character {
                    return Ok(offset + byte);
                }
                utf16_column += if character.len_utf16() == 2 { 2 } else { 1 };
                if utf16_column > target.character {
                    return Err("LSP position splits a UTF-16 scalar".into());
                }
            }
            if utf16_column == target.character {
                return Ok(offset + content.len());
            }
            return Err("LSP character position is past the end of the line".into());
        }
        offset += segment.len();
    }
    Err("LSP line position is past the end of the document".into())
}

fn position(source: &str, target: usize) -> Position {
    let target = target.min(source.len());
    let prefix = &source[..target];
    let line =
        u32::try_from(prefix.bytes().filter(|byte| *byte == b'\n').count()).unwrap_or(u32::MAX);
    let line_start = prefix.rfind('\n').map_or(0, |index| index + 1);
    let character = source[line_start..target]
        .chars()
        .fold(0_u32, |units, character| {
            units.saturating_add(if character.len_utf16() == 2 { 2 } else { 1 })
        });
    Position::new(line, character)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsp_positions_count_utf16_code_units() {
        let source = "a😀b\nλ";
        assert_eq!(byte_offset(source, Position::new(0, 3)).unwrap(), 5);
        assert_eq!(position(source, 5), Position::new(0, 3));
        assert!(byte_offset(source, Position::new(0, 2)).is_err());
    }

    #[test]
    fn semantic_document_diagnostics_use_the_compiler_pipeline() {
        let diagnostics = semantic_diagnostics("@Unknown\nfunction main(): void {}\n");
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic.code
                == Some(lsp_types::NumberOrString::String(
                    "TYPE_UNKNOWN_ATTRIBUTE".into()
                ))),
            "{diagnostics:?}"
        );
    }
}

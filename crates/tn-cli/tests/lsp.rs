use std::io::Write;
use std::process::{Command, Stdio};

fn frame(json: &str) -> String {
    format!("Content-Length: {}\r\n\r\n{json}", json.len())
}

#[test]
fn lsp_publishes_structured_syntax_diagnostics() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tn"))
        .arg("lsp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("tn lsp starts");
    let messages = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}"#,
        r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///test.tn","languageId":"typenative","version":1,"text":"function main(): void { const x = ; }"}}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"shutdown","params":null}"#,
        r#"{"jsonrpc":"2.0","method":"exit","params":null}"#,
    ]
    .into_iter()
    .map(frame)
    .collect::<String>();
    child
        .stdin
        .take()
        .expect("child stdin exists")
        .write_all(messages.as_bytes())
        .expect("protocol messages write");
    let output = child.wait_with_output().expect("language server exits");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("LSP output is UTF-8");
    assert!(stdout.contains("textDocument/publishDiagnostics"));
    assert!(stdout.contains("SYNTAX_EXPECTED_EXPRESSION"));
}

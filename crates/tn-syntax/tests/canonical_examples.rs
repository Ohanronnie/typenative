use std::path::{Path, PathBuf};

fn extract_tn_blocks(markdown: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current = None::<String>;
    for line in markdown.lines() {
        if line == "```tn" {
            current = Some(String::new());
        } else if line == "```" {
            if let Some(block) = current.take() {
                blocks.push(block);
            }
        } else if let Some(block) = current.as_mut() {
            block.push_str(line);
            block.push('\n');
        }
    }
    blocks
}

fn workspace_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

#[test]
fn every_canonical_tn_example_parses_in_its_documented_context() {
    let sources = ["README.md", "docs/language-spec.md"];
    let mut checked = 0;
    for relative in sources {
        let path = workspace_path(relative);
        let markdown = std::fs::read_to_string(&path).expect("canonical document is readable");
        for (index, block) in extract_tn_blocks(&markdown).into_iter().enumerate() {
            let contextual =
                if block.contains("if (port !== undefined)") || block.starts_with("try {") {
                    format!("function documentedContext(): void {{\n{block}}}\n")
                } else {
                    block
                };
            let name = format!("{}#tn-{}", path.display(), index + 1);
            let parsed = tn_syntax::parse(&name, contextual.as_bytes());
            assert!(
                parsed.is_success(),
                "{name} produced diagnostics: {:#?}\n{contextual}",
                parsed.diagnostics()
            );
            checked += 1;
        }
    }
    assert_eq!(checked, 1, "canonical TypeNative example count changed");
}

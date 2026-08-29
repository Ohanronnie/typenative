use std::process::Command;

#[test]
fn builds_f32_literals_with_their_native_width() {
    let directory = tempfile::tempdir().expect("f32 fixture directory");
    let source = directory.path().join("main.tn");
    std::fs::write(
        &source,
        "function main(): i32 {\n  const value: f32 = 1.5f32;\n  if (value > 1.0f32) { return 0; }\n  return 1;\n}\n",
    )
    .expect("write f32 fixture");
    let output = directory.path().join("f32-literals");
    let result = Command::new(env!("CARGO_BIN_EXE_tn"))
        .args([
            "build",
            source.to_str().expect("UTF-8 f32 fixture path"),
            "--profile",
            "debug",
            "--emit",
            "executable",
            "--out",
            output.to_str().expect("UTF-8 f32 output path"),
        ])
        .output()
        .expect("build f32 fixture");
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

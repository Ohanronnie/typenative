use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::process::Command as ProcessCommand;

#[derive(Parser)]
#[command(name = "tn", version, about = "TypeNative compiler and tooling")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Build(BuildArgs),
    Run(RunArgs),
    Check(CheckArgs),
    Test(TestArgs),
    Fmt(FmtArgs),
    Doc(DocArgs),
    Lsp,
}

#[derive(clap::Args)]
struct BuildArgs {
    path: Option<PathBuf>,
    #[arg(long)]
    target: Option<Target>,
    #[arg(long, value_enum)]
    profile: Option<Profile>,
    #[arg(long, value_enum)]
    emit: Option<Emit>,
    #[arg(long)]
    out: Option<PathBuf>,
    #[arg(long, help = "print compiler phase timings to stderr")]
    timings: bool,
    #[arg(long = "link-library")]
    link_libraries: Vec<String>,
    #[arg(long = "link-search")]
    link_search_paths: Vec<PathBuf>,
    #[arg(long = "link-argument")]
    link_arguments: Vec<String>,
}

#[derive(clap::Args)]
struct RunArgs {
    path: Option<PathBuf>,
    #[arg(long)]
    target: Option<Target>,
    #[arg(long, value_enum)]
    profile: Option<Profile>,
    #[arg(long, help = "print compiler phase timings to stderr")]
    timings: bool,
    #[arg(last = true)]
    arguments: Vec<String>,
}

#[derive(clap::Args)]
struct CheckArgs {
    path: Option<PathBuf>,
    #[arg(long)]
    target: Option<Target>,
    #[arg(long, value_enum)]
    profile: Option<Profile>,
    #[arg(long)]
    json: bool,
    #[arg(long, help = "print compiler phase timings to stderr")]
    timings: bool,
}

#[derive(clap::Args)]
struct TestArgs {
    path: Option<PathBuf>,
    filter: Option<String>,
}

#[derive(clap::Args)]
struct FmtArgs {
    path: Option<PathBuf>,
    #[arg(long)]
    check: bool,
}

#[derive(clap::Args)]
struct DocArgs {
    path: Option<PathBuf>,
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Profile {
    Debug,
    Optimized,
}

#[derive(Clone, Copy, ValueEnum)]
enum Target {
    #[value(name = "aarch64-apple-darwin")]
    Aarch64AppleDarwin,
}

#[derive(Clone, Copy, ValueEnum)]
enum Emit {
    Executable,
    Object,
    LlvmIr,
    Bitcode,
    Assembly,
    SharedLibrary,
    NodeAddon,
}

fn main() -> std::process::ExitCode {
    match Cli::parse().command {
        Command::Build(args) => build(&args),
        Command::Run(args) => run(&args),
        Command::Check(args) => check(&args),
        Command::Fmt(args) => fmt(&args),
        Command::Test(args) => test(&args),
        Command::Doc(args) => doc(&args),
        Command::Lsp => match tn_driver::run_lsp() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("language server failed: {error}");
                std::process::ExitCode::from(2)
            }
        },
    }
}

fn build(args: &BuildArgs) -> std::process::ExitCode {
    let mut project = match tn_driver::load_project(args.path.as_deref()) {
        Ok(project) => project,
        Err(error) => {
            eprintln!("{error}");
            return std::process::ExitCode::from(2);
        }
    };
    apply_target_profile(&mut project, args.target, args.profile);
    if let Some(emit) = args.emit {
        project.config.emit = driver_emit(emit);
    }
    project
        .config
        .link
        .libraries
        .extend(args.link_libraries.iter().cloned());
    project
        .config
        .link
        .search_paths
        .extend(args.link_search_paths.iter().cloned());
    project
        .config
        .link
        .arguments
        .extend(args.link_arguments.iter().cloned());
    match tn_driver::build_project_with_timings(&project, args.out.as_deref(), args.timings) {
        Ok(output) => {
            println!("{}", output.product.display());
            std::process::ExitCode::SUCCESS
        }
        Err(error) => render_build_error(&error),
    }
}

fn run(args: &RunArgs) -> std::process::ExitCode {
    let mut project = match tn_driver::load_project(args.path.as_deref()) {
        Ok(project) => project,
        Err(error) => {
            eprintln!("{error}");
            return std::process::ExitCode::from(2);
        }
    };
    apply_target_profile(&mut project, args.target, args.profile);
    project.config.emit = tn_driver::Emit::Executable;
    let output = match tn_driver::build_project_with_timings(&project, None, args.timings) {
        Ok(output) => output,
        Err(error) => return render_build_error(&error),
    };
    match ProcessCommand::new(&output.product)
        .args(&args.arguments)
        .status()
    {
        Ok(status) if status.success() => std::process::ExitCode::SUCCESS,
        Ok(status) => std::process::ExitCode::from(
            status
                .code()
                .and_then(|code| u8::try_from(code).ok())
                .unwrap_or(1),
        ),
        Err(error) => {
            eprintln!("failed to run {}: {error}", output.product.display());
            std::process::ExitCode::from(2)
        }
    }
}

fn render_build_error(error: &tn_driver::BuildError) -> std::process::ExitCode {
    if let tn_driver::BuildError::Diagnostics(diagnostics) = &error {
        for diagnostic in diagnostics {
            eprint!("{}", tn_diagnostics::render_text(diagnostic));
        }
        std::process::ExitCode::FAILURE
    } else {
        eprintln!("{error}");
        std::process::ExitCode::from(2)
    }
}

fn apply_target_profile(
    project: &mut tn_driver::Project,
    target: Option<Target>,
    profile: Option<Profile>,
) {
    if let Some(target) = target {
        project.config.target = match target {
            Target::Aarch64AppleDarwin => tn_driver::Target::Aarch64AppleDarwin,
        };
    }
    if let Some(profile) = profile {
        project.config.profile = match profile {
            Profile::Debug => tn_driver::Profile::Debug,
            Profile::Optimized => tn_driver::Profile::Optimized,
        };
    }
}

const fn driver_emit(emit: Emit) -> tn_driver::Emit {
    match emit {
        Emit::Executable => tn_driver::Emit::Executable,
        Emit::Object => tn_driver::Emit::Object,
        Emit::LlvmIr => tn_driver::Emit::LlvmIr,
        Emit::Bitcode => tn_driver::Emit::Bitcode,
        Emit::Assembly => tn_driver::Emit::Assembly,
        Emit::SharedLibrary => tn_driver::Emit::SharedLibrary,
        Emit::NodeAddon => tn_driver::Emit::NodeAddon,
    }
}

fn fmt(args: &FmtArgs) -> std::process::ExitCode {
    let path = args.path.clone().unwrap_or_else(|| PathBuf::from("."));
    let files = match source_files(&path) {
        Ok(files) => files,
        Err(error) => {
            eprintln!("{}: {error}", path.display());
            return std::process::ExitCode::from(2);
        }
    };
    let mut changed = false;
    let mut failed = false;
    for file in files {
        let bytes = match std::fs::read(&file) {
            Ok(bytes) => bytes,
            Err(error) => {
                eprintln!("{}: {error}", file.display());
                failed = true;
                continue;
            }
        };
        let output = tn_driver::format_source(&file, &bytes);
        for diagnostic in &output.diagnostics {
            eprint!("{}", tn_diagnostics::render_text(diagnostic));
        }
        if !output.diagnostics.is_empty() {
            failed = true;
            continue;
        }
        if output.formatted.as_bytes() != bytes {
            changed = true;
            if args.check {
                eprintln!("{} is not formatted", file.display());
            } else if let Err(error) = std::fs::write(&file, output.formatted) {
                eprintln!("{}: {error}", file.display());
                failed = true;
            }
        }
    }
    if failed || (args.check && changed) {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
}

fn source_files(path: &std::path::Path) -> std::io::Result<Vec<PathBuf>> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    let mut pending = vec![path.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let mut entries = std::fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries.into_iter().rev() {
            let entry_path = entry.path();
            if entry_path.is_dir() {
                pending.push(entry_path);
            } else if entry_path
                .extension()
                .is_some_and(|extension| extension == "tn")
            {
                files.push(entry_path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn check(args: &CheckArgs) -> std::process::ExitCode {
    let project = match tn_driver::load_project(args.path.as_deref()) {
        Ok(project) => project,
        Err(error) => {
            eprintln!("{error}");
            return std::process::ExitCode::from(2);
        }
    };
    let mut project = project;
    apply_target_profile(&mut project, args.target, args.profile);
    let output = tn_driver::check_project_with_timings(&project, args.timings);
    for diagnostic in &output.diagnostics {
        if args.json {
            match tn_diagnostics::render_json(diagnostic) {
                Ok(rendered) => println!("{rendered}"),
                Err(error) => {
                    eprintln!("failed to serialize diagnostic: {error}");
                    return std::process::ExitCode::from(2);
                }
            }
        } else {
            eprint!("{}", tn_diagnostics::render_text(diagnostic));
        }
    }
    if output.is_success() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

fn test(args: &TestArgs) -> std::process::ExitCode {
    let project = match tn_driver::load_project(args.path.as_deref()) {
        Ok(project) => project,
        Err(error) => {
            eprintln!("{error}");
            return std::process::ExitCode::from(2);
        }
    };
    let output = match tn_driver::run_tests(&project, args.filter.as_deref()) {
        Ok(output) => output,
        Err(error) => return render_build_error(&error),
    };
    for line in &output.lines {
        println!("{line}");
    }
    println!("\nresult: {} passed; {} total", output.passed, output.total);
    if output.is_success() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

fn doc(args: &DocArgs) -> std::process::ExitCode {
    let project = match tn_driver::load_project(args.path.as_deref()) {
        Ok(project) => project,
        Err(error) => {
            eprintln!("{error}");
            return std::process::ExitCode::from(2);
        }
    };
    let documentation = match tn_driver::generate_docs(&project) {
        Ok(documentation) => documentation,
        Err(error) => return render_build_error(&error),
    };
    if let Some(output) = args.out.as_deref() {
        if let Some(parent) = output.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            eprintln!("{}: {error}", output.display());
            return std::process::ExitCode::from(2);
        }
        if let Err(error) = std::fs::write(output, documentation) {
            eprintln!("{}: {error}", output.display());
            return std::process::ExitCode::from(2);
        }
    } else {
        print!("{documentation}");
    }
    std::process::ExitCode::SUCCESS
}

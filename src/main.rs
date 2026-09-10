fn main() {
    env_logger::init();
    #[cfg(target_os = "macos")]
    {
        let mut args = std::env::args_os().skip(1);
        let smoke_directory = match args.next() {
            None => None,
            Some(arg) if arg == "--smoke-test" => {
                let root = args
                    .next()
                    .map(std::path::PathBuf::from)
                    .expect("--smoke-test requires a project-local output directory");
                let directory = root.join(format!("ui-smoke-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir_all(&directory).expect("create smoke-test directory");
                let directory =
                    std::fs::canonicalize(directory).expect("resolve smoke-test directory");
                println!("Smoke artifacts: {}", directory.display());
                Some(directory)
            }
            Some(_) => {
                eprintln!("Usage: oculi [--smoke-test OUTPUT_DIRECTORY]");
                std::process::exit(2);
            }
        };
        oculi::ui::run(smoke_directory);
    }
    #[cfg(not(target_os = "macos"))]
    eprintln!("Oculi requires macOS 15 or later.");
}

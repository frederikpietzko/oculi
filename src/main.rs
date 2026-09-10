fn main() {
    env_logger::init();
    #[cfg(target_os = "macos")]
    {
        let mut args = std::env::args_os().skip(1);
        let smoke_directory = match args.next() {
            None => None,
            Some(arg) if arg == "--recording-smoke-test" => {
                let report = args
                    .next()
                    .map(std::path::PathBuf::from)
                    .expect("--recording-smoke-test requires a new report file path");
                std::process::exit(recording_smoke(report));
            }
            Some(arg) if arg == "--preview-smoke-test" => {
                let report = args
                    .next()
                    .map(std::path::PathBuf::from)
                    .expect("--preview-smoke-test requires a new report file path");
                std::process::exit(preview_smoke(report));
            }
            Some(arg) if arg == "--device-smoke-test" => {
                let report = args
                    .next()
                    .map(std::path::PathBuf::from)
                    .expect("--device-smoke-test requires a new report file path");
                std::process::exit(device_smoke(report));
            }
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
                eprintln!(
                    "Usage: oculi [--smoke-test OUTPUT_DIRECTORY | --device-smoke-test REPORT_FILE | --preview-smoke-test REPORT_FILE | --recording-smoke-test REPORT_FILE]"
                );
                std::process::exit(2);
            }
        };
        oculi::ui::run(smoke_directory);
    }
    #[cfg(not(target_os = "macos"))]
    eprintln!("Oculi requires macOS 15 or later.");
}

#[cfg(target_os = "macos")]
fn recording_smoke(path: std::path::PathBuf) -> i32 {
    use std::io::Write;
    let mut report = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .expect("create a new recording smoke report");
    writeln!(report, "Bundle: {:?}\nNo permission prompts or saved configuration changes. Explicit smoke creates two media outputs.",
        objc2_foundation::NSBundle::mainBundle().bundleIdentifier().map(|id| id.to_string())).expect("write identity");
    report.flush().expect("flush identity");
    let (code, message) = match oculi::recording::runtime::recording_smoke_check() {
        Ok(message) => (if message.starts_with("SKIP") { 2 } else { 0 }, message),
        Err(error) => (1, format!("FAIL {error}")),
    };
    writeln!(report, "{message}\nCOMPLETE exit_code={code}").expect("write result");
    report.sync_all().expect("sync report");
    code
}

#[cfg(target_os = "macos")]
fn preview_smoke(path: std::path::PathBuf) -> i32 {
    use std::io::Write;
    let mut report = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .expect("create a new preview smoke report");
    writeln!(
        report,
        "Bundle: {:?}\nNo permission prompts, settings changes, or media files.",
        objc2_foundation::NSBundle::mainBundle()
            .bundleIdentifier()
            .map(|id| id.to_string())
    )
    .expect("write smoke identity");
    let (code, message) = match oculi::platform::macos::capture::preview_smoke_check() {
        Ok(message) => (if message.starts_with("SKIP") { 2 } else { 0 }, message),
        Err(error) => (1, format!("FAIL {error}")),
    };
    writeln!(report, "{message}\nCOMPLETE exit_code={code}").expect("write preview result");
    report.sync_all().expect("sync preview report");
    code
}

#[cfg(target_os = "macos")]
fn device_smoke(path: std::path::PathBuf) -> i32 {
    use std::io::Write;

    let mut report = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .expect("create a new device smoke report (existing files are never overwritten)");
    let bundle = objc2_foundation::NSBundle::mainBundle();
    writeln!(
        report,
        "Bundle: {:?}\nExecutable: {:?}\nNo permission prompts, settings changes, or media files.",
        bundle.bundleIdentifier().map(|id| id.to_string()),
        std::env::current_exe()
    )
    .expect("write smoke identity");
    report.flush().expect("flush smoke identity");
    // Device session setup/teardown stays off the AppKit main thread.
    std::thread::spawn(move || {
        let mut exit_code = 0;
        for camera in [true, false] {
            let message = match oculi::platform::macos::device_capture::smoke_check(camera) {
                Ok(message) => {
                    if message.starts_with("SKIP") && exit_code == 0 {
                        exit_code = 2;
                    }
                    message
                }
                Err(error) => {
                    exit_code = 1;
                    format!("FAIL {error}")
                }
            };
            writeln!(report, "{message}").expect("write device result");
            report.flush().expect("flush device result");
        }
        writeln!(report, "COMPLETE exit_code={exit_code}").expect("write completion");
        report.sync_all().expect("sync smoke report");
        exit_code
    })
    .join()
    .unwrap_or(1)
}

use std::process::Command;

#[test]
fn logging_child() {
    let Some(destination) = std::env::var_os("POLARIS_TEST_LOG_DESTINATION") else {
        return;
    };
    let path = (destination != "stdout").then(|| destination.into());
    polaris_agent::cli::initialize_logging(path).unwrap();
    tracing::info!("fixture startup info");
    tracing::warn!("fixture connection warning");
    tracing::debug!("fixture debug detail");
}

#[test]
fn logging_defaults_to_info_and_respects_environment_for_both_outputs() {
    let directory = std::env::temp_dir().join(format!("polaris-logging-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let cases = [
        (None, true, true, false),
        (Some(""), true, true, false),
        (Some("info"), true, true, false),
        (Some("warn"), false, true, false),
        (Some("debug"), true, true, true),
        (Some("off"), false, false, false),
        (Some("logging_test=invalid-level"), true, true, false),
    ];
    for file_output in [false, true] {
        for (index, (filter, info, warn, debug)) in cases.iter().enumerate() {
            let path = directory.join(format!("{file_output}-{index}.log"));
            let mut child = Command::new(std::env::current_exe().unwrap());
            child
                .args(["--exact", "logging_child", "--nocapture"])
                .env_remove("RUST_LOG")
                .env(
                    "POLARIS_TEST_LOG_DESTINATION",
                    if file_output {
                        path.as_os_str()
                    } else {
                        std::ffi::OsStr::new("stdout")
                    },
                );
            if let Some(filter) = filter {
                child.env("RUST_LOG", filter);
            }
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let logged = if file_output {
                std::fs::read_to_string(&path).unwrap()
            } else {
                String::from_utf8(output.stdout).unwrap()
            };
            assert!(
                !logged.contains('\u{1b}'),
                "redirected logs must not include ANSI escapes"
            );
            for (message, expected) in [
                ("fixture startup info", info),
                ("fixture connection warning", warn),
                ("fixture debug detail", debug),
            ] {
                assert_eq!(
                    logged.contains(message),
                    *expected,
                    "file={file_output}, filter={filter:?}, message={message}"
                );
            }
        }
    }
    std::fs::remove_dir_all(directory).unwrap();
}

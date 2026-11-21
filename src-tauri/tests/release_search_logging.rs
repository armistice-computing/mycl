#![cfg(feature = "diagnostics-test")]
#[path = "../src/release_search_logging.rs"]
mod release_search_logging;

#[test]
fn release_sink_filters_rotates_and_refuses_invalid_destination() {
    let root = std::env::temp_dir().join(format!("garden-release-log-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let app = tauri::test::mock_app();
    let (_plugin, _level, logger) = release_search_logging::builder(&root, 1024)
        .unwrap()
        .split(app.handle())
        .unwrap();
    assert!(
        !cfg!(debug_assertions),
        "run this acceptance test with --release"
    );
    assert!(release_search_logging::enabled(
        cfg!(debug_assertions),
        Some("1")
    ));
    for i in 0..100 {
        logger.log(
            &log::Record::builder()
                .target("search_timing")
                .level(log::Level::Info)
                .args(format_args!(
                    "search_id={i} stage=test elapsed_us=123456789"
                ))
                .build(),
        );
        logger.log(
            &log::Record::builder()
                .target("private_document")
                .level(log::Level::Error)
                .args(format_args!("PRIVATE_SENTINEL_MUST_NOT_APPEAR"))
                .build(),
        );
    }
    logger.flush();
    drop(logger);
    let files = std::fs::read_dir(root.join("diagnostics"))
        .unwrap()
        .map(|x| x.unwrap().path())
        .collect::<Vec<_>>();
    assert!(!files.is_empty());
    assert_eq!(files.len(), 1, "single bounded current log: {files:?}");
    let mut seen = false;
    for file in &files {
        let content = std::fs::read_to_string(file).unwrap();
        assert!(!content.contains("PRIVATE_SENTINEL"));
        seen |= content.contains("search_id=99 ");
        assert!(!content.contains("search_id=0 "), "old records must rotate away");
        assert!(content.len() < 2048);
    }
    assert!(seen);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(root.join("diagnostics"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    let bad = root.join("bad");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join("diagnostics"), "not a directory").unwrap();
    assert!(release_search_logging::builder(&bad, 1024).is_err());
    std::fs::remove_dir_all(root).unwrap();
}

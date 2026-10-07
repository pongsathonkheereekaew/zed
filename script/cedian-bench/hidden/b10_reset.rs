    #[test]
    fn bench_b10_old_store_names_the_reset_command() {
        let dir = std::env::temp_dir().join(format!("bench-b10-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".cedian")).unwrap();
        std::fs::write(
            dir.join(".cedian/browser.json"),
            r#"{"port":9222,"ws_url":"ws://x","url":"about:blank","seq":1}"#,
        )
        .unwrap();
        let err = load(&dir).unwrap_err();
        assert!(err.contains("too old") && err.contains("cedian browser open"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

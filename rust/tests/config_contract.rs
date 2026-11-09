//! Behavioral contracts through the public document, store, and path boundary.
//! All fixtures are synthetic; tests never change process-global environment.

use std::fs;
use std::path::{Path, PathBuf};

use isponsorblocktv::config::{
    ConfigDocument, ConfigError, ConfigStore, ConfigWarning, DEFAULT_SPONSORBLOCK_API_URL,
    Environment, LogLevel, Platform, SaveOutcome, resolve_data_dir,
};
use serde_json::{Value, json};
use tempfile::tempdir;

fn document(value: Value) -> ConfigDocument {
    serde_json::from_value(value).expect("object fixture")
}

fn paired() -> ConfigDocument {
    document(json!({"devices": [{"screen_id": "synthetic-screen"}]}))
}

fn environment(platform: Platform) -> Environment {
    Environment {
        platform,
        data_dir: None,
        docker: false,
        home: Some(PathBuf::from("/fixture/home")),
        xdg_data_home: None,
        local_app_data: Some(PathBuf::from("/fixture/local-app-data")),
    }
}

fn assert_field(document: &ConfigDocument, expected: &str) {
    match document
        .runtime_config()
        .expect_err("must reject invalid runtime configuration")
    {
        ConfigError::Field { path, .. } => assert_eq!(path, expected),
        error => panic!("expected a field diagnostic, got {error}"),
    }
}

#[test]
fn empty_document_loads_in_memory_and_only_explicit_save_creates_file() {
    let root = tempdir().unwrap();
    let data_dir = root.path().join("new/nested");
    let (store, document) = ConfigStore::load(data_dir.clone(), false).unwrap();
    assert!(data_dir.is_dir());
    assert_eq!(store.data_dir(), data_dir);
    assert_eq!(store.config_file(), data_dir.join("config.json"));
    assert!(document.0.is_empty());
    assert!(!store.config_file().exists());
    assert_field(&document, "devices");
    assert_eq!(store.save(&document).unwrap(), SaveOutcome::Saved);
    assert_eq!(fs::read(store.config_file()).unwrap(), b"{}\n");
    assert_eq!(store.save(&document).unwrap(), SaveOutcome::Unchanged);
    let (_, reloaded) = ConfigStore::load(data_dir, false).unwrap();
    assert_eq!(reloaded, document);
}

#[test]
fn every_runtime_default_is_compatible_and_conversion_does_not_mutate() {
    let document = paired();
    let before = document.clone();
    let runtime = document.runtime_config().unwrap();
    assert_eq!(runtime.log_level, LogLevel::Info);
    assert_eq!(runtime.devices.len(), 1);
    assert_eq!(runtime.devices[0].screen_id, "synthetic-screen");
    assert_eq!(runtime.devices[0].name, None);
    assert_eq!(runtime.devices[0].display_name(), "Unnamed device");
    assert_eq!(runtime.devices[0].offset_seconds, 0.0);
    assert_eq!(runtime.apikey, "");
    assert_eq!(runtime.skip_categories, ["sponsor"]);
    assert!(runtime.channel_whitelist.is_empty());
    assert!(runtime.skip_count_tracking);
    assert!(!runtime.mute_ads);
    assert!(!runtime.skip_ads);
    assert_eq!(runtime.minimum_skip_length, 1.0);
    assert!(runtime.auto_play);
    assert_eq!(runtime.join_name, "iSponsorBlockTV");
    assert!(!runtime.use_proxy);
    assert_eq!(
        runtime.sponsorblock_api_url.as_str(),
        DEFAULT_SPONSORBLOCK_API_URL
    );
    assert!(runtime.warnings.is_empty());
    assert_eq!(document, before);
}

#[test]
fn log_levels_are_strict_lossless_and_default_to_info() {
    assert_eq!(
        ConfigDocument::default().log_level().unwrap(),
        LogLevel::Info
    );
    for (value, expected) in [
        ("off", LogLevel::Off),
        ("error", LogLevel::Error),
        ("warn", LogLevel::Warn),
        ("info", LogLevel::Info),
        ("debug", LogLevel::Debug),
        ("trace", LogLevel::Trace),
    ] {
        let mut document = paired();
        document.0.insert("log_level".into(), json!(value));
        let before = document.clone();
        assert_eq!(document.log_level().unwrap(), expected);
        assert_eq!(document.runtime_config().unwrap().log_level, expected);
        assert_eq!(document, before);
        let serialized = serde_json::to_value(&document).unwrap();
        assert_eq!(serialized["log_level"], value);
    }
}

#[test]
fn invalid_log_levels_report_only_the_field_and_fixed_reason() {
    for value in [
        json!(null),
        json!(true),
        json!(42),
        json!([]),
        json!({"secret": "sensitive-value"}),
        json!(""),
        json!("INFO"),
        json!("warning"),
        json!(" info"),
        json!("sensitive-value"),
    ] {
        let mut document = paired();
        document.0.insert("log_level".into(), value);
        assert_field(&document, "log_level");
        let error = document.log_level().unwrap_err();
        assert!(matches!(&error, ConfigError::Field { path, .. } if path == "log_level"));
        assert!(!format!("{error:?} {error}").contains("sensitive-value"));
    }
}

#[test]
fn missing_null_empty_and_future_categories_remain_distinct_on_disk() {
    for categories in [
        None,
        Some(Value::Null),
        Some(json!([])),
        Some(json!(["hook", "exclusive_access", "future-category"])),
    ] {
        let root = tempdir().unwrap();
        let (store, _) = ConfigStore::load(root.path().to_owned(), false).unwrap();
        let mut draft = paired();
        if let Some(value) = &categories {
            draft.0.insert("skip_categories".into(), value.clone());
        }
        let before = draft.clone();
        let runtime = draft.runtime_config().unwrap();
        match &categories {
            None | Some(Value::Null) => assert_eq!(runtime.skip_categories, ["sponsor"]),
            Some(Value::Array(values)) => assert_eq!(runtime.skip_categories.len(), values.len()),
            _ => unreachable!(),
        }
        store.save(&draft).unwrap();
        let (_, loaded) = ConfigStore::load(root.path().to_owned(), false).unwrap();
        assert_eq!(loaded.0.get("skip_categories"), categories.as_ref());
        assert_eq!(draft, before);
        assert_eq!(loaded, before);
    }
}

#[test]
fn arbitrary_precision_unknown_values_survive_at_every_object_level() {
    let fixture = r#"{
        "devices": [
            {"screen_id":"screen-one", "name":null, "offset":-250.5,
             "future":{"positive":18446744073709551617,"negative":-18446744073709551617},
             "data_dir":"nested-path-is-not-reserved"},
            {"screen_id":"screen-two", "offset":1500}
        ],
        "apikey":"synthetic-key",
        "channel_whitelist":[{"id":"channel-one", "name":null,
            "future":{"positive":18446744073709551617,"negative":-18446744073709551617},
            "config_file": {"keep":true}}],
        "future":{"positive":18446744073709551617,"negative":-18446744073709551617,
            "nested":[null,true,"Unicode café 🦀",{"x":1.2345678901234567890123456789}]},
        "minimum_skip_length":-2.5
    }"#;
    let root = tempdir().unwrap();
    fs::write(root.path().join("config.json"), fixture).unwrap();
    let (store, mut draft) = ConfigStore::load(root.path().to_owned(), false).unwrap();
    let before = draft.clone();
    let runtime = draft.runtime_config().unwrap();
    assert_eq!(runtime.devices[0].offset_seconds, -0.2505);
    assert_eq!(runtime.devices[1].offset_seconds, 1.5);
    assert_eq!(runtime.devices[0].display_name(), "Unnamed device");
    assert_eq!(runtime.minimum_skip_length, -2.5);
    assert_eq!(runtime.channel_whitelist[0].name, None);
    assert_eq!(draft, before);
    draft.0.insert("auto_play".into(), json!(false));
    assert_eq!(store.save(&draft).unwrap(), SaveOutcome::Saved);
    let bytes = fs::read_to_string(store.config_file()).unwrap();
    assert!(bytes.contains("18446744073709551617"));
    assert!(bytes.contains("-18446744073709551617"));
    assert!(bytes.contains("1.2345678901234567890123456789"));
    assert!(bytes.contains("\n    \"auto_play\": false"));
    assert!(bytes.ends_with('\n'));
    let (_, reloaded) = ConfigStore::load(root.path().to_owned(), false).unwrap();
    assert_eq!(reloaded, draft);
    for key in ["devices", "channel_whitelist", "future"] {
        assert_eq!(reloaded.0[key], before.0[key]);
    }
}

#[test]
fn save_accepts_incomplete_and_invalid_runtime_drafts_without_mutation() {
    let root = tempdir().unwrap();
    let (store, _) = ConfigStore::load(root.path().to_owned(), false).unwrap();
    let draft = document(json!({
        "devices": [{"name": null, "offset": -250.5}],
        "channel_whitelist": [{"id": "unfinished-channel"}],
        "minimum_skip_length": "unfinished input",
        "atvs": null,
        "unknown": [true, null]
    }));
    let before = draft.clone();
    assert!(draft.runtime_config().is_err());
    assert_eq!(store.save(&draft).unwrap(), SaveOutcome::Saved);
    assert_eq!(draft, before);
    let (_, reloaded) = ConfigStore::load(root.path().to_owned(), false).unwrap();
    assert_eq!(reloaded, draft);
}

#[test]
fn reserved_path_keys_only_are_removed_without_mutating_live_draft() {
    let root = tempdir().unwrap();
    fs::write(
        root.path().join("config.json"),
        r#"{
        "config_file":"never-follow-this", "data_dir":"never-follow-this-either",
        "devices":[{"screen_id":"synthetic-screen","config_file":null,"data_dir":42}],
        "other":null
    }"#,
    )
    .unwrap();
    let (store, mut draft) = ConfigStore::load(root.path().to_owned(), false).unwrap();
    assert!(!draft.0.contains_key("config_file"));
    assert!(!draft.0.contains_key("data_dir"));
    assert!(draft.0.contains_key("other"));
    assert_eq!(store.save(&draft).unwrap(), SaveOutcome::Saved);
    let persisted: Value = serde_json::from_slice(&fs::read(store.config_file()).unwrap()).unwrap();
    assert!(persisted.get("config_file").is_none());
    assert!(persisted.get("data_dir").is_none());
    assert_eq!(persisted["devices"][0]["config_file"], Value::Null);
    assert_eq!(persisted["devices"][0]["data_dir"], 42);
    draft
        .0
        .insert("config_file".into(), json!("another-reserved-value"));
    draft.0.insert("data_dir".into(), Value::Null);
    let before = draft.clone();
    assert_eq!(store.save(&draft).unwrap(), SaveOutcome::Unchanged);
    assert_eq!(draft, before);
}

#[test]
fn semantic_baseline_ignores_whitespace_key_order_and_mtime() {
    let root = tempdir().unwrap();
    let path = root.path().join("config.json");
    let original = b"{  \"use_proxy\" : false,\"devices\":[{\"screen_id\":\"screen\"}]}";
    fs::write(&path, original).unwrap();
    let (store, mut draft) = ConfigStore::load(root.path().to_owned(), false).unwrap();
    assert_eq!(store.save(&draft).unwrap(), SaveOutcome::Unchanged);
    assert_eq!(fs::read(&path).unwrap(), original);
    // A same-content rewrite changes filesystem history, not semantic content.
    fs::write(&path, original).unwrap();
    assert_eq!(store.save(&draft).unwrap(), SaveOutcome::Unchanged);
    draft.0.insert("use_proxy".into(), json!(true));
    assert_eq!(store.save(&draft).unwrap(), SaveOutcome::Saved);
    assert_eq!(store.save(&draft).unwrap(), SaveOutcome::Unchanged);
    let (reloaded_store, reloaded) = ConfigStore::load(root.path().to_owned(), false).unwrap();
    assert_eq!(
        reloaded_store.save(&reloaded).unwrap(),
        SaveOutcome::Unchanged
    );
}

#[test]
fn malformed_existing_files_are_never_overwritten() {
    for bytes in [
        b"{\"apikey\":\"secret-sentinel\", broken}".as_slice(),
        b"[]",
        b"null",
        b"\"text\"",
        b"{",
        b"",
    ] {
        let root = tempdir().unwrap();
        let path = root.path().join("config.json");
        fs::write(&path, bytes).unwrap();
        let error = ConfigStore::load(root.path().to_owned(), false).unwrap_err();
        assert!(matches!(&error, ConfigError::File { path: reported, .. } if reported == &path));
        assert!(!format!("{error} {error:?}").contains("secret-sentinel"));
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn failed_atomic_replacement_preserves_files_draft_and_saved_baseline() {
    let root = tempdir().unwrap();
    let (store, mut draft) = ConfigStore::load(root.path().to_owned(), false).unwrap();
    draft.0.insert("apikey".into(), json!("old-synthetic-key"));
    store.save(&draft).unwrap();
    let old_bytes = fs::read(store.config_file()).unwrap();
    let original = draft.clone();
    // A directory at the destination deterministically prevents atomic rename,
    // even for privileged users. Preserve the actual previous file next to it.
    let retained = root.path().join("retained.json");
    fs::rename(store.config_file(), &retained).unwrap();
    fs::create_dir(store.config_file()).unwrap();
    let sentinel = store.config_file().join("untouched.json");
    fs::write(&sentinel, &old_bytes).unwrap();
    draft.0.insert("apikey".into(), json!("new-synthetic-key"));
    let before = draft.clone();
    let error = store.save(&draft).unwrap_err();
    assert!(matches!(
        error,
        ConfigError::File {
            operation: "atomically replace",
            ..
        }
    ));
    assert_eq!(fs::read(&retained).unwrap(), old_bytes);
    assert_eq!(fs::read(&sentinel).unwrap(), old_bytes);
    assert_eq!(draft, before);
    assert_eq!(
        fs::read_dir(root.path()).unwrap().count(),
        2,
        "failed temporary file must be removed"
    );
    fs::remove_file(sentinel).unwrap();
    fs::remove_dir(store.config_file()).unwrap();
    fs::rename(retained, store.config_file()).unwrap();
    assert_eq!(store.save(&original).unwrap(), SaveOutcome::Unchanged);
    assert_eq!(fs::read(store.config_file()).unwrap(), old_bytes);
    assert_eq!(
        store.save(&draft).unwrap(),
        SaveOutcome::Saved,
        "failure must not advance baseline"
    );
    let (_, reloaded) = ConfigStore::load(root.path().to_owned(), false).unwrap();
    assert_eq!(reloaded, draft);
}

#[test]
fn failure_to_create_temporary_file_preserves_previous_contents() {
    let root = tempdir().unwrap();
    let dir = root.path().join("data");
    let moved = root.path().join("retained-data");
    let (store, mut draft) = ConfigStore::load(dir.clone(), false).unwrap();
    store.save(&draft).unwrap();
    let old_bytes = fs::read(store.config_file()).unwrap();
    fs::rename(&dir, &moved).unwrap();
    draft.0.insert("auto_play".into(), json!(false));
    let before = draft.clone();
    assert!(store.save(&draft).is_err());
    assert_eq!(fs::read(moved.join("config.json")).unwrap(), old_bytes);
    assert_eq!(draft, before);
    fs::rename(moved, dir).unwrap();
    assert_eq!(store.save(&draft).unwrap(), SaveOutcome::Saved);
}

#[cfg(unix)]
#[test]
fn atomic_replacement_preserves_existing_unix_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempdir().unwrap();
    let path = root.path().join("config.json");
    fs::write(&path, "{}").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    let (store, draft) = ConfigStore::load(root.path().to_owned(), false).unwrap();
    let mut draft = draft;
    draft.0.insert("auto_play".into(), json!(false));
    store.save(&draft).unwrap();
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o640
    );
}

#[test]
fn docker_requires_existing_directory_but_does_not_require_a_file() {
    let root = tempdir().unwrap();
    let missing = root.path().join("missing");
    let error = ConfigStore::load(missing.clone(), true).unwrap_err();
    assert!(matches!(error, ConfigError::File { path, .. } if path == missing));
    assert!(!missing.exists());
    let (store, draft) = ConfigStore::load(root.path().to_owned(), true).unwrap();
    assert!(draft.0.is_empty());
    assert!(!store.config_file().exists());
    let not_directory = root.path().join("file");
    fs::write(&not_directory, b"untouched").unwrap();
    assert!(ConfigStore::load(not_directory.clone(), false).is_err());
    assert_eq!(fs::read(not_directory).unwrap(), b"untouched");
}

#[test]
fn migration_is_explicit_and_saved_removal_refreshes_baseline() {
    for legacy in [
        Value::Null,
        json!([]),
        json!({"unknown": "preserve"}),
        json!(false),
    ] {
        let root = tempdir().unwrap();
        let (store, _) = ConfigStore::load(root.path().to_owned(), false).unwrap();
        let mut draft = paired();
        draft.0.insert("atvs".into(), legacy.clone());
        draft.0.insert("future".into(), json!({"preserved": true}));
        store.save(&draft).unwrap();
        assert_field(&draft, "atvs");
        assert_eq!(draft.0["atvs"], legacy);
        let disk_before = fs::read(store.config_file()).unwrap();
        let mut migrated = draft.clone();
        migrated.0.remove("atvs");
        assert!(migrated.runtime_config().is_ok());
        assert_eq!(fs::read(store.config_file()).unwrap(), disk_before);
        assert_eq!(store.save(&migrated).unwrap(), SaveOutcome::Saved);
        assert_eq!(store.save(&migrated).unwrap(), SaveOutcome::Unchanged);
        let (_, reloaded) = ConfigStore::load(root.path().to_owned(), false).unwrap();
        assert_eq!(reloaded, migrated);
        assert_eq!(reloaded.0["future"], draft.0["future"]);
    }
}

#[test]
fn invalid_types_and_required_identifiers_have_precise_field_paths() {
    let cases = [
        ("devices", json!(null), "devices"),
        ("devices", json!({}), "devices"),
        ("devices", json!([null]), "devices[0]"),
        ("devices", json!([{}]), "devices[0].screen_id"),
        (
            "devices",
            json!([{"screen_id": ""}]),
            "devices[0].screen_id",
        ),
        (
            "devices",
            json!([{"screen_id": 123}]),
            "devices[0].screen_id",
        ),
        (
            "devices",
            json!([{"screen_id": null}]),
            "devices[0].screen_id",
        ),
        (
            "devices",
            json!([{"screen_id": "screen", "name": false}]),
            "devices[0].name",
        ),
        (
            "devices",
            json!([{"screen_id": "screen", "offset": null}]),
            "devices[0].offset",
        ),
        (
            "devices",
            json!([{"screen_id": "screen", "offset": "250"}]),
            "devices[0].offset",
        ),
        (
            "devices",
            json!([{"screen_id": "screen", "offset": true}]),
            "devices[0].offset",
        ),
        ("apikey", json!(null), "apikey"),
        ("apikey", json!(123), "apikey"),
        ("skip_categories", json!("sponsor"), "skip_categories"),
        ("skip_categories", json!([null]), "skip_categories[0]"),
        ("channel_whitelist", json!(null), "channel_whitelist"),
        (
            "channel_whitelist",
            json!(["channel"]),
            "channel_whitelist[0]",
        ),
        ("channel_whitelist", json!([{}]), "channel_whitelist[0].id"),
        (
            "channel_whitelist",
            json!([{"id": ""}]),
            "channel_whitelist[0].id",
        ),
        (
            "channel_whitelist",
            json!([{"id": 123}]),
            "channel_whitelist[0].id",
        ),
        (
            "channel_whitelist",
            json!([{"channel_id": "not-an-alias"}]),
            "channel_whitelist[0].id",
        ),
        (
            "channel_whitelist",
            json!([{"id": "channel", "name": 123}]),
            "channel_whitelist[0].name",
        ),
        ("minimum_skip_length", json!(null), "minimum_skip_length"),
        ("minimum_skip_length", json!("1"), "minimum_skip_length"),
        ("join_name", json!(null), "join_name"),
        ("join_name", json!([]), "join_name"),
        ("sponsorblock_api_url", json!(null), "sponsorblock_api_url"),
    ];
    for (key, value, path) in cases {
        let mut draft = paired();
        draft.0.insert(key.into(), value);
        let before = draft.clone();
        assert_field(&draft, path);
        assert_eq!(draft, before);
    }
    for key in [
        "skip_count_tracking",
        "mute_ads",
        "skip_ads",
        "auto_play",
        "use_proxy",
    ] {
        for value in [Value::Null, json!(0), json!("true"), json!([])] {
            let mut draft = paired();
            draft.0.insert(key.into(), value);
            assert_field(&draft, key);
        }
    }
}

#[test]
fn timing_numbers_require_finite_nonunderflowing_runtime_conversion() {
    for number in ["1e400", "-1e400", "1e-400", "-1e-400"] {
        for (key, path) in [
            ("offset", "devices[0].offset"),
            ("minimum_skip_length", "minimum_skip_length"),
        ] {
            let mut draft = paired();
            let value: Value = serde_json::from_str(number).unwrap();
            if key == "offset" {
                draft.0.get_mut("devices").unwrap()[0][key] = value;
            } else {
                draft.0.insert(key.into(), value);
            }
            assert_field(&draft, path);
        }
    }
    let mut draft = paired();
    draft.0.get_mut("devices").unwrap()[0]["offset"] =
        serde_json::from_str::<Value>("5e-324").unwrap();
    assert_field(&draft, "devices[0].offset");
    for number in ["-2.5", "0", "-0.0", "0e-400", "1.25"] {
        draft.0.get_mut("devices").unwrap()[0]["offset"] =
            serde_json::from_str::<Value>(number).unwrap();
        draft.0.insert(
            "minimum_skip_length".into(),
            serde_json::from_str::<Value>(number).unwrap(),
        );
        assert!(draft.runtime_config().is_ok());
    }
}

#[test]
fn whitelist_requires_api_key_and_null_names_are_allowed() {
    let mut draft = paired();
    draft.0.insert(
        "channel_whitelist".into(),
        json!([{"id": "channel", "name": null}]),
    );
    assert_field(&draft, "apikey");
    draft.0.insert("apikey".into(), json!(""));
    assert_field(&draft, "apikey");
    draft.0.insert("apikey".into(), json!("synthetic-key"));
    assert_eq!(
        draft.runtime_config().unwrap().channel_whitelist[0].name,
        None
    );
}

#[test]
fn legacy_screen_id_warns_without_rejecting_or_exposing_identity() {
    let screen = "s".repeat(26);
    let draft = document(json!({"devices": [{"screen_id": screen}]}));
    let runtime = draft.runtime_config().unwrap();
    assert_eq!(
        runtime.warnings,
        [ConfigWarning::LegacyScreenId { device_index: 0 }]
    );
    let warning = runtime.warnings[0].to_string();
    assert!(warning.contains("devices[0].screen_id"));
    assert!(!warning.contains(&screen));
}

#[test]
fn independent_ad_flags_and_other_explicit_values_survive() {
    for mute in [false, true] {
        for skip in [false, true] {
            let mut draft = paired();
            draft.0.insert("mute_ads".into(), json!(mute));
            draft.0.insert("skip_ads".into(), json!(skip));
            draft.0.insert("skip_count_tracking".into(), json!(false));
            draft.0.insert("auto_play".into(), json!(false));
            draft.0.insert("use_proxy".into(), json!(true));
            draft
                .0
                .insert("join_name".into(), json!("custom join name"));
            let runtime = draft.runtime_config().unwrap();
            assert_eq!(runtime.mute_ads, mute);
            assert_eq!(runtime.skip_ads, skip);
            assert!(!runtime.skip_count_tracking);
            assert!(!runtime.auto_play);
            assert!(runtime.use_proxy);
            assert_eq!(runtime.join_name, "custom join name");
        }
    }
}

#[test]
fn base_urls_are_validated_without_rewriting_untouched_documents() {
    for input in [
        "",
        "/relative",
        "file:///tmp/api",
        "ftp://example.com/api",
        "https://",
        "https://user:secret@example.com/api",
        "https://user@example.com/api",
        "https://@example.com/api",
        "https://example.com/api?key=secret",
        "https://example.com/api?",
        "https://example.com/api#fragment",
        "https://example.com/api#",
    ] {
        let mut draft = paired();
        draft.0.insert("sponsorblock_api_url".into(), json!(input));
        assert_field(&draft, "sponsorblock_api_url");
    }
    for (input, expected) in [
        ("https://example.com/api", "https://example.com/api/"),
        (
            "http://127.0.0.1:12345/custom/",
            "http://127.0.0.1:12345/custom/",
        ),
        (
            "https://example.com/encoded%20path",
            "https://example.com/encoded%20path/",
        ),
    ] {
        let mut draft = paired();
        draft.0.insert("sponsorblock_api_url".into(), json!(input));
        let before = draft.clone();
        assert_eq!(
            draft
                .runtime_config()
                .unwrap()
                .sponsorblock_api_url
                .as_str(),
            expected
        );
        assert_eq!(draft, before);
    }
}

#[test]
fn debug_and_errors_redact_secrets_and_unknown_document_contents() {
    let draft = document(json!({
        "devices": [{"screen_id":"screen-secret-sentinel","name":"device-secret-sentinel"}],
        "apikey":"api-secret-sentinel",
        "channel_whitelist":[{"id":"channel-secret-sentinel","name":"name-secret-sentinel"}],
        "future":{"private":"unknown-secret-sentinel"}
    }));
    let runtime = draft.runtime_config().unwrap();
    let diagnostics = format!(
        "{draft:?} {runtime:?} {:?} {:?}",
        runtime.devices, runtime.channel_whitelist
    );
    assert!(!diagnostics.contains("secret-sentinel"));
    let mut invalid = draft;
    invalid.0.insert(
        "sponsorblock_api_url".into(),
        json!("https://user:url-secret-sentinel@example.com/"),
    );
    let error = invalid.runtime_config().unwrap_err();
    assert!(!format!("{error} {error:?}").contains("secret-sentinel"));
}

#[test]
fn platform_defaults_use_local_not_roaming_windows_appdata() {
    let cases = [
        (
            Platform::Linux,
            "/fixture/home/.local/share/iSponsorBlockTV",
        ),
        (
            Platform::MacOs,
            "/fixture/home/Library/Application Support/iSponsorBlockTV",
        ),
        (
            Platform::Windows,
            "/fixture/local-app-data/dmunozv04/iSponsorBlockTV",
        ),
    ];
    for (platform, expected) in cases {
        assert_eq!(
            resolve_data_dir(None, &environment(platform)).unwrap(),
            PathBuf::from(expected)
        );
    }
    let mut linux = environment(Platform::Linux);
    linux.xdg_data_home = Some(PathBuf::from("/fixture/xdg"));
    assert_eq!(
        resolve_data_dir(None, &linux).unwrap(),
        PathBuf::from("/fixture/xdg/iSponsorBlockTV")
    );
    linux.home = None;
    assert!(
        resolve_data_dir(None, &linux).is_ok(),
        "XDG path needs no home fallback"
    );
    linux.xdg_data_home = Some(PathBuf::new());
    assert!(resolve_data_dir(None, &linux).is_err());
    for platform in [Platform::MacOs, Platform::Windows] {
        let mut env = environment(platform);
        env.xdg_data_home = Some(PathBuf::from("/ignored-on-this-platform"));
        let expected = resolve_data_dir(None, &environment(platform)).unwrap();
        assert_eq!(resolve_data_dir(None, &env).unwrap(), expected);
    }
}

#[test]
fn path_precedence_preserves_relative_cli_and_environment_paths() {
    for platform in [Platform::Linux, Platform::MacOs, Platform::Windows] {
        let mut env = environment(platform);
        env.home = None;
        env.local_app_data = None;
        env.data_dir = Some(PathBuf::from("relative-env"));
        assert_eq!(
            resolve_data_dir(None, &env).unwrap(),
            PathBuf::from("relative-env")
        );
        assert_eq!(
            resolve_data_dir(Some(Path::new("relative-cli")), &env).unwrap(),
            PathBuf::from("relative-cli")
        );
        env.data_dir = Some(PathBuf::new());
        assert!(resolve_data_dir(None, &env).is_err());
        assert_eq!(
            resolve_data_dir(Some(Path::new("explicit")), &env).unwrap(),
            PathBuf::from("explicit")
        );
    }
}

#[test]
fn missing_or_empty_os_directories_are_actionable_errors() {
    for platform in [Platform::Linux, Platform::MacOs, Platform::Windows] {
        for path in [None, Some(PathBuf::new())] {
            let mut env = environment(platform);
            env.home = path.clone();
            env.local_app_data = path;
            let error = resolve_data_dir(None, &env).unwrap_err();
            assert!(matches!(error, ConfigError::Environment { .. }));
            assert!(error.to_string().contains("--data"));
        }
    }
}

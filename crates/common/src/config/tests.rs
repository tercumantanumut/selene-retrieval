use super::*;
use crate::error::{ErrorCode, SuggestedAction};

#[test]
fn defaults_are_valid_and_match_the_plan() {
    let s = Settings::default();
    assert!(
        s.validate().is_empty(),
        "defaults must validate: {:?}",
        s.validate()
    );
    // P1.2: local search, no key, optional features off.
    assert_eq!(s.search.search_model, SearchModel::Standard);
    assert!(s.provider.provider.is_none());
    assert!(!s.answers.answers_enabled);
    assert!(!s.answers.deeper_answers);
    assert!(!s.integrations.http_server_enabled);
    assert!(!s.integrations.mcp_add_folder_enabled);
    assert!(!s.integrations.mcp_sync_folder_enabled);
    assert!(!s.ranking.rerank_enabled);
    assert!(!s.indexing.follow_symlinks);
    assert!(s.general.start_with_windows);
    assert_eq!(s.indexing.max_file_bytes, 10 * 1024 * 1024);
    assert_eq!(s.indexing.debounce_ms, 500);
    assert_eq!(s.indexing.reconcile_minutes, 60);
    assert_eq!(s.ranking.rrf_k, 60);
    assert_eq!(s.ranking.default_max_results, 50);
    assert!((s.ranking.min_score - 0.1).abs() < f32::EPSILON);
    assert_eq!(s.system.log_level, LogLevel::Info);
}

#[test]
fn empty_file_gives_defaults_without_issues() {
    let (s, issues, reset) = from_toml_str("");
    assert_eq!(s, Settings::default());
    assert!(issues.is_empty());
    assert!(!reset);
}

#[test]
fn bad_values_fall_back_per_field_and_good_values_are_kept() {
    let toml = r#"
        [general]
        language = "not a language!"
        notify_on_problems = false

        [search]
        search_model = "quantum"

        [indexing]
        max_file_bytes = 1
        debounce_ms = "fast"
        chunk_preset = "huge"
        reconcile_minutes = 30

        [ranking]
        rrf_k = 0
        min_score = 7.5
        mmr_lambda = nan
        default_max_results = 20

        [network]
        base_url = "http://evil.example.com"
        proxy = "ftp://x"

        [integrations]
        http_port = 80

        [system]
        data_dir = "relative\\dir"
        log_level = "loud"
    "#;
    let (s, issues, reset) = from_toml_str(toml);
    assert!(!reset);
    let d = Settings::default();
    // reset
    assert_eq!(s.general.language, d.general.language);
    assert_eq!(s.search.search_model, d.search.search_model);
    assert_eq!(s.indexing.max_file_bytes, d.indexing.max_file_bytes);
    assert_eq!(s.indexing.debounce_ms, d.indexing.debounce_ms);
    assert_eq!(s.indexing.chunk_preset, d.indexing.chunk_preset);
    assert_eq!(s.ranking.rrf_k, d.ranking.rrf_k);
    assert_eq!(s.ranking.min_score, d.ranking.min_score);
    assert_eq!(s.ranking.mmr_lambda, None);
    assert_eq!(s.network.base_url, None);
    assert_eq!(s.network.proxy, None);
    assert_eq!(s.integrations.http_port, None);
    assert_eq!(s.system.data_dir, None);
    assert_eq!(s.system.log_level, d.system.log_level);
    // kept
    assert!(!s.general.notify_on_problems);
    assert_eq!(s.indexing.reconcile_minutes, 30);
    assert_eq!(s.ranking.default_max_results, 20);

    let paths: Vec<&str> = issues.iter().map(|i| i.path.as_str()).collect();
    for p in [
        "general.language",
        "search.search_model",
        "indexing.max_file_bytes",
        "indexing.debounce_ms",
        "indexing.chunk_preset",
        "ranking.rrf_k",
        "ranking.min_score",
        "ranking.mmr_lambda",
        "network.base_url",
        "network.proxy",
        "integrations.http_port",
        "system.data_dir",
        "system.log_level",
    ] {
        assert!(paths.contains(&p), "{p} not reported in {paths:?}");
    }
    assert!(!paths.contains(&"indexing.reconcile_minutes"));
    assert!(s.validate().is_empty());
}

#[test]
fn wrong_shapes_never_crash() {
    for toml in [
        "general = 5",
        "[general]\nstart_with_windows = \"yes\"",
        "version = \"one\"",
        "[indexing]\nexclude_globs = \"*.log\"",
        "[indexing]\nchunk_size = 100\nchunk_overlap = 100",
        "[indexing]\nchunk_size = -3",
        "[integrations]\nhttp_port = 70000",
        "[[ranking]]\nrrf_k = 1",
    ] {
        let (s, issues, reset) = from_toml_str(toml);
        assert!(!reset, "{toml}");
        assert!(!issues.is_empty(), "expected an issue for {toml}");
        assert!(s.validate().is_empty(), "{toml}: result must be valid");
    }
}

#[test]
fn unparseable_file_is_reset_entirely() {
    let (s, issues, reset) = from_toml_str("this is = = not toml [[[");
    assert!(reset);
    assert_eq!(s, Settings::default());
    assert_eq!(issues.len(), 1);
}

#[test]
fn unknown_keys_survive_a_rewrite() {
    let toml = r#"
        future_top = "keep me"
        [general]
        future_flag = true
        [plugin_x]
        level = 3
    "#;
    let (s, issues, _) = from_toml_str(toml);
    assert!(issues.is_empty(), "{issues:?}");
    let out = toml::to_string_pretty(&s).unwrap();
    let (again, issues2, _) = from_toml_str(&out);
    assert!(issues2.is_empty(), "{issues2:?}\n{out}");
    assert_eq!(again, s);
    assert_eq!(
        again.extra.get("future_top").and_then(|v| v.as_str()),
        Some("keep me")
    );
    assert_eq!(
        again
            .general
            .extra
            .get("future_flag")
            .and_then(toml::Value::as_bool),
        Some(true)
    );
    assert!(
        again
            .extra
            .get("plugin_x")
            .is_some_and(toml::Value::is_table)
    );
}

/// Amendment 17: `settings.toml` never had a folder mode, but a hand-edited or older file that
/// still says `indexing_mode = "files_only"` (anywhere) loads without an error or a reset notice;
/// the key is kept as an unknown key, like any other.
#[test]
fn removed_indexing_mode_key_loads_without_error() {
    let toml = r#"
        indexing_mode = "files_only"
        [indexing]
        indexing_mode = "files_only"
        [folders]
        indexing_mode = "files_only"
    "#;
    let (s, issues, reset) = from_toml_str(toml);
    assert!(issues.is_empty(), "{issues:?}");
    assert!(!reset);
    let kept = |e: &Extra| {
        e.get("indexing_mode")
            .and_then(|v| v.as_str().map(str::to_owned))
    };
    assert_eq!(kept(&s.extra).as_deref(), Some("files_only"));
    assert_eq!(kept(&s.indexing.extra).as_deref(), Some("files_only"));
    let indexing = Indexing {
        extra: Extra::new(),
        ..s.indexing.clone()
    };
    assert_eq!(indexing, Indexing::default(), "nothing else changes");
}

#[test]
fn search_model_strings() {
    let (s, issues, _) = from_toml_str("[search]\nsearch_model = \"cloud:openai\"");
    assert!(issues.is_empty());
    assert_eq!(s.search.search_model, SearchModel::Cloud("openai".into()));
    let (s, issues, _) = from_toml_str("[search]\nsearch_model = \"cloud:\"");
    assert_eq!(issues.len(), 1);
    assert_eq!(s.search.search_model, SearchModel::Standard);
}

#[test]
fn provider_url_rules() {
    assert!(is_valid_provider_url("https://api.openai.com/v1"));
    assert!(is_valid_provider_url("http://127.0.0.1:11434"));
    assert!(is_valid_provider_url("http://localhost:11434/api"));
    assert!(is_valid_provider_url("http://[::1]:11434"));
    assert!(!is_valid_provider_url("http://192.168.1.5:11434"));
    assert!(!is_valid_provider_url("http://localhost.evil.com"));
    assert!(!is_valid_provider_url("https://user@host"));
    assert!(!is_valid_provider_url("file:///C:/x"));
    assert!(!is_valid_provider_url("https://"));
}

#[test]
fn every_leaf_field_has_a_tier() {
    let tiers = Settings::field_tiers();
    assert!(tiers.len() > 30, "only {} fields found", tiers.len());
    for (path, tier) in &tiers {
        assert!(tier.is_some(), "{path} has no x-tier");
    }
    let basic: Vec<_> = tiers
        .iter()
        .filter(|(_, t)| *t == Some(FieldTier::Basic))
        .map(|(p, _)| p.as_str())
        .collect();
    for p in [
        "general.start_with_windows",
        "search.search_model",
        "provider.provider",
    ] {
        assert!(basic.contains(&p), "{p} should be basic");
    }
    assert!(!basic.contains(&"ranking.rrf_k"));
}

#[test]
fn load_or_create_never_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub").join(SETTINGS_FILE);

    // Missing → created with defaults.
    let r = load_or_create(&path);
    assert!(r.created && r.issues.is_empty() && r.persist_error.is_none());
    assert!(path.exists());
    assert!(r.notice().is_none());
    let r = load_or_create(&path);
    assert!(!r.created && r.issues.is_empty());

    // Bad field → reset, original kept as .bak, notice with one action.
    std::fs::write(
        &path,
        "[ranking]\nrrf_k = 0\n[indexing]\ndebounce_ms = 900\n",
    )
    .unwrap();
    let r = load_or_create(&path);
    assert_eq!(r.settings.ranking.rrf_k, 60);
    assert_eq!(r.settings.indexing.debounce_ms, 900);
    assert!(path.with_file_name("settings.toml.bak").exists());
    let n = r.notice().unwrap();
    assert_eq!(n.code, ErrorCode::SettingsInvalid);
    let m = n.user_message();
    assert_eq!(m.title, "Some settings were reset");
    assert_eq!(
        m.action,
        SuggestedAction::OpenSettings(crate::error::Section::Ranking)
    );
    // Rewritten file is now clean.
    assert!(load_or_create(&path).issues.is_empty());

    // Garbage / non-UTF-8 → backed up as .invalid and replaced.
    std::fs::write(&path, [0xff, 0xfe, 0x00, 0x41]).unwrap();
    let r = load_or_create(&path);
    assert!(r.file_reset);
    assert_eq!(r.settings, Settings::default());
    assert!(path.with_file_name("settings.toml.invalid").exists());
    assert!(load_or_create(&path).issues.is_empty());
}

#[test]
fn utf8_bom_is_accepted() {
    let (s, issues, reset) = from_toml_str("\u{feff}[general]\nlanguage = \"de-DE\"\n");
    assert!(!reset && issues.is_empty());
    assert_eq!(s.general.language, "de-DE");
}

// CO-1: typed tier choices (Amendment 4.10: settings.toml is their only home).

#[test]
fn tier_choices_default_to_auto_and_graphics_card_on() {
    use crate::tier::Tier;
    let s = Settings::default();
    assert_eq!(s.search.tier_override, None, "Automatic");
    assert!(s.search.use_graphics_card, "Amendment 2: GPU on by default");
    // Written out explicitly so the file is self-describing, and read back unchanged.
    let out = toml::to_string_pretty(&s).unwrap();
    assert!(out.contains("tier_override = \"auto\""), "{out}");
    assert!(out.contains("use_graphics_card = true"), "{out}");
    let (back, issues, _) = from_toml_str(&out);
    assert!(issues.is_empty(), "{issues:?}");
    assert_eq!(back, s);
    let mut s2 = Settings::default();
    s2.search.tier_override = Some(Tier::High);
    s2.search.use_graphics_card = false;
    let out = toml::to_string_pretty(&s2).unwrap();
    assert!(out.contains("tier_override = \"best\""), "{out}");
    assert_eq!(from_toml_str(&out).0, s2);
}

#[test]
fn tier_choices_accept_user_words_and_tier_names() {
    use crate::tier::Tier;
    for (v, want) in [
        ("auto", None),
        ("basic", Some(Tier::Minimum)),
        ("better", Some(Tier::Standard)),
        ("best", Some(Tier::High)),
        ("minimum", Some(Tier::Minimum)),
        ("standard", Some(Tier::Standard)),
        ("High", Some(Tier::High)),
        (" Better ", Some(Tier::Standard)),
    ] {
        let (s, issues, _) = from_toml_str(&format!("[search]\ntier_override = \"{v}\"\n"));
        assert!(issues.is_empty(), "{v}: {issues:?}");
        assert_eq!(s.search.tier_override, want, "{v}");
    }
    for (v, want) in [
        ("true", true),
        ("false", false),
        ("\"auto\"", true),
        ("\"on\"", true),
        ("\"off\"", false),
        ("\"OFF\"", false),
    ] {
        let (s, issues, _) = from_toml_str(&format!("[search]\nuse_graphics_card = {v}\n"));
        assert!(issues.is_empty(), "{v}: {issues:?}");
        assert_eq!(s.search.use_graphics_card, want, "{v}");
    }
}

#[test]
fn bad_tier_choices_fall_back_to_defaults_without_failing() {
    let toml = r#"
        [search]
        search_model = "standard"
        tier_override = "ultra"
        use_graphics_card = "maybe"
    "#;
    let (s, issues, reset) = from_toml_str(toml);
    assert!(!reset, "the file still loads");
    assert_eq!(s.search.tier_override, None);
    assert!(s.search.use_graphics_card);
    let paths: Vec<_> = issues.iter().map(|i| i.path.as_str()).collect();
    assert!(paths.contains(&"search.tier_override"), "{paths:?}");
    assert!(paths.contains(&"search.use_graphics_card"), "{paths:?}");
    assert!(issues.iter().all(|i| i.section == Section::Search));
    // Wrong types too (number, table, array), never a startup failure.
    for v in ["3", "{ a = 1 }", "[\"best\"]"] {
        let (s, issues, reset) = from_toml_str(&format!(
            "[search]\ntier_override = {v}\nuse_graphics_card = {v}\n"
        ));
        assert!(!reset);
        assert_eq!(s.search.tier_override, None, "{v}");
        assert!(s.search.use_graphics_card, "{v}");
        assert_eq!(issues.len(), 2, "{v}: {issues:?}");
    }
    // Through the file loader: the notice is "Some settings were reset" for Search.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join(SETTINGS_FILE);
    std::fs::write(&p, toml).unwrap();
    let r = load_or_create(&p);
    assert!(r.persist_error.is_none());
    assert_eq!(r.settings.search.tier_override, None);
    let n = r.notice().expect("notice");
    assert_eq!(n.code, ErrorCode::SettingsInvalid);
    let again = load_or_create(&p);
    assert!(
        again.issues.is_empty(),
        "rewritten with defaults: {:?}",
        again.issues
    );
}

#[test]
fn tier_choices_are_advanced_with_plain_labels() {
    let tiers = Settings::field_tiers();
    for f in ["search.tier_override", "search.use_graphics_card"] {
        assert_eq!(
            tiers.iter().find(|(p, _)| p == f).map(|(_, t)| *t),
            Some(Some(FieldTier::Advanced)),
            "{f}"
        );
    }
    let schema = Settings::json_schema();
    let search = &schema["$defs"]["Search"]["properties"];
    let t = &search["tier_override"];
    assert_eq!(
        t["enum"],
        serde_json::json!(["auto", "basic", "better", "best"])
    );
    assert_eq!(t["x-enum-labels"]["auto"], "Automatic (recommended)");
    assert_eq!(t["x-section"], "search");
    let g = &search["use_graphics_card"];
    assert_eq!(g["type"], "boolean");
    assert_eq!(g["x-enum-labels"]["false"], "Off");
    for f in [t, g] {
        let d = f["description"].as_str().unwrap_or_default();
        assert!(!d.is_empty(), "label missing: {f}");
        // Plain language: no TOML, code or tier jargon in the label.
        for jargon in ["TOML", "`", "Minimum", "EP", "DirectML", "M1", "M2"] {
            assert!(!d.contains(jargon), "{jargon:?} in label {d:?}");
        }
    }
}

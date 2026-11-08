use super::*;
use crate::ids::{ApprovalId, FolderId};

fn full_ctx() -> ErrCtx {
    ErrCtx {
        provider: Some("OpenAI".into()),
        path: Some(PathBuf::from(r"C:\Users\me\Documents\report.pdf")),
        name: Some("Documents".into()),
        other: Some("Projects".into()),
        folder_id: Some(FolderId::new()),
        approval_id: Some(ApprovalId::new()),
        limit: Some("10 MB".into()),
        client: Some(ClientKind::ClaudeDesktop),
        client_name: None,
        host: Some("api.openai.com".into()),
        model: Some("multilingual model".into()),
        need: Some("1.2 GB".into()),
        drive: Some("C:".into()),
        purpose: Some(Purpose::EmbedQuery),
        section: Some(Section::Ranking),
        secs: Some(12),
        detail: Some("internal detail".into()),
    }
}

/// Expected action per the §2.5 table.
fn expected_action(code: ErrorCode) -> &'static str {
    use ErrorCode as C;
    match code {
        C::InvalidInput
        | C::PathDenied
        | C::FolderOverlap
        | C::ApprovalDenied
        | C::Cancelled
        | C::Corrupt
        | C::ManagedByPolicy
        | C::ConnectionNotTrusted => "None",
        C::NotFound
        | C::PermissionDenied
        | C::FileLocked
        | C::Timeout
        | C::ProviderUnavailable
        | C::Offline
        | C::ModelDownloadFailed
        | C::ModelIntegrity
        | C::PortInUse => "Retry",
        C::PathOutsideSandbox | C::FolderUnavailable => "ChooseFolder",
        C::PathRefused => "ChooseSubfolder",
        C::FileTooLarge | C::FileUnreadable | C::ExtractTimeout => "ViewSkippedFiles",
        C::FileOnlineOnly
        | C::ProviderQuota
        | C::ProviderRequest
        | C::SettingsInvalid
        | C::IndexMissing => "OpenSettings",
        C::ApprovalRequired => "ReviewApproval",
        C::Conflict | C::Busy | C::RateLimited => "WaitAndRetry",
        C::ProviderNotConfigured => "AddProvider",
        C::ProviderAuth => "ReenterKey",
        C::PrivacyBlocked => "AllowRemote",
        C::NetworkBlocked | C::Internal | C::ModelIncompatible => "CopyDiagnostics",
        C::DiskFull => "FreeDiskSpace",
        C::ServiceUnavailable => "RestartApp",
        C::UpgradeRequired => "RestartClient",
    }
}

fn action_name(a: &SuggestedAction) -> &'static str {
    match a {
        SuggestedAction::Retry => "Retry",
        SuggestedAction::WaitAndRetry(_) => "WaitAndRetry",
        SuggestedAction::OpenSettings(_) => "OpenSettings",
        SuggestedAction::ReenterKey(_) => "ReenterKey",
        SuggestedAction::AddProvider => "AddProvider",
        SuggestedAction::AllowRemote(..) => "AllowRemote",
        SuggestedAction::ChooseFolder => "ChooseFolder",
        SuggestedAction::ChooseSubfolder(_) => "ChooseSubfolder",
        SuggestedAction::ReviewApproval(_) => "ReviewApproval",
        SuggestedAction::ViewSkippedFiles(_) => "ViewSkippedFiles",
        SuggestedAction::FreeDiskSpace => "FreeDiskSpace",
        SuggestedAction::RestartApp => "RestartApp",
        SuggestedAction::RestartClient(_) => "RestartClient",
        SuggestedAction::ReconnectClient(_) => "ReconnectClient",
        SuggestedAction::CopyDiagnostics => "CopyDiagnostics",
        SuggestedAction::None => "None",
    }
}

fn expected_retry(code: ErrorCode) -> RetryPolicy {
    use ErrorCode as C;
    match code {
        C::FolderUnavailable | C::DiskFull | C::Corrupt | C::IndexMissing | C::PortInUse => {
            RetryPolicy::Auto
        }
        C::PermissionDenied
        | C::FileLocked
        | C::Conflict
        | C::Busy
        | C::Timeout
        | C::RateLimited
        | C::ProviderUnavailable
        | C::Offline
        | C::ModelDownloadFailed
        | C::ModelIntegrity
        | C::ServiceUnavailable => RetryPolicy::Yes,
        C::ApprovalRequired => RetryPolicy::NotApplicable,
        _ => RetryPolicy::No,
    }
}

#[test]
fn table_has_41_unique_codes() {
    assert_eq!(ErrorCode::ALL.len(), 41);
    let names: std::collections::HashSet<_> = ErrorCode::ALL.iter().map(|c| c.as_str()).collect();
    assert_eq!(names.len(), 41);
    assert_eq!(ErrorCode::ALL.first(), Some(&ErrorCode::InvalidInput));
    // Append-only: #37 stays ModelIncompatible, #38 IndexMissing (R34), #39 ManagedByPolicy (M5-E).
    assert_eq!(ErrorCode::ALL[36], ErrorCode::ModelIncompatible);
    assert_eq!(ErrorCode::ALL[37], ErrorCode::IndexMissing);
    assert_eq!(ErrorCode::ALL[38], ErrorCode::ManagedByPolicy);
    // #40 ConnectionNotTrusted (M5-E, S-01): refused pipe squatter.
    assert_eq!(ErrorCode::ALL[39], ErrorCode::ConnectionNotTrusted);
    // #41 PortInUse (M5-E, S-03): HTTP stays off rather than moving ports.
    assert_eq!(ErrorCode::ALL.last(), Some(&ErrorCode::PortInUse));
    // #36 stays Internal: codes are append-only.
    assert_eq!(ErrorCode::ALL[35], ErrorCode::Internal);
}

#[test]
fn every_code_maps_to_complete_plain_message_with_one_action() {
    for &code in ErrorCode::ALL {
        for ctx in [ErrCtx::default(), full_ctx()] {
            let full = ctx.provider.is_some();
            let e = Error::new(code).with_ctx(ctx);
            let m = e.user_message();
            assert!(
                !m.title.is_empty() && !m.body.is_empty(),
                "{code}: empty text"
            );
            for t in [&m.title, &m.body] {
                assert!(
                    !t.contains('{') && !t.contains('}'),
                    "{code}: placeholder left: {t}"
                );
                assert!(
                    !t.contains(code.as_str()) || code == ErrorCode::Busy,
                    "{code}: raw code: {t}"
                );
                assert!(!t.to_lowercase().contains("daemon"), "{code}: jargon: {t}");
                assert!(
                    t.chars().next().is_some_and(|c| !c.is_lowercase()),
                    "{code}: {t}"
                );
                assert!(!t.contains("internal detail"), "{code}: detail leaked: {t}");
                // ApprovalRequired deliberately shows the folder the user is asked to approve.
                if code != ErrorCode::ApprovalRequired {
                    assert!(!t.contains(r"C:\Users"), "{code}: full path leaked: {t}");
                }
            }
            assert!(
                m.body.ends_with('.'),
                "{code}: body should be a sentence: {}",
                m.body
            );
            // Without an approval id there is nothing to review; the renderer falls back to the
            // Connected apps page so the user still gets exactly one action.
            let want = if code == ErrorCode::ApprovalRequired && !full {
                "OpenSettings"
            } else {
                expected_action(code)
            };
            assert_eq!(action_name(&m.action), want, "{code} (full={full})");
            assert_eq!(m.details.code, code);
            assert_eq!(
                e.retryable,
                expected_retry(code).is_retryable(),
                "{code}: retryable"
            );
            assert_eq!(
                code.retry_policy(),
                expected_retry(code),
                "{code}: retry policy"
            );
            assert!(m.agent_text().starts_with(&m.title));
            if m.action != SuggestedAction::None {
                assert!(!m.action.label().is_empty());
                assert!(!m.action.agent_phrase().is_empty());
            }
        }
    }
}

#[test]
fn context_is_used_when_present() {
    let m = Error::new(ErrorCode::ProviderAuth)
        .with_provider("OpenAI")
        .user_message();
    assert_eq!(m.title, "Your OpenAI key was rejected");
    assert_eq!(m.action, SuggestedAction::ReenterKey("OpenAI".into()));
    assert!(m.agent_text().contains("re-enter their OpenAI key"));
    // Title and body read as two sentences (M1 tester P2 finding).
    assert!(
        m.agent_text()
            .starts_with("Your OpenAI key was rejected. The key may be wrong"),
        "{}",
        m.agent_text()
    );
    let o = Error::new(ErrorCode::PathOutsideSandbox).user_message();
    assert!(
        o.agent_text().starts_with(
            "That file is outside your folders. Selene Retrieval can only open files inside \
             folders you've added. "
        ),
        "{}",
        o.agent_text()
    );

    let m = Error::new(ErrorCode::ProviderAuth).user_message();
    assert_eq!(m.title, "Your AI provider key was rejected");

    let m = Error::new(ErrorCode::FileTooLarge)
        .with_ctx(ErrCtx {
            limit: Some("10 MB".into()),
            ..ErrCtx::default()
        })
        .with_path(r"C:\data\big.bin")
        .user_message();
    assert_eq!(
        m.body,
        "\"big.bin\" is bigger than the 10 MB limit and was skipped."
    );

    let e = Error::new(ErrorCode::RateLimited)
        .with_provider("Gemini")
        .with_retry_after(7);
    let m = e.user_message();
    assert!(m.body.contains("7 seconds"));
    assert_eq!(m.action, SuggestedAction::WaitAndRetry(7));
    assert_eq!(e.mcp_structured()["retry_after"], 7);
    assert_eq!(e.mcp_structured()["code"], "RateLimited");
}

#[test]
fn display_and_debug_never_show_full_paths() {
    let e = Error::new(ErrorCode::FileLocked).with_path(r"C:\Users\secret-name\file.txt");
    assert!(!format!("{e}").contains("secret-name"));
    assert!(!format!("{e:?}").contains("secret-name"));
}

#[test]
fn source_chain_goes_to_details() {
    let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
    let e = Error::from(io);
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    assert_eq!(
        e.user_message().details.chain,
        vec!["access denied".to_string()]
    );
    let locked = Error::from(std::io::Error::from_raw_os_error(32));
    assert_eq!(locked.code, ErrorCode::FileLocked);
}

#[test]
fn fallback_wording_is_never_capitalised_mid_sentence() {
    for &code in ErrorCode::ALL {
        let m = Error::new(code).user_message();
        for placeholder in [
            "This file",
            "This folder",
            "Your AI provider",
            "A connected app",
        ] {
            if let Some(pos) = m.body.find(placeholder) {
                assert_eq!(pos, 0, "{code}: {:?}", m.body);
            }
        }
    }
    assert_eq!(
        Error::new(ErrorCode::PermissionDenied).user_message().body,
        "Windows didn't allow access to this file."
    );
}

#[test]
fn model_incompatible_keeps_current_search() {
    let e = Error::new(ErrorCode::ModelIncompatible);
    let m = e.user_message();
    assert_eq!(m.title, "Couldn't switch search model");
    assert!(m.body.contains("current search was kept"));
    assert_eq!(m.action, SuggestedAction::CopyDiagnostics);
    assert!(!e.retryable);
    assert_eq!(
        serde_json::to_value(ErrorCode::ModelIncompatible).unwrap_or_default(),
        "ModelIncompatible"
    );
}

/// R34 (M5-E): a missing moved index has its own text and never tells anyone to "add the folder".
#[test]
fn index_missing_has_its_own_words() {
    let e = Error::new(ErrorCode::IndexMissing).with_name(r"D:\SeleneRetrieval Index");
    let m = e.user_message();
    assert_eq!(m.title, "Search index not found");
    assert!(
        m.body
            .contains("Reconnect the drive, or move the index back"),
        "{}",
        m.body
    );
    assert_eq!(m.action, SuggestedAction::OpenSettings(Section::System));
    let t = m.agent_text();
    assert!(!t.contains("add the folder"), "{t}");
    assert!(t.contains("reconnect the drive"), "{t}");
    assert!(e.retryable);
}

/// M5-E: a policy refusal says who decided and tells an agent not to retry.
#[test]
fn managed_by_policy_is_plain_and_final() {
    let m = Error::new(ErrorCode::ManagedByPolicy)
        .with_name("Projects")
        .user_message();
    assert_eq!(m.title, "Managed by your organisation");
    assert!(m.body.contains("\"Projects\""), "{}", m.body);
    assert_eq!(m.action, SuggestedAction::None);
    assert!(m.agent_text().contains("don't retry"));
    assert!(!Error::new(ErrorCode::ManagedByPolicy).retryable);
}

/// M5-C: CLI sign-in providers name the user's tool, never a key, and point at Providers & Keys.
#[test]
fn cli_signin_providers_name_the_tool() {
    let m = Error::new(ErrorCode::ProviderAuth)
        .with_provider(crate::api::CLAUDE_SIGNIN_LABEL)
        .user_message();
    assert_eq!(m.title, "Claude Code isn't signed in");
    assert!(!m.body.contains("key"), "{}", m.body);
    assert_eq!(m.action, SuggestedAction::OpenSettings(Section::Providers));
    let m = Error::new(ErrorCode::ProviderNotConfigured)
        .with_provider(crate::api::CODEX_SIGNIN_LABEL)
        .user_message();
    assert_eq!(m.title, "Codex isn't installed on this computer");
    assert_eq!(m.action, SuggestedAction::OpenSettings(Section::Providers));
    // Other providers keep their wording.
    let m = Error::new(ErrorCode::ProviderAuth)
        .with_provider("Claude")
        .user_message();
    assert_eq!(m.title, "Your Claude key was rejected");
}

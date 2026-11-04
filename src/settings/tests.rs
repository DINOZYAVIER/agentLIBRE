use super::*;
use crate::error::Expected;
use crate::forge::testutil::TempDir;
use std::fs;

use serde_json::json;

use super::search::SearchCaFile;
use super::server::DEFAULT_STRATA_URL;

/// Writes `text` as a settings file and loads it.
fn user(text: &str) -> Result<UserFile, Error> {
    let dir = TempDir::new();
    let path = dir.0.join(SETTINGS_FILE);
    fs::write(&path, text).unwrap();
    UserFile::load(&path)
}

/// A user file of the attributes `body`.
fn user_of(body: &str) -> Result<UserFile, Error> {
    user(&format!("{{ agl, ... }}: {{ {body} }}"))
}

fn with_url() -> UserFile {
    UserFile {
        server: ServerFile {
            llama_url: Some("http://x".into()),
            ..ServerFile::default()
        },
        ..UserFile::default()
    }
}

/// The model of a llama Function.
fn llama_model() -> ServerFile {
    ServerFile {
        kind: Some("llama".into()),
        model: Some("m.gguf".into()),
        context: Some(8192),
        ..ServerFile::default()
    }
}

fn resolve_with(user: &UserFile, project: &ProjectFile, cli: Option<&str>, model: &ServerFile) -> Result<Settings, Error> {
    Settings::resolve(
        user,
        Path::new("/c/settings.nix"),
        (Path::new("/d/settings.nix"), project),
        cli,
        model,
    )
}

#[test]
fn user_file() {
    let loaded = user_of(
        r#"function = "coder"; language = "uk";
           server = { engine = "vulkan-server"; llama.url = "http://127.0.0.1:18080"; strata.url = "http://127.0.0.1:18088"; };
           models = [ "/mnt/gguf" ];"#,
    )
    .unwrap();
    assert_eq!(
        loaded,
        UserFile {
            function: Some("coder".into()),
            language: Some(Lang::Uk),
            server: ServerFile {
                llama_url: Some("http://127.0.0.1:18080".into()),
                strata_url: Some("http://127.0.0.1:18088".into()),
                engine: Some("vulkan-server".into()),
                ..ServerFile::default()
            },
            models: vec![PathBuf::from("/mnt/gguf")],
            ..UserFile::default()
        }
    );
    // The model keys are the Function's (AGL-039, D21).
    for key in ["model", "context", "kind"] {
        assert!(
            matches!(user_of(&format!("server.llama.{key} = 1;")), Err(Error::UnknownKey { .. })),
            "{key}"
        );
    }
    assert!(matches!(user_of("server.kind = \"llama\";"), Err(Error::UnknownKey { key, .. }) if key == "server.kind"));
    // The weights folders are absolute (D24b).
    assert!(matches!(user_of(r#"models = [ "gguf" ];"#), Err(Error::SearchPathNotAbsolute { .. })));
    // A settings file is a function of `{ agl, ... }` (D8, D12).
    assert!(matches!(user("{ function = \"x\"; }"), Err(Error::NixNotAFunction { .. })));
    assert!(matches!(user("{ agl, ... }: { function = ; }"), Err(Error::NixEval { .. })));
}

#[test]
fn the_whisper_table() {
    let loaded = user_of(r#"whisper = { url = "http://127.0.0.1:18082"; model = "w.bin"; args = [ "-t" "12" ]; };"#)
        .unwrap()
        .whisper;
    assert_eq!(
        loaded,
        WhisperFile {
            url: Some("http://127.0.0.1:18082".into()),
            model: Some("w.bin".into()),
            args: Some(vec!["-t".into(), "12".into()]),
        }
    );
    // The project wins per key, `args` as a whole.
    let project = ProjectFile {
        whisper: WhisperFile {
            model: Some("small.bin".into()),
            args: Some(Vec::new()),
            ..WhisperFile::default()
        },
        ..ProjectFile::default()
    };
    let settings = resolve_with(
        &UserFile { whisper: loaded.clone(), ..with_url() },
        &project,
        None,
        &llama_model(),
    )
    .unwrap();
    assert_eq!(settings.whisper.url, loaded.url);
    assert_eq!(settings.whisper.model.as_deref(), Some("small.bin"));
    assert_eq!(settings.whisper.args, Some(Vec::new()));
    assert!(matches!(
        user_of("whisper.port = 1;"),
        Err(Error::UnknownKey { key, .. }) if key == "whisper.port"
    ));
    // http:// and https:// (AGL-043 F10); another scheme is refused.
    assert!(user_of(r#"whisper.url = "https://x";"#).is_ok());
    assert!(matches!(user_of(r#"whisper.url = "ftp://x";"#), Err(Error::UnsupportedUrl { .. })));
    // The model is a file name or an absolute path (AGL-039, D24a).
    assert!(user_of(r#"whisper.model = "/w/w.bin";"#).is_ok());
    assert!(matches!(user_of(r#"whisper.model = "a/w.bin";"#), Err(Error::InvalidName(_))));
}

fn full_search() -> SearchFile {
    SearchFile {
        url: Some("https://search.example/v1/search".into()),
        client_certificate: Some("/c/client.crt".into()),
        client_private_key: Some("/c/client.pk8".into()),
        private_ca: Some("/c/ca.crt".into()),
        ca: SearchCaFile::default(),
    }
}

#[test]
fn the_step_ca_of_the_search() {
    // AGL-043 F20: `search.ca` names the step-ca of `agl search login`.
    let file = user_of(
        r#"search.ca = { url = "https://ca.example"; root = "/c/root.crt"; provisioner = "kanidm"; };"#,
    )
    .unwrap();
    assert_eq!(
        file.search.ca.complete().unwrap(),
        SearchCa {
            url: "https://ca.example".into(),
            root: "/c/root.crt".into(),
            provisioner: "kanidm".into(),
        }
    );
    // Alone it is no half-set search: no search, no error.
    assert!(file.search.is_empty());
    assert_eq!(file.search.complete(), Ok(None));
    // The keys: an https:// address, an absolute root, a missing key named.
    assert!(matches!(
        user_of(r#"search.ca.url = "http://ca.example";"#),
        Err(Error::UnsupportedSearchUrl { .. })
    ));
    assert!(matches!(
        user_of(r#"search.ca.root = "root.crt";"#),
        Err(Error::SearchPathNotAbsolute { .. })
    ));
    assert!(matches!(
        user_of(r#"search.ca.port = 1;"#),
        Err(Error::UnknownKey { key, .. }) if key == "search.ca.port"
    ));
    let partial = user_of(r#"search.ca.url = "https://ca.example";"#).unwrap();
    assert_eq!(partial.search.ca.complete(), Err("root"));
    // The project's keys over the user's.
    let project = SearchFile {
        ca: SearchCaFile {
            provisioner: Some("other".into()),
            ..SearchCaFile::default()
        },
        ..SearchFile::default()
    };
    let merged = project.over(&file.search);
    assert_eq!(merged.ca.complete().unwrap().provisioner, "other");
    assert_eq!(merged.ca.complete().unwrap().url, "https://ca.example");
}

#[test]
fn the_search_table() {
    let loaded = user_of(
        r#"search = {
             url = "https://search.example/v1/search";
             client_certificate = "/c/client.crt";
             client_private_key = "/c/client.pk8";
             private_ca = "/c/ca.crt";
           };"#,
    )
    .unwrap()
    .search;
    assert_eq!(loaded, full_search());
    assert!(matches!(
        user_of("search.port = 1;"),
        Err(Error::UnknownKey { key, .. }) if key == "search.port"
    ));
    assert!(matches!(
        user_of(r#"search.url = "http://x";"#),
        Err(Error::UnsupportedSearchUrl { url, .. }) if url == "http://x"
    ));
    assert!(matches!(
        user_of(r#"search.private_ca = "ca.crt";"#),
        Err(Error::SearchPathNotAbsolute { key, value, .. })
            if key == "search.private_ca" && value == "ca.crt"
    ));
}

#[test]
fn the_search_is_set_up_only_by_a_complete_table() {
    let resolve = |user_search: SearchFile, project_search: SearchFile| {
        resolve_with(
            &UserFile { search: user_search, ..with_url() },
            &ProjectFile { search: project_search, ..ProjectFile::default() },
            None,
            &llama_model(),
        )
    };
    // None: no search, no error.
    assert_eq!(resolve(SearchFile::default(), SearchFile::default()).unwrap().search, None);
    // All four: the search.
    let search = resolve(full_search(), SearchFile::default()).unwrap().search.unwrap();
    assert_eq!(search.url, "https://search.example/v1/search");
    assert_eq!(search.private_ca, Path::new("/c/ca.crt"));
    // Some, as the user's: the first missing key, in the user's file.
    let partial = SearchFile {
        url: full_search().url,
        client_private_key: full_search().client_private_key,
        ..SearchFile::default()
    };
    assert!(matches!(
        resolve(partial.clone(), SearchFile::default()),
        Err(Error::MissingKey { path, key })
            if key == "search.client_certificate" && path == Path::new("/c/settings.nix")
    ));
    // The project completes the user's table per key...
    let rest = SearchFile {
        client_certificate: full_search().client_certificate,
        private_ca: full_search().private_ca,
        ..SearchFile::default()
    };
    assert!(resolve(partial, rest).unwrap().search.is_some());
    // ...and a project's own partial table names its own file.
    assert!(matches!(
        resolve(SearchFile::default(), SearchFile { url: full_search().url, ..SearchFile::default() }),
        Err(Error::MissingKey { path, key })
            if key == "search.client_certificate" && path == Path::new("/d/settings.nix")
    ));
}

#[test]
fn the_effective_server_is_the_model_with_the_url_of_its_kind() {
    // AGL-039, D21: the kind and the model keys are the Function's, the
    // url of that kind and the engine are the settings'.
    let user = UserFile {
        server: ServerFile {
            llama_url: Some("http://127.0.0.1:18080".into()),
            strata_url: Some("http://127.0.0.1:19000".into()),
            engine: Some("e".into()),
            ..ServerFile::default()
        },
        ..UserFile::default()
    };
    let llama = resolve_with(&user, &ProjectFile::default(), None, &llama_model()).unwrap();
    assert_eq!(llama.server_url, "http://127.0.0.1:18080");
    assert_eq!(
        llama.server,
        ServerFile {
            kind: Some("llama".into()),
            llama_url: Some("http://127.0.0.1:18080".into()),
            engine: Some("e".into()),
            ..llama_model()
        }
    );
    let strata_model = ServerFile {
        kind: Some("strata".into()),
        strata_config: Some("/s.json".into()),
        ..ServerFile::default()
    };
    let strata = resolve_with(&user, &ProjectFile::default(), None, &strata_model).unwrap();
    assert_eq!(strata.server_url, "http://127.0.0.1:19000");
    assert_eq!(strata.server.llama_url, None);
    // The Strata server has a default address (AGL-036, D6); the
    // llama-server's is required.
    let strata = resolve_with(&with_url(), &ProjectFile::default(), None, &strata_model).unwrap();
    assert_eq!(strata.server_url, DEFAULT_STRATA_URL);
    assert!(matches!(
        resolve_with(&UserFile::default(), &ProjectFile::default(), None, &llama_model()),
        Err(Error::MissingServerUrl { .. })
    ));
    // The project's url wins per key.
    let project = ProjectFile {
        server: ServerFile { llama_url: Some("http://p".into()), ..ServerFile::default() },
        ..ProjectFile::default()
    };
    assert_eq!(resolve_with(&user, &project, None, &llama_model()).unwrap().server_url, "http://p");
}

#[test]
fn colors_repl_and_forge_tables() {
    let loaded = user_of(
        r##"colors = { muted = "italic #010203"; input_background = "#000000"; };
            repl = { tool_lines = 3; frame = false; max_record_seconds = 15; };
            forge.auto_rehome = false;"##,
    )
    .unwrap();
    assert_eq!(loaded.colors.len(), 2);
    assert_eq!(loaded.colors[0].0, Role::InputBackground);
    assert_eq!(loaded.colors[1].0, Role::Muted);
    assert_eq!(
        loaded.repl,
        ReplFile {
            tool_lines: Some(3),
            tool_chars: None,
            frame: Some(false),
            max_record_seconds: Some(15),
            start_server: None,
        }
    );
    assert_eq!(loaded.forge, ForgeFile { auto_rehome: Some(false) });
    let settings = resolve_with(
        &UserFile { server: with_url().server, ..user_of(r#"repl = { tool_lines = 3; frame = false; max_record_seconds = 15; };"#).unwrap() },
        &ProjectFile::default(),
        None,
        &llama_model(),
    )
    .unwrap();
    assert_eq!(
        settings.repl,
        ReplSettings {
            tool_lines: 3,
            tool_chars: 500,
            frame: false,
            max_record_seconds: 15,
            start_server: true,
        }
    );
    // The voice limit is 300 s unless set (AGL-016, D5; AGL-039, D20;
    // AGL-043 F18).
    let plain = resolve_with(&with_url(), &ProjectFile::default(), None, &llama_model()).unwrap();
    assert_eq!(plain.repl.max_record_seconds, 300);
}

#[test]
fn invalid_values_are_errors() {
    assert!(matches!(
        user_of(r#"colors.muted = "red";"#),
        Err(Error::InvalidStyle { key, .. }) if key == "colors.muted"
    ));
    assert!(matches!(
        user_of(r#"colors.mutd = "dim";"#),
        Err(Error::UnknownKey { key, .. }) if key == "colors.mutd"
    ));
    for (body, key) in [
        ("repl.tool_lines = 0;", "repl.tool_lines"),
        ("repl.tool_chars = -5;", "repl.tool_chars"),
        ("repl.max_record_seconds = 0;", "repl.max_record_seconds"),
    ] {
        assert!(
            matches!(user_of(body), Err(Error::NotPositive { key: k, .. }) if k == key),
            "{body}"
        );
    }
    assert!(matches!(
        user_of("forge.auto_rehome = \"on\";"),
        Err(Error::WrongType { key, .. }) if key == "forge.auto_rehome"
    ));
    assert!(matches!(
        user_of(r#"functon = "coder";"#),
        Err(Error::UnknownKey { key, .. }) if key == "functon"
    ));
    assert!(matches!(
        user_of("function = 1;"),
        Err(Error::WrongType { key, expected: Expected::String, .. }) if key == "function"
    ));
    assert!(matches!(
        user_of(r#"server = "http://x";"#),
        Err(Error::WrongType { key, expected: Expected::Table, .. }) if key == "server"
    ));
    assert!(matches!(
        user_of(r#"language = "de";"#),
        Err(Error::UnsupportedLanguage { value, .. }) if value == "de"
    ));
    assert!(user_of(r#"server.strata.url = "https://x";"#).is_ok());
    assert!(matches!(
        user_of(r#"server.llama.url = "unix:/run/llama";"#),
        Err(Error::UnsupportedUrl { url, .. }) if url == "unix:/run/llama"
    ));
}

#[test]
fn missing_user_file_is_empty() {
    let dir = TempDir::new();
    let loaded = UserFile::load(&dir.0.join(SETTINGS_FILE)).unwrap();
    assert_eq!(loaded, UserFile::default());
}

#[test]
fn the_project_file() {
    let dir = TempDir::new();
    let path = dir.0.join(SETTINGS_FILE);
    let load = |path: &Path| ProjectFile::from_section(evaluate(path)?.expect("written"), path);
    fs::write(&path, r#"{ agl, ... }: { function = "f"; server.llama.url = "http://p"; whisper.model = "w.bin"; }"#).unwrap();
    let project = load(&path).unwrap();
    assert_eq!(project.function.as_deref(), Some("f"));
    assert_eq!(project.server.llama_url.as_deref(), Some("http://p"));
    assert_eq!(project.whisper.model.as_deref(), Some("w.bin"));
    // The interface and the weights folders are the user's.
    fs::write(&path, "{ agl, ... }: { language = \"uk\"; }").unwrap();
    assert!(matches!(load(&path), Err(Error::UnknownKey { key, .. }) if key == "language"));
}

#[test]
fn set_top_writes_the_file_anew_and_keeps_the_values() {
    // AGL-039, D11, D12.
    let dir = TempDir::new();
    let path = dir.0.join("config/settings.nix");
    set_top(&path, "function", Value::String("a".into())).unwrap();
    assert_eq!(UserFile::load(&path).unwrap().function.as_deref(), Some("a"));
    fs::write(
        &path,
        "# a comment\n{ agl, ... }: let url = \"http://x\"; in { function = \"a\"; server.llama.url = url; }\n",
    )
    .unwrap();
    set_top(&path, "function", Value::String("b".into())).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("{ agl, ... }: {\n"), "{text}");
    assert!(!text.contains("# a comment") && !text.contains("let"));
    let loaded = UserFile::load(&path).unwrap();
    assert_eq!(loaded.function.as_deref(), Some("b"));
    assert_eq!(loaded.server.llama_url.as_deref(), Some("http://x"));
}

#[test]
fn function_precedence() {
    let user = UserFile { function: Some("user".into()), ..with_url() };
    let resolve = |user: &UserFile, project: Option<&str>, cli| {
        resolve_with(
            user,
            &ProjectFile { function: project.map(str::to_owned), ..ProjectFile::default() },
            cli,
            &llama_model(),
        )
        .unwrap()
        .function
    };
    assert_eq!(resolve(&user, Some("project"), Some("cli")), "cli");
    assert_eq!(resolve(&user, Some("project"), None), "project");
    assert_eq!(resolve(&user, None, None), "user");
    // The default (AGL-039, D32).
    assert_eq!(resolve(&with_url(), None, None), "agentLIBRE-gpu-high");
}

#[test]
fn the_record_round_trips() {
    let server = ServerFile {
        kind: Some("llama".into()),
        llama_url: Some("http://x".into()),
        model: Some("/m/m.gguf".into()),
        context: Some(131072),
        args: Some(vec!["-ngl".into(), "999".into()]),
        mmproj_offload: Some(false),
        ..ServerFile::default()
    };
    assert_eq!(ServerFile::from_json(&server.to_json()), Some(server));
    assert_eq!(ServerFile::from_json(&json!({ "kind": 1 })), None);
}

#[test]
fn the_session_starts_the_server_unless_told_not_to() {
    // AGL-046.
    assert!(ReplSettings::default().start_server);
}

#[test]
fn about_is_read_and_its_source_must_be_absolute() {
    // AGL-048.
    let about = user_of(r#"about = { enabled = false; source = "/r/agentLIBRE"; };"#)
        .unwrap()
        .about;
    assert_eq!(about.enabled, Some(false));
    assert_eq!(about.source.as_deref(), Some("/r/agentLIBRE"));
    assert!(user_of(r#"about.source = "relative";"#).is_err());
    assert!(user_of("about.nope = true;").is_err());
}

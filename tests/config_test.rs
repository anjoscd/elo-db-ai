use std::path::Path;

use elo_ctx::config::{Config, ConfigError};

fn parse(text: &str) -> Result<Config, ConfigError> {
    Config::parse(text, Path::new("elo.toml"))
}

#[test]
fn defaults_apply_to_an_empty_file() {
    let config = parse("").expect("empty configuration is valid");
    assert_eq!(config.workspace.budget_bytes, 9000);
    assert!((config.workspace.min_score - 0.35).abs() < 1e-9);
    assert!((config.quotas.project - 0.50).abs() < 1e-9);
    assert!((config.quotas.link - 0.25).abs() < 1e-9);
    assert!((config.quotas.global - 0.25).abs() < 1e-9);
    assert!(config.projects.is_empty());
    assert!(config.links.is_empty());
}

#[test]
fn quotas_must_add_up_to_one() {
    let error =
        parse("[quotas]\nproject = 0.5\nlink = 0.2\nglobal = 0.2\n").expect_err("sum is 0.9");
    assert!(matches!(error, ConfigError::QuotaSum(_)));
}

#[test]
fn nested_roots_are_allowed() {
    let text = r#"
[[project]]
name = "api"
root = "services/api"

[[project]]
name = "api-inner"
root = "services/api/internal"
"#;
    assert!(parse(text).is_ok());
}

#[test]
fn a_link_needs_two_known_projects() {
    let text = r#"
[[project]]
name = "api"
root = "services/api"

[[link]]
name = "auth"
projects = ["api"]
"#;
    let error = parse(text).expect_err("only one project is known");
    assert!(matches!(error, ConfigError::LinkTooSmall { found: 1, .. }));
}

#[test]
fn duplicate_project_names_are_rejected() {
    let text = r#"
[[project]]
name = "api"
root = "services/api"

[[project]]
name = "api"
root = "services/web"
"#;
    let error = parse(text).expect_err("duplicate name");
    assert!(matches!(error, ConfigError::DuplicateProject(_)));
}

#[test]
fn equivalent_roots_are_allowed() {
    let text = r#"
[[project]]
name = "api"
root = "services/api"
[[project]]
name = "web"
root = "services/web/../api"
"#;
    assert!(parse(text).is_ok());
}

#[test]
fn invalid_roots_are_rejected() {
    for root in ["/abs", "../fora", "services/../../fora", ""] {
        let text = format!("[[project]]\nname = \"api\"\nroot = {root:?}\n");
        assert!(parse(&text).is_err(), "accepted {root}");
    }
}

#[test]
fn lexical_roots_need_not_exist() {
    assert!(parse("[[project]]\nname = \"api\"\nroot = \"./services/web/../api\"").is_ok());
}

#[test]
fn links_reject_repeated_and_unknown_projects() {
    for (projects, repeated) in [
        ("[\"api\", \"api\"]", true),
        ("[\"api\", \"web\", \"unknown\"]", false),
    ] {
        let text = format!(
            r#"
[[project]]
name = "api"
root = "services/api"
[[project]]
name = "web"
root = "services/web"
[[link]]
name = "auth"
projects = {projects}
"#
        );
        let error = parse(&text).expect_err("invalid link");
        if repeated {
            assert!(matches!(error, ConfigError::DuplicateLinkProject { .. }));
        } else {
            assert!(matches!(error, ConfigError::UnknownLinkProject { .. }));
        }
    }
}

#[test]
fn full_min_score_defaults_overrides_and_validation() {
    assert_eq!(parse("").unwrap().workspace.full_min_score, 0.6);
    for value in ["0", "0.75", "1"] {
        let config = parse(&format!("[workspace]\nfull_min_score={value}")).unwrap();
        assert_eq!(
            config.workspace.full_min_score,
            value.parse::<f64>().unwrap()
        );
    }
    for value in ["-0.1", "1.1", "nan", "inf"] {
        assert!(matches!(
            parse(&format!("[workspace]\nfull_min_score={value}")),
            Err(ConfigError::FullMinScoreRange(_))
        ));
    }
}

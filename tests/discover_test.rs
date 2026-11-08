use std::fs;

use elo_ctx::config::Config;
use elo_ctx::discover::{discover, to_toml};
use tempfile::TempDir;

fn touch(dir: &TempDir, rel: &str) {
    let p = dir.path().join(rel);
    fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
    fs::write(p, "{}").expect("write");
}

#[test]
fn monorepo_projects_come_from_manifests() {
    let d = TempDir::new().expect("tmp");
    touch(&d, "package.json");
    touch(&d, "apps/web/package.json");
    touch(&d, "services/pay/Cargo.toml");
    touch(&d, "services/pay/node_modules/x/package.json");
    touch(&d, ".hidden/package.json");
    let found = discover(d.path());
    let roots: Vec<_> = found.projects.iter().map(|p| p.root.as_str()).collect();
    assert_eq!(roots, ["apps/web/", "services/pay/"]);
    assert!(!found.single_root);
}

#[test]
fn outermost_manifest_wins_and_nested_are_reported() {
    let d = TempDir::new().expect("tmp");
    touch(&d, "app/package.json");
    touch(&d, "app/sub/package.json");
    let found = discover(d.path());
    assert_eq!(found.projects.len(), 1);
    assert_eq!(found.nested, ["app/sub/"]);
}

#[test]
fn single_project_repo_is_flagged() {
    let d = TempDir::new().expect("tmp");
    touch(&d, "Cargo.toml");
    let found = discover(d.path());
    assert!(found.projects.is_empty());
    assert!(found.single_root);
}

#[test]
fn repeated_folder_names_get_unique_names_and_valid_toml() {
    let d = TempDir::new().expect("tmp");
    touch(&d, "a/api/package.json");
    touch(&d, "b/api/package.json");
    let found = discover(d.path());
    let names: Vec<_> = found.projects.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["a-api", "b-api"]);
    let text = format!(
        "[workspace]\nbudget_bytes = 9000\n{}",
        to_toml(&found.projects)
    );
    let cfg = Config::parse(&text, d.path()).expect("discovered config parses");
    cfg.validate().expect("discovered config is valid");
}

#[test]
fn git_worktrees_are_copies_not_projects() {
    let d = TempDir::new().expect("tmp");
    touch(&d, "app/package.json");
    touch(&d, "wt-feature/package.json");
    let main = d.path().join("app");
    fs::write(
        d.path().join("wt-feature/.git"),
        format!("gitdir: {}/.git/worktrees/wt-feature\n", main.display()),
    )
    .expect("write .git file");
    let found = discover(d.path());
    let roots: Vec<_> = found.projects.iter().map(|p| p.root.as_str()).collect();
    assert_eq!(roots, ["app/"]);
    assert_eq!(found.worktrees.len(), 1);
    assert_eq!(found.worktrees[0].0, "wt-feature/");
    assert_eq!(
        found.worktrees[0].1,
        fs::canonicalize(&main).expect("canon")
    );
}

#[test]
fn submodules_are_not_worktrees() {
    let d = TempDir::new().expect("tmp");
    touch(&d, "fw/package.json");
    touch(&d, "fw/lib/package.json");
    fs::write(
        d.path().join("fw/lib/.git"),
        "gitdir: ../.git/modules/lib\n",
    )
    .expect("write");
    let found = discover(d.path());
    assert!(found.worktrees.is_empty());
    assert_eq!(found.projects.len(), 1);
}

#[test]
fn relative_worktree_gitdir_resolves_to_the_main_repo() {
    let d = TempDir::new().expect("tmp");
    touch(&d, "main/package.json");
    fs::create_dir_all(d.path().join("main/.git/worktrees/wt")).expect("mkdir");
    touch(&d, "wt/package.json");
    fs::write(
        d.path().join("wt/.git"),
        "gitdir: ../main/.git/worktrees/wt\n",
    )
    .expect("write");
    let found = discover(d.path());
    assert_eq!(found.worktrees.len(), 1);
    let expected = fs::canonicalize(d.path().join("main")).expect("canon");
    assert_eq!(found.worktrees[0].1, expected);
}

#[test]
fn generated_names_never_collide_and_escape_safely() {
    let d = TempDir::new().expect("tmp");
    touch(&d, "services/api/package.json");
    touch(&d, "apps/api/package.json");
    touch(&d, "services-api/package.json");
    touch(&d, "we\"ird dir/package.json");
    let found = discover(d.path());
    let mut names: Vec<_> = found.projects.iter().map(|p| p.name.clone()).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), found.projects.len(), "unique names: {names:?}");
    let text = format!(
        "[workspace]\nbudget_bytes = 9000\n{}",
        to_toml(&found.projects)
    );
    let cfg = Config::parse(&text, d.path()).expect("escaped config parses");
    cfg.validate().expect("valid");
}

#[test]
fn a_directory_named_csproj_is_not_a_manifest() {
    let d = TempDir::new().expect("tmp");
    fs::create_dir_all(d.path().join("x/teste.csproj")).expect("mkdir");
    touch(&d, "y/App.CSPROJ");
    let found = discover(d.path());
    let roots: Vec<_> = found.projects.iter().map(|p| p.root.as_str()).collect();
    assert_eq!(roots, ["y/"]);
}

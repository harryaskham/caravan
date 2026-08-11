use serde_json::Value;

const CARGO_LOCK: &str = include_str!("../Cargo.lock");
const FLAKE_LOCK: &str = include_str!("../flake.lock");

fn cargo_git_revision(package: &str) -> &str {
    let marker = format!("name = \"{package}\"");
    let block = CARGO_LOCK
        .split("[[package]]")
        .find(|block| block.contains(&marker))
        .unwrap_or_else(|| panic!("Cargo.lock package `{package}` exists"));
    let source = block
        .lines()
        .find_map(|line| line.trim().strip_prefix("source = \"git+"))
        .and_then(|line| line.strip_suffix('"'))
        .unwrap_or_else(|| panic!("Cargo.lock package `{package}` uses a Git source"));
    source
        .rsplit_once('#')
        .map(|(_, revision)| revision)
        .filter(|revision| revision.len() == 40)
        .unwrap_or_else(|| panic!("Cargo.lock package `{package}` has an exact Git revision"))
}

#[test]
fn nix_updatable_cli_input_matches_cargo_git_revision() {
    let lock: Value = serde_json::from_str(FLAKE_LOCK).expect("flake.lock is JSON");
    let flake_revision = lock["nodes"]["updatable-cli"]["locked"]["rev"]
        .as_str()
        .expect("flake.lock updatable-cli revision");
    let cargo_revision = cargo_git_revision("updatable-cli");

    assert_eq!(
        flake_revision, cargo_revision,
        "Nix patches Cargo to its flake input, so both dependency revisions must be identical"
    );
}

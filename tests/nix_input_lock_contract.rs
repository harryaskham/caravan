use serde_json::Value;

const CARGO_MANIFEST: &str = include_str!("../Cargo.toml");
const CARGO_LOCK: &str = include_str!("../Cargo.lock");
const FLAKE_LOCK: &str = include_str!("../flake.lock");

fn dependency_declaration<'a>(manifest: &'a str, package: &str) -> Option<&'a str> {
    manifest.lines().map(str::trim).find(|line| {
        line.strip_prefix(package)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    })
}

fn cargo_git_revision<'a>(
    lock: &'a str,
    manifest: &str,
    package: &str,
) -> Result<Option<&'a str>, String> {
    let marker = format!("name = \"{package}\"");
    let block = lock
        .split("[[package]]")
        .find(|block| block.contains(&marker))
        .ok_or_else(|| format!("Cargo.lock package `{package}` exists"))?;
    if let Some(source) = block
        .lines()
        .find_map(|line| line.trim().strip_prefix("source = \"git+"))
        .and_then(|line| line.strip_suffix('"'))
    {
        let revision = source
            .rsplit_once('#')
            .map(|(_, revision)| revision)
            .filter(|revision| {
                revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            .ok_or_else(|| format!("Cargo.lock package `{package}` has an exact Git revision"))?;
        return Ok(Some(revision));
    }

    let declaration = dependency_declaration(manifest, package)
        .ok_or_else(|| format!("Cargo.toml dependency `{package}` exists"))?;
    if declaration.contains("path =") && !declaration.contains("git =") {
        // flake.nix deliberately rewrites the three public Git dependencies to
        // exact Nix store paths and removes their Git source rows from the
        // sandbox Cargo.lock. Direct CI still takes the Some(revision) branch;
        // only that explicit path rewrite may use flake.lock as authority.
        return Ok(None);
    }
    Err(format!(
        "Cargo.lock package `{package}` uses a Git source unless Cargo.toml contains the explicit Nix path rewrite"
    ))
}

#[test]
fn nix_updatable_cli_input_matches_cargo_git_revision() {
    let lock: Value = serde_json::from_str(FLAKE_LOCK).expect("flake.lock is JSON");
    let flake_revision = lock["nodes"]["updatable-cli"]["locked"]["rev"]
        .as_str()
        .expect("flake.lock updatable-cli revision");
    if let Some(cargo_revision) = cargo_git_revision(CARGO_LOCK, CARGO_MANIFEST, "updatable-cli")
        .expect("updatable-cli dependency provenance is explicit")
    {
        assert_eq!(
            flake_revision, cargo_revision,
            "Nix patches Cargo to its flake input, so both dependency revisions must be identical"
        );
    } else {
        assert_eq!(
            flake_revision.len(),
            40,
            "path-rewritten Nix source still pins one exact flake revision"
        );
        assert!(
            flake_revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "path-rewritten Nix source still pins one hexadecimal flake revision"
        );
    }
}

#[test]
fn provenance_parser_distinguishes_direct_git_and_nix_path_rewrite() {
    let revision = "5167a197f7c950b19e5c812931f0949e0cdedfd4";
    let direct_lock = format!(
        "[[package]]\nname = \"updatable-cli\"\nsource = \"git+https://example.invalid/updatable-cli?branch=main#{revision}\"\n"
    );
    let direct_manifest =
        "updatable-cli = { git = \"https://example.invalid/updatable-cli\", branch = \"main\" }";
    assert_eq!(
        cargo_git_revision(&direct_lock, direct_manifest, "updatable-cli").unwrap(),
        Some(revision)
    );

    let rewritten_lock = "[[package]]\nname = \"updatable-cli\"\n";
    let rewritten_manifest = "updatable-cli = { path = \"/nix/store/exact-updatable-cli\" }";
    assert_eq!(
        cargo_git_revision(rewritten_lock, rewritten_manifest, "updatable-cli").unwrap(),
        None
    );

    let malformed_manifest = "updatable-cli = { branch = \"main\" }";
    assert!(cargo_git_revision(rewritten_lock, malformed_manifest, "updatable-cli").is_err());
    assert!(cargo_git_revision("", rewritten_manifest, "updatable-cli").is_err());
}

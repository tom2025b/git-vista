/// #765: a reproducible browser entry point for mutation checking. Native
/// tests cannot execute the frame capture, menu, confirmation, or API dispatch.
#[test]
#[ignore = "builds wasm and runs the browser tag-selector contract"]
fn captured_tag_selector_browser_contract_for_mutation_check() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let run = |args: &[&str]| {
        std::process::Command::new("buildlock")
            .args(args)
            .current_dir(&root)
            .env_remove("BUILDLOCK_FILE")
            .env_remove("NO_COLOR")
            .env("CARGO_TARGET_DIR", root.join("target"))
            .status()
            .unwrap_or_else(|e| panic!("could not run buildlock {args:?}: {e}"))
    };
    assert!(run(&["trunk", "build", "--config", "crates/git-vista/Trunk.toml"]).success());
    assert!(run(&["./dev", "browser", "tag-selector.spec.mjs"]).success());
}

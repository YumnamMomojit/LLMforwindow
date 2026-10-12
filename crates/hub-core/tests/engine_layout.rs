//! Both upstream layouts must be accepted: a source checkout keeps the server in `c/`, while
//! the official release archive packs it flat at the root next to the engines (release.yml).
use hub_core::engine::server_script;

#[test]
fn finds_the_server_in_a_checkout_and_in_a_release_archive() {
    let d = tempfile::tempdir().unwrap();
    assert_eq!(server_script(d.path()), None);

    let release = d.path().join("colibri-v2.0.0-windows-x86_64");
    std::fs::create_dir_all(&release).unwrap();
    std::fs::write(release.join("openai_server.py"), "").unwrap();
    assert_eq!(
        server_script(&release),
        Some(release.join("openai_server.py"))
    );

    let checkout = d.path().join("colibri");
    std::fs::create_dir_all(checkout.join("c")).unwrap();
    std::fs::write(checkout.join("c").join("openai_server.py"), "").unwrap();
    assert_eq!(
        server_script(&checkout),
        Some(checkout.join("c").join("openai_server.py"))
    );
}

/// validate_model.py must import upstream's family_registry from either layout.
#[test]
fn validator_imports_family_registry_from_either_layout() {
    if std::process::Command::new("python3")
        .arg("-c")
        .arg("pass")
        .status()
        .is_err()
    {
        return;
    }
    let helper =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/validate_model.py");
    let stub = "from types import SimpleNamespace\n\
def resolve_model(d):\n    return SimpleNamespace(model_type='t', descriptor=SimpleNamespace(\n\
        id='stub', display_name='Stub', modality='text', tokenizer_file='tokenizer.json'))\n";
    let d = tempfile::tempdir().unwrap();
    for code in [d.path().join("flat"), d.path().join("checkout").join("c")] {
        std::fs::create_dir_all(&code).unwrap();
        std::fs::write(code.join("family_registry.py"), stub).unwrap();
        let root = if code.ends_with("c") {
            code.parent().unwrap().to_path_buf()
        } else {
            code.clone()
        };
        let out = std::process::Command::new("python3")
            .arg("-I")
            .arg(&helper)
            .arg(&root)
            .arg(d.path())
            .output()
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["ok"], true, "{}: {v}", root.display());
        assert_eq!(v["family"], "stub");
    }
}

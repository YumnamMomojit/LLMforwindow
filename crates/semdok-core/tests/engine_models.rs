//! Model id rules and modality gating in `models::validate`, using scripts/validate_model.py
//! against a stub upstream `family_registry` (the descriptor fields the helper reads).
use semdok_core::models;
use std::path::{Path, PathBuf};

#[test]
fn model_ids_must_not_look_like_options_or_hidden_files() {
    for ok in ["llama", "qwen3.6-35b", "m_1.2", "A-b"] {
        assert!(models::valid_model_id(ok), "{ok}");
    }
    for bad in [
        "",
        "-llama",
        "--",
        "-1",
        ".hidden",
        "a b",
        "a/b",
        &"x".repeat(65),
    ] {
        assert!(!models::valid_model_id(bad), "{bad:?}");
    }
    let c = rusqlite::Connection::open_in_memory().unwrap();
    let info = models::ModelInfo {
        family: "qwen36".into(),
        display_name: "Qwen".into(),
        model_type: "qwen3_5_moe".into(),
        modality: "text".into(),
        weights_bytes: 0,
        has_tokenizer: true,
        issues: vec![],
    };
    let err = models::register(&c, "-llama", Path::new("/m"), &info).unwrap_err();
    assert!(err.to_string().contains("not starting with"), "{err}");
}

fn python3() -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| {
            d.join(if cfg!(windows) {
                "python.exe"
            } else {
                "python3"
            })
        })
        .find(|p| p.is_file())
}

fn helper() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/validate_model.py")
}

/// A Colibri checkout whose registry resolves every model to one family.
fn stub_checkout(root: &Path, id: &str, display: &str, modality: Option<&str>) -> PathBuf {
    let dir = root.join(format!("colibri-{id}"));
    std::fs::create_dir_all(dir.join("c")).unwrap();
    let modality = modality.map_or(String::new(), |m| format!("    modality = {m:?}\n"));
    std::fs::write(
        dir.join("c/family_registry.py"),
        format!(
            "class D:\n    id = {id:?}\n    display_name = {display:?}\n    tokenizer_file = 'tokenizer.json'\n{modality}\
             class R:\n    descriptor = D\n    model_type = 'stub'\n\
             def resolve_model(p):\n    return R\n"
        ),
    )
    .unwrap();
    dir
}

fn model_dir(root: &Path) -> PathBuf {
    let m = root.join("model");
    std::fs::create_dir_all(&m).unwrap();
    std::fs::write(m.join("config.json"), "{}").unwrap();
    std::fs::write(m.join("tokenizer.json"), "{}").unwrap();
    std::fs::write(m.join("model.safetensors"), [0u8; 8]).unwrap();
    m
}

#[tokio::test]
async fn validate_rejects_decision_and_image_families() {
    let Some(py) = python3() else {
        eprintln!("skipped: python3 not on PATH");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let m = model_dir(tmp.path());
    for (id, display, modality) in [
        ("gliner_decide", "GLiNER2.5-Decide", "decision"),
        ("laya", "Laya", "decision"),
        ("qwen_image", "Qwen-Image", "image"),
    ] {
        let c = stub_checkout(tmp.path(), id, display, Some(modality));
        let issues = models::validate(&m, &py, &c, &helper()).await.unwrap_err();
        assert!(
            issues
                .iter()
                .any(|i| i.contains("not a chat model") && i.contains(modality)),
            "{id}: {issues:?}"
        );
    }
    let c = stub_checkout(tmp.path(), "qwen36", "Qwen3.6", Some("text"));
    let info = models::validate(&m, &py, &c, &helper()).await.unwrap();
    assert_eq!(
        (info.family.as_str(), info.modality.as_str()),
        ("qwen36", "text")
    );
    // A registry from before `modality` existed only had chat families.
    let c = stub_checkout(tmp.path(), "olmoe", "OLMoE", None);
    let info = models::validate(&m, &py, &c, &helper()).await.unwrap();
    assert_eq!(info.modality, "text");
}

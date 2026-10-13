#!/usr/bin/env python3
"""Ask the pinned upstream Colibri which engine family (if any) serves a model directory.

usage: validate_model.py <colibri_checkout> <model_dir>
Prints one JSON object on stdout. Never loads weights; reads config.json only.
Run as: python -I validate_model.py ...
"""
import json
import sys
from pathlib import Path


def main():
    if len(sys.argv) != 3:
        print(json.dumps({"ok": False, "error": "usage: validate_model.py <colibri_checkout> <model_dir>"}))
        return 2
    # Source checkout keeps the Python in c/; a release archive packs it flat at its root.
    root = Path(sys.argv[1])
    code = root / "c" if (root / "c" / "family_registry.py").is_file() else root
    sys.path.insert(0, str(code))
    try:
        import family_registry as fr
        r = fr.resolve_model(sys.argv[2])
        d = r.descriptor
        # FamilyDescriptor.modality: "text" for the chat engines, "image" (Qwen-Image) or
        # "decision" (Laya, GLiNER2.5-Decide). Semdok Edge accepts "text" only. Registries from
        # before the field existed had chat families only, hence the default.
        modality = getattr(d, "modality", "text")
        print(json.dumps({
            "ok": True, "family": d.id, "display_name": d.display_name,
            "model_type": r.model_type,
            "modality": modality if isinstance(modality, str) else None,
            "tokenizer_file": d.tokenizer_file if isinstance(d.tokenizer_file, str) else None,
        }))
        return 0
    except Exception as e:  # unsupported / unreadable: report, never crash
        print(json.dumps({"ok": False, "error": f"{type(e).__name__}: {e}"[:600]}))
        return 1


if __name__ == "__main__":
    sys.exit(main())

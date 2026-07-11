#!/usr/bin/env python3
"""Validate skills/ against its registry. Stdlib only — runs identically in CI
and locally (`python3 scripts/validate_skills.py`).

Checks, in order:
  1. skills/registry.json conforms to skills/registry.schema.json (the schema
     file is authoritative; this script interprets the JSON Schema subset it
     uses: type / required / properties / additionalProperties / enum / items /
     pattern / minItems / minLength).
  2. Registry <-> directory sync: every registry id has skills/<id>/SKILL.md,
     every skills/*/SKILL.md has a registry entry, ids are unique.
  3. SKILL.md frontmatter: opens with `---`, single-line `name:` equal to the
     directory id, single-line `description:` that is non-empty, <= 1024 chars,
     and starts with "Use when" (discovery convention).
  4. Every relative markdown link in skills/**/*.md resolves from its file,
     and the same holds for the repo-root README.md and TODO.md.
  5. Every fenced ```json block under skills/ parses as JSON — the contract
     docs teach consumers by example, so a malformed example is a doc bug.
     Deliberate fragments that should not parse standalone must use a
     different fence tag (```jsonc or plain ```).

Exit 0 with a one-line summary, or exit 1 listing every failure.
"""

import argparse
import json
import re
import sys
from pathlib import Path


def check_schema(value, schema, path, errors):
    """Interpret the subset of JSON Schema the registry schema uses."""
    typ = schema.get("type")
    if typ == "object":
        if not isinstance(value, dict):
            errors.append(f"{path}: expected object, got {type(value).__name__}")
            return
        for key in schema.get("required", []):
            if key not in value:
                errors.append(f"{path}: missing required key '{key}'")
        props = schema.get("properties", {})
        if schema.get("additionalProperties") is False:
            for key in value:
                if key not in props:
                    errors.append(f"{path}: unknown key '{key}'")
        for key, sub in props.items():
            if key in value:
                check_schema(value[key], sub, f"{path}.{key}", errors)
    elif typ == "array":
        if not isinstance(value, list):
            errors.append(f"{path}: expected array, got {type(value).__name__}")
            return
        if "minItems" in schema and len(value) < schema["minItems"]:
            errors.append(f"{path}: fewer than {schema['minItems']} items")
        item_schema = schema.get("items")
        if item_schema:
            for i, item in enumerate(value):
                check_schema(item, item_schema, f"{path}[{i}]", errors)
    elif typ == "string":
        if not isinstance(value, str):
            errors.append(f"{path}: expected string, got {type(value).__name__}")
            return
        if "enum" in schema and value not in schema["enum"]:
            errors.append(f"{path}: '{value}' not in {schema['enum']}")
        if "pattern" in schema and not re.fullmatch(schema["pattern"], value):
            errors.append(f"{path}: '{value}' does not match /{schema['pattern']}/")
        if "minLength" in schema and len(value) < schema["minLength"]:
            errors.append(f"{path}: shorter than {schema['minLength']} chars")
    else:
        errors.append(f"{path}: schema uses unsupported type '{typ}' — extend this script")


def parse_frontmatter(text, where, errors):
    """Return {key: value} for single-line keys in the leading --- block."""
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        errors.append(f"{where}: no frontmatter (must open with ---)")
        return {}
    fm = {}
    for line in lines[1:]:
        if line.strip() == "---":
            return fm
        m = re.match(r"^([A-Za-z_-]+):\s*(.*)$", line)
        if m:
            fm[m.group(1)] = m.group(2).strip()
        elif line.strip():
            errors.append(f"{where}: frontmatter line not 'key: value' (multi-line values unsupported): {line[:60]!r}")
    errors.append(f"{where}: frontmatter never closed with ---")
    return fm


LINK_RE = re.compile(r"\]\(([^)#\s]+)(?:#[^)\s]*)?\)")


def check_links(md_path, skills_root, errors):
    for target in LINK_RE.findall(md_path.read_text(encoding="utf-8")):
        if re.match(r"^[a-z]+://", target) or target.startswith("mailto:"):
            continue
        resolved = (md_path.parent / target).resolve()
        if not resolved.exists():
            errors.append(f"{md_path.relative_to(skills_root.parent)}: broken link -> {target}")


JSON_FENCE_RE = re.compile(r"^```json\s*$(.*?)^```\s*$", re.MULTILINE | re.DOTALL)


def check_json_fences(md_path, repo_root, errors):
    """A ```json fence must hold one or more whitespace-separated JSON values
    (contract docs routinely show several example objects in one fence)."""
    decoder = json.JSONDecoder()
    for i, block in enumerate(JSON_FENCE_RE.findall(md_path.read_text(encoding="utf-8")), 1):
        pos, text = 0, block.strip()
        while pos < len(text):
            try:
                _, end = decoder.raw_decode(text, pos)
            except json.JSONDecodeError as e:
                errors.append(
                    f"{md_path.relative_to(repo_root)}: ```json block #{i} is not valid JSON"
                    f" ({e.msg}) — fix it or retag the fence (```jsonc)"
                )
                break
            pos = end
            while pos < len(text) and text[pos].isspace():
                pos += 1


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent,
                    help="repo root (default: this script's parent's parent)")
    root = ap.parse_args().root
    skills = root / "skills"
    errors = []

    try:
        registry = json.loads((skills / "registry.json").read_text(encoding="utf-8"))
        schema = json.loads((skills / "registry.schema.json").read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as e:
        print(f"FAIL: cannot load registry/schema: {e}", file=sys.stderr)
        return 1

    check_schema(registry, schema, "registry", errors)

    entries = registry.get("skills", []) if isinstance(registry, dict) else []
    ids = [e.get("id") for e in entries if isinstance(e, dict) and "id" in e]
    for dup in {i for i in ids if ids.count(i) > 1}:
        errors.append(f"registry: duplicate id '{dup}'")

    dirs = {p.parent.name for p in skills.glob("*/SKILL.md")}
    for missing in sorted(set(ids) - dirs):
        errors.append(f"registry id '{missing}' has no skills/{missing}/SKILL.md")
    for unlisted in sorted(dirs - set(ids)):
        errors.append(f"skills/{unlisted}/SKILL.md is not in registry.json")

    for skill_dir in sorted(dirs):
        md = skills / skill_dir / "SKILL.md"
        fm = parse_frontmatter(md.read_text(encoding="utf-8"), f"skills/{skill_dir}/SKILL.md", errors)
        name, desc = fm.get("name"), fm.get("description")
        if name != skill_dir:
            errors.append(f"skills/{skill_dir}/SKILL.md: frontmatter name '{name}' != directory id")
        if not desc:
            errors.append(f"skills/{skill_dir}/SKILL.md: missing description")
        else:
            if len(desc) > 1024:
                errors.append(f"skills/{skill_dir}/SKILL.md: description {len(desc)} chars (max 1024)")
            if not desc.startswith("Use when"):
                errors.append(f"skills/{skill_dir}/SKILL.md: description must start with 'Use when'")

    for md in sorted(skills.rglob("*.md")):
        check_links(md, skills, errors)
        check_json_fences(md, root, errors)
    for name in ("README.md", "TODO.md"):
        if (root / name).exists():
            check_links(root / name, skills, errors)

    if errors:
        print(f"FAIL: {len(errors)} problem(s)", file=sys.stderr)
        for e in errors:
            print(f"  - {e}", file=sys.stderr)
        return 1
    print(f"ok: {len(entries)} registered skill(s), frontmatter and all markdown links valid")
    return 0


if __name__ == "__main__":
    sys.exit(main())

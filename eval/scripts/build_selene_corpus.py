"""Freeze the `selene-src` corpus: git-tracked files of the Selene repo at a pinned commit.

Writes eval/corpora/selene-src/manifest.json (paths + sha256 + sizes; no file copies) and
materialises the files into eval/.cache/corpora/selene-src/ via `git archive` (read-only on the
Selene repo). Re-running with the same commit is byte-identical; `--verify` re-hashes the cache.

Usage: python eval/scripts/build_selene_corpus.py [--commit SHA] [--verify]
"""
import argparse, hashlib, io, json, os, subprocess, sys, tarfile

HERE = os.path.dirname(os.path.abspath(__file__))
EVAL = os.path.dirname(HERE)
SELENE = os.path.abspath(os.path.join(EVAL, "..", "..", "selene"))  # the Selene checkout next to this repository
DEFAULT_COMMIT = "e09802592ccb61e1764e1f0c2c1ffbb3e8de25d4"

# First-party code + docs. Third-party bundled skill text and tests are excluded.
INCLUDE_DIRS = [
    "lib/vectordb/", "lib/documents/", "lib/config/", "lib/settings/", "lib/ai/vector-search/",
    "lib/ai/providers/", "lib/ai/filesystem/", "lib/ai/ripgrep/", "lib/db/", "lib/auth/",
    "lib/mcp/", "lib/scheduler/", "lib/background-tasks/", "lib/agent-memory/", "lib/workspace/",
    "lib/context-window/", "lib/command-execution/", "lib/plugins/", "lib/utils/",
    "lib/observability/", "electron/", "selene-plugins/",
]
TOP_LEVEL_AI = "lib/ai/"  # only direct children (*.ts) of lib/ai
EXTS = (".ts", ".tsx", ".md", ".mjs", ".js")
EXCLUDE_SUBSTR = (".test.", "__tests__", "/bundled/", ".d.ts")


def git(*args, binary=False):
    out = subprocess.run(["git", "-C", SELENE, *args], capture_output=True, check=True)
    return out.stdout if binary else out.stdout.decode("utf-8")


def selected(path):
    if not path.endswith(EXTS) or any(s in path for s in EXCLUDE_SUBSTR):
        return False
    if "/" not in path:
        return path.endswith(".md")
    if path.startswith(TOP_LEVEL_AI) and path.count("/") == 2 and path.endswith((".ts", ".tsx")):
        return True
    return any(path.startswith(d) for d in INCLUDE_DIRS)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--commit", default=DEFAULT_COMMIT)
    ap.add_argument("--verify", action="store_true")
    a = ap.parse_args()
    man_dir = os.path.join(EVAL, "corpora", "selene-src")
    cache = os.path.join(EVAL, ".cache", "corpora", "selene-src")
    if a.verify:
        man = json.load(open(os.path.join(man_dir, "manifest.json"), encoding="utf-8"))
        bad = 0
        for f in man["files"]:
            p = os.path.join(cache, f["path"])
            h = hashlib.sha256(open(p, "rb").read()).hexdigest() if os.path.exists(p) else None
            if h != f["sha256"]:
                bad += 1
                print("MISMATCH", f["path"])
        print(f"verified {len(man['files'])} files, {bad} mismatches")
        sys.exit(1 if bad else 0)
    paths = sorted(p for p in git("ls-tree", "-r", "--name-only", a.commit).splitlines() if selected(p))
    blob = git("archive", "--format=tar", a.commit, "--", *paths, binary=True)
    files = []
    with tarfile.open(fileobj=io.BytesIO(blob)) as tar:
        for m in tar.getmembers():
            if not m.isfile():
                continue
            data = tar.extractfile(m).read()
            dest = os.path.join(cache, m.name)
            os.makedirs(os.path.dirname(dest), exist_ok=True)
            with open(dest, "wb") as fh:
                fh.write(data)
            text = data.decode("utf-8", errors="replace")
            files.append({
                "path": m.name,
                "sha256": hashlib.sha256(data).hexdigest(),
                "bytes": len(data),
                "lines": text.count("\n") + (0 if text.endswith("\n") or not text else 1),
                "max_line_chars": max((len(l) for l in text.split("\n")), default=0),
            })
    files.sort(key=lambda f: f["path"])
    os.makedirs(man_dir, exist_ok=True)
    man = {
        "corpus": "selene-src",
        "source": "Selene repository (first-party), git-tracked files only",
        "commit": a.commit,
        "materialise": "python eval/scripts/build_selene_corpus.py --commit <commit>",
        "include_dirs": INCLUDE_DIRS + ["lib/ai/*.ts (direct children)", "*.md (repo root)"],
        "exclude_substrings": list(EXCLUDE_SUBSTR),
        "file_count": len(files),
        "total_bytes": sum(f["bytes"] for f in files),
        "files": files,
    }
    with open(os.path.join(man_dir, "manifest.json"), "w", encoding="utf-8", newline="\n") as fh:
        json.dump(man, fh, indent=1)
        fh.write("\n")
    print(f"{len(files)} files, {man['total_bytes']} bytes -> {cache}")


if __name__ == "__main__":
    main()

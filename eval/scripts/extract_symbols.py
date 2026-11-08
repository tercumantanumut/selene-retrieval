"""Extract top-level declarations (with line spans, leading doc comment and cross-file references)
from the frozen selene-src corpus. Used to *propose* golden-set candidates and to compute
definition spans; every committed label is reviewed by hand (see eval/golden/README.md).

Usage: python eval/scripts/extract_symbols.py > eval/.cache/symbols.json
"""
import json, os, re, sys
from collections import defaultdict

EVAL = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ROOT = os.path.join(EVAL, ".cache", "corpora", "selene-src")
MAN = os.path.join(EVAL, "corpora", "selene-src", "manifest.json")

DECL = re.compile(
    r"^(?:export\s+)?(?:default\s+)?(?:declare\s+)?(?:async\s+)?"
    r"(?:(function)\*?\s+(\w+)|(?:abstract\s+)?(class)\s+(\w+)|(const|let)\s+(\w+)\s*[:=]|"
    r"(interface|type|enum)\s+(\w+))"
)


def strip_code(line, state):
    """Return the line with strings/comments blanked (crude lexer; state carries block comments
    and template literals across lines)."""
    out, i, n = [], 0, len(line)
    while i < n:
        c = line[i]
        if state["block"]:
            if line.startswith("*/", i):
                state["block"] = False
                i += 2
            else:
                i += 1
            continue
        if state["tpl"]:
            if c == "\\":
                i += 2
                continue
            if c == "`":
                state["tpl"] = False
            i += 1
            continue
        if line.startswith("//", i):
            break
        if line.startswith("/*", i):
            state["block"] = True
            i += 2
            continue
        if c in "'\"":
            j = i + 1
            while j < n and line[j] != c:
                j += 2 if line[j] == "\\" else 1
            i = j + 1
            continue
        if c == "`":
            state["tpl"] = True
            i += 1
            continue
        out.append(c)
        i += 1
    return "".join(out)


def decls(path, lines):
    res = []
    state = {"block": False, "tpl": False}
    depth = 0
    open_decl = None
    doc_start = None
    for idx, raw in enumerate(lines):
        ln = idx + 1
        stripped = raw.strip()
        if raw.startswith("export ") and DECL.match(raw):
            # resync: the crude lexer can be confused by regex literals containing quotes/backticks
            depth, state["block"], state["tpl"] = 0, False, False
            if open_decl:
                open_decl["end"] = max(open_decl["decl_line"], ln - 1)
                res.append(open_decl)
                open_decl = None
        at_top = depth == 0 and not state["block"] and not state["tpl"]
        if at_top and stripped.startswith("/**"):
            doc_start = ln
        m = DECL.match(raw) if at_top else None
        if m:
            kind = next(g for g in (m.group(1), m.group(3), m.group(5), m.group(7)) if g)
            name = next(g for g in (m.group(2), m.group(4), m.group(6), m.group(8)) if g)
            start = doc_start if doc_start and doc_start >= ln - 40 else ln
            doc = ""
            if start < ln:
                doc_lines = [l.strip().lstrip("/*").strip() for l in lines[start - 1:ln - 1]]
                doc = " ".join(l for l in doc_lines if l and not l.startswith("@"))[:200]
            open_decl = {"name": name, "kind": kind, "start": start, "decl_line": ln,
                         "exported": raw.startswith("export"), "doc": doc, "path": path}
        code = strip_code(raw, state)
        depth += code.count("{") + code.count("(") - code.count("}") - code.count(")")
        depth = max(depth, 0)
        if open_decl and depth == 0 and not state["tpl"] and not state["block"]:
            ends = stripped.endswith((";", "}", "})", "});", "};", ")", "]", "];")) or stripped == ""
            if ends or ln > open_decl["decl_line"]:
                open_decl["end"] = ln
                res.append(open_decl)
                open_decl = None
        if not stripped.startswith(("/**", "*", "*/")) and stripped and not m:
            if depth == 0 and not stripped.startswith("@"):
                doc_start = None
        if m:
            doc_start = None
    if open_decl:
        open_decl["end"] = len(lines)
        res.append(open_decl)
    return res


def main():
    man = json.load(open(MAN, encoding="utf-8"))
    texts, out = {}, []
    for f in man["files"]:
        p = f["path"]
        text = open(os.path.join(ROOT, p), encoding="utf-8", errors="replace").read().replace("\r\n", "\n")
        texts[p] = text
        if p.endswith((".ts", ".tsx", ".js", ".mjs")):
            out.extend(decls(p, text.split("\n")))
    # cross-file references (word-boundary match) for exported names
    words = defaultdict(set)
    for p, t in texts.items():
        for w in set(re.findall(r"[A-Za-z_]\w{3,}", t)):
            words[w].add(p)
    for d in out:
        d["ref_files"] = sorted(words.get(d["name"], set()) - {d["path"]})
    json.dump(out, sys.stdout, indent=0)


if __name__ == "__main__":
    main()

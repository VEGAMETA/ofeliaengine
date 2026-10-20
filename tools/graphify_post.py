r"""The engine's layer over graphify's knowledge graph (`graphify-out/`, team decision
2026-10-20).

A map for agents new to a subsystem, kept off the commit path: no git hooks rebuild it
(they did after every commit and merge, CPU the perf checks then wait out). graphify
extracts code by its syntax for free; documents get their headings that way and their
concepts only from an LLM pass. This adds what can be had without one; run it after every
rebuild (`graphify update .` for the code, `/graphify` for an LLM pass):

- **References:** `ADR NNNN`, `engine_*` crates and `docs/*.md` paths named in the ADRs,
  the research notes and the dev guide become edges between those documents and crates.
- **Mentions:** an identifier in backticks (`FrameGraph`) becomes an edge from the section
  that names it to the code of that name, looked up only in the crates the document names
  and only when exactly one matches.
- **Noise:** guessed edges between a document and code outside the crates it names (name
  collisions, a `Decision` heading tied to a C++ `image()`) are dropped.
- **Paths:** a concept whose `source_file` names no file on disk (an agent once wrote Windows
  paths in unescaped strings, `docs\adr` turning into control characters) gets its document
  back from its id's stem; graphify's update drops concepts of files it cannot find.
- **Names:** communities containing the anchor nodes of `tools/graphify_labels.json` get
  those names (anchors survive reclustering, community numbers do not); the rest are
  named after their most connected node and where it lives.

Run with graphify's Python (`graphify-out/.graphify_python`):

    python tools/graphify_post.py                  # refresh the layer, the report, graph.html
    python tools/graphify_post.py --recluster      # the same after clustering again (manual runs)
    python tools/graphify_post.py --outlines       # outlines of the docs for an LLM pass
    python tools/graphify_post.py --install-hooks  # if graphify's hooks are ever installed again

Outlines (`graphify-out/outline/`) keep each document's headings, its first paragraphs and
its decision-like sections, about a third of the text: what an LLM pass reads instead of the
documents. The pass itself is graphify's (`/graphify`, its subagents told to read them).

Measured 2026-10-20: for exact questions (who calls a function) search is faster and right;
the graph links by name, so it misses method calls. It is for orientation.
"""

import collections
import json
import re
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "graphify-out"
LABELS = ROOT / "tools" / "graphify_labels.json"
DOC_DIRS = ("docs/adr", "docs/research", "docs/dev-guide")
# Edges this script owns: removed and made again on every run.
OWN = ("doc-ref", "doc-mention")
# Identifiers too common to tie a mention to one piece of code.
STOP = {"self", "true", "false", "none", "some", "default", "new", "from", "into", "build",
        "main", "test", "tests", "error", "result", "option", "string", "with", "lib"}
HOOKS = ("post-commit", "post-merge", "post-checkout", "post-rewrite")
HOOK_MARK = "# ofeliaengine: tools/graphify_post.py"


def stem_id(rel: str) -> str:
    """graphify's node id for a whole file: every path segment, the extension dropped."""
    p = Path(rel).with_suffix("")
    return "_".join(re.sub(r"[^a-z0-9]", "_", part.lower()) for part in p.parts)


def norm(path) -> str:
    return str(path or "").replace("\\", "/")


def crate_of(node: dict):
    p = norm(node.get("source_file")).split("/")
    return p[1] if len(p) > 1 and p[0] == "crates" else None


def is_doc(node: dict) -> bool:
    return norm(node.get("source_file")).endswith(".md")


def documents():
    for d in DOC_DIRS:
        for p in sorted((ROOT / d).rglob("*.md")):
            yield p.relative_to(ROOT).as_posix(), p.read_text(encoding="utf-8", errors="ignore")


# --- Outlines -------------------------------------------------------------------------------

KEEP_WHOLE = ("decision", "what we take", "what we do not take", "conclusion", "summary",
              "recommendation", "consequences", "status", "in short")
WHOLE_CAP, PARA_CAP = 900, 220  # characters kept under one heading


def outline(text: str) -> str:
    out, whole_level, para, in_code, used = [], 0, "none", False, 0
    for line in text.splitlines():
        if line.strip().startswith("```"):
            in_code = not in_code
            continue
        if in_code:
            continue
        h = re.match(r"(#+)\s+(.*)", line)
        if h:
            level = len(h.group(1))
            if whole_level and level <= whole_level:
                whole_level = 0
            if not whole_level and h.group(2).strip().lower().startswith(KEEP_WHOLE):
                whole_level = level
            out.append(line)
            para, used = "wait", 0
            continue
        if line.lstrip().startswith("|") or used >= (WHOLE_CAP if whole_level else PARA_CAP):
            continue
        if whole_level or (para == "wait" and line.strip()) or (para == "in" and line.strip()):
            out.append(line)
            used += len(line)
            if para == "wait":
                para = "in"
        elif para == "in":
            out.append("")
            para = "none"
    return "\n".join(out).strip() + "\n"


def write_outlines():
    total_in = total_out = 0
    for rel, text in documents():
        body = f"<!-- outline of {rel}; the source file is {ROOT / rel} -->\n" + outline(text)
        target = OUT / "outline" / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(body, encoding="utf-8", newline="\n")
        total_in += len(text)
        total_out += len(body)
    print(f"outlines: {total_in / 1e6:.2f}M -> {total_out / 1e6:.2f}M characters in {OUT / 'outline'}")


# --- The layer ------------------------------------------------------------------------------

def pair(a: str, b: str) -> tuple:
    """An edge's endpoints in either direction: the graph is undirected."""
    return (a, b) if a <= b else (b, a)


def reference_edges(nodes: dict, existing: set) -> list:
    crates = {p.name for p in (ROOT / "crates").iterdir() if p.is_dir()}
    adrs = {}
    for p in (ROOT / "docs/adr").glob("*.md"):
        m = re.match(r"(\d{4})-", p.name)
        if m:
            adrs[m.group(1)] = p.relative_to(ROOT).as_posix()
    edges = []
    for rel, text in documents():
        src = stem_id(rel)
        if src not in nodes:
            continue
        for r in sorted(set(re.findall(r"ADR \d{4}|engine_\w+|docs/[\w/.-]+\.md", text))):
            if r.startswith("ADR "):
                to = adrs.get(r[4:])
                tgt = stem_id(to) if to and to != rel else None
            elif r.startswith("engine_"):
                tgt = f"crates_{r}_src_lib" if r in crates else None
            else:
                tgt = stem_id(r) if (ROOT / r).exists() and r != rel else None
            if tgt in nodes and pair(src, tgt) not in existing:
                existing.add(pair(src, tgt))
                edges.append({"source": src, "target": tgt, "relation": "references",
                              "confidence": "EXTRACTED", "confidence_score": 1.0,
                              "source_file": rel, "source_location": None, "weight": 1.0,
                              "_origin": "semantic", "context": "doc-ref"})
    return edges


def mention_edges(nodes: dict, existing: set) -> list:
    by_name = collections.defaultdict(list)
    for n in nodes.values():
        crate = crate_of(n)
        if crate and n.get("file_type") == "code" and "/src/" in norm(n.get("source_file")):
            name = re.sub(r"\(\)$", "", str(n.get("label", ""))).strip()
            if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]{3,}", name):
                by_name[(crate, name)].append(n["id"])
    sections = collections.defaultdict(list)
    for n in nodes.values():
        m = re.fullmatch(r"L(\d+)", str(n.get("source_location") or ""))
        if m and n.get("_origin") == "ast" and is_doc(n):
            sections[norm(n.get("source_file"))].append((int(m.group(1)), n["id"]))
    for v in sections.values():
        v.sort()
    edges = []
    for rel, text in documents():
        crates = set(re.findall(r"\b(engine_\w+)", text))
        if not crates or not sections.get(rel):
            continue
        for lineno, line in enumerate(text.splitlines(), 1):
            for ident in re.findall(r"`([A-Za-z_][A-Za-z0-9_]*)(?:::[A-Za-z0-9_]+)*(?:\(\))?`", line):
                if ident.lower() in STOP or len(ident) < 4:
                    continue
                targets = {t for c in crates for t in by_name.get((c, ident), [])}
                if len(targets) != 1:
                    continue
                before = [sid for ln, sid in sections[rel] if ln <= lineno]
                src = before[-1] if before else sections[rel][0][1]
                tgt = next(iter(targets))
                if pair(src, tgt) in existing:
                    continue
                existing.add(pair(src, tgt))
                edges.append({"source": src, "target": tgt, "relation": "references",
                              "confidence": "INFERRED", "confidence_score": 0.95,
                              "source_file": rel, "source_location": f"L{lineno}",
                              "weight": 1.0, "_origin": "semantic", "context": "doc-mention"})
    return edges


def is_noise(edge: dict, nodes: dict, doc_crates: dict) -> bool:
    if edge.get("confidence") != "INFERRED" or edge.get("context") in OWN:
        return False
    a, b = nodes.get(edge["source"]), nodes.get(edge["target"])
    if not a or not b or is_doc(a) == is_doc(b):
        return False
    doc, code = (a, b) if is_doc(a) else (b, a)
    return crate_of(code) not in doc_crates.get(norm(doc.get("source_file")), set())


def repair_paths(g: dict) -> int:
    """Gives semantic nodes, edges and hyperedges whose source_file is not a file on disk the
    document their id starts with. Returns how many were repaired."""
    docs = {stem_id(p.relative_to(ROOT).as_posix()): p.relative_to(ROOT).as_posix()
            for p in (ROOT / "docs").rglob("*.md")}
    stems = sorted(docs, key=len, reverse=True)

    def doc_of(node_id: str):
        return next((docs[s] for s in stems if node_id == s or node_id.startswith(s + "_")), None)

    def bad(path) -> bool:
        p = str(path or "")
        return any(ord(c) < 32 for c in p) or not p or not (ROOT / norm(p)).exists()

    fixed = 0
    for item, ids in ([(n, [n["id"]]) for n in g["nodes"] if n.get("_origin") == "semantic"]
                      + [(e, [e["source"], e["target"]]) for e in g["links"] if e.get("_origin") == "semantic"]
                      + [(h, list(h.get("nodes", []))) for h in g.get("hyperedges", [])]):
        if bad(item.get("source_file")):
            doc = next((doc_of(i) for i in ids if doc_of(i)), None)
            if doc:
                item["source_file"] = doc
                fixed += 1
    return fixed


def community_names(nodes: dict, communities: dict, degree: dict) -> dict:
    anchors = json.loads(LABELS.read_text(encoding="utf-8")) if LABELS.exists() else {}
    where_anchor = {a: label for label, ids in anchors.items() for a in ids}
    names, taken = {}, set()
    for cid in sorted(communities, key=lambda c: -len(communities[c])):
        members = communities[cid]
        votes = collections.Counter(where_anchor[m] for m in members if m in where_anchor)
        label = next((lab for lab, _ in votes.most_common() if lab not in taken), None)
        if label:
            taken.add(label)
            names[cid] = label
            continue
        top = max(members, key=lambda m: degree.get(m, 0))
        lab = re.sub(r"\s+", " ", str(nodes[top].get("label", top))).strip()[:40]
        p = norm(nodes[top].get("source_file")).split("/")
        where = p[1] if len(p) > 1 and p[0] in ("crates", "docs") else p[0]
        names[cid] = f"{lab} ({where})" if where else lab
    return names


def refresh(recluster: bool):
    started = time.perf_counter()
    graph_path = OUT / "graph.json"
    g = json.loads(graph_path.read_text(encoding="utf-8"))
    repaired = repair_paths(g)
    nodes = {n["id"]: n for n in g["nodes"]}
    doc_crates = {rel: set(re.findall(r"\b(engine_\w+)", t)) for rel, t in documents()}
    # Docs outside the three directories still name their crates for the noise rule.
    for p in (ROOT / "docs").rglob("*.md"):
        rel = p.relative_to(ROOT).as_posix()
        if rel not in doc_crates:
            doc_crates[rel] = set(re.findall(r"\b(engine_\w+)", p.read_text(encoding="utf-8", errors="ignore")))
    links = [e for e in g["links"] if e.get("context") not in OWN]
    before = len(links)
    links = [e for e in links if not is_noise(e, nodes, doc_crates)]
    dropped = before - len(links)
    existing = {pair(e["source"], e["target"]) for e in links}
    refs = reference_edges(nodes, existing)
    mentions = mention_edges(nodes, existing)
    links += refs + mentions
    g["links"] = links

    if recluster:
        from graphify.build import build_from_json
        from graphify.cluster import cluster
        for n in g["nodes"]:
            n.pop("community", None)
            n.pop("community_name", None)
        extraction = {"nodes": g["nodes"], "edges": links, "hyperedges": g.get("hyperedges", [])}
        G = build_from_json(extraction, root=str(ROOT), directed=False)
        communities = {int(k): list(v) for k, v in cluster(G).items()}
        for cid, members in communities.items():
            for m in members:
                if m in nodes:
                    nodes[m]["community"] = cid
    communities = collections.defaultdict(list)
    for n in g["nodes"]:
        if n.get("community") is not None:
            communities[int(n["community"])].append(n["id"])
    degree = collections.Counter()
    for e in links:
        degree[e["source"]] += 1
        degree[e["target"]] += 1
    names = community_names(nodes, communities, degree)
    for n in g["nodes"]:
        if n.get("community") is not None:
            n["community_name"] = names[int(n["community"])]
    tmp = graph_path.with_suffix(".json.partial")
    tmp.write_text(json.dumps(g, ensure_ascii=False), encoding="utf-8")
    tmp.replace(graph_path)
    (OUT / ".graphify_labels.json").write_text(
        json.dumps({str(k): v for k, v in names.items()}, ensure_ascii=False), encoding="utf-8")
    write_report_and_html(g, dict(communities), names)
    print(f"[graphify_post] {len(refs)} references, {len(mentions)} mentions, {dropped} noisy "
          f"edges dropped, {repaired} paths repaired, {len(communities)} communities named "
          f"({time.perf_counter() - started:.1f} s)")


def write_report_and_html(g: dict, communities: dict, names: dict):
    from graphify.analyze import god_nodes, suggest_questions, surprising_connections
    from graphify.cluster import score_all
    from graphify.detect import detect
    from graphify.exporters.html import _viz_node_limit
    from graphify.export import to_html
    from graphify.paths import load_node_link_graph, out_path
    from graphify.report import generate
    from graphify.watch import _read_build_excludes, _read_build_gitignore

    G = load_node_link_graph(g)
    communities = {c: [m for m in ms if m in G] for c, ms in communities.items()}
    cohesion = score_all(G, communities)
    detection = detect(ROOT, extra_excludes=_read_build_excludes(out_path()),
                       gitignore=_read_build_gitignore(out_path()))
    cost = OUT / "cost.json"
    runs = json.loads(cost.read_text(encoding="utf-8"))["runs"] if cost.exists() else []
    tokens = {"input": runs[-1]["input_tokens"] if runs else 0, "output": 0}
    report = generate(G, communities, cohesion, names, god_nodes(G),
                      surprising_connections(G, communities), detection, tokens, str(ROOT),
                      suggested_questions=suggest_questions(G, communities, names))
    (OUT / "GRAPH_REPORT.md").write_text(report, encoding="utf-8")
    to_html(G, communities, str(OUT / "graph.html"), community_labels=names,
            node_limit=_viz_node_limit())


# --- Hooks ----------------------------------------------------------------------------------

# Python inside the hooks' double-quoted shell string: no double quotes, dollars or backticks.
HOOK_CALL = f"""    {HOOK_MARK}
    try:
        import runpy
        _gfy_post = _root / 'tools' / 'graphify_post.py'
        if _gfy_post.is_file():
            sys.argv = [str(_gfy_post)]
            runpy.run_path(str(_gfy_post), run_name='__main__')
    except Exception as _gfy_exc:
        print(f'[graphify_post] failed: {{_gfy_exc}}')
"""


def install_hooks():
    hooks = ROOT / ".git" / "hooks"
    for name in HOOKS:
        path = hooks / name
        if not path.exists():
            print(f"{name}: not installed (run `graphify hook install` first)")
            continue
        text = path.read_text(encoding="utf-8")
        if HOOK_MARK in text:
            print(f"{name}: already calls tools/graphify_post.py")
            continue
        m = re.search(r"^    _rebuild_code\(.*\)\n", text, re.M)
        if not m:
            print(f"{name}: no `_rebuild_code(...)` line found, left as it is")
            continue
        path.write_text(text[:m.end()] + HOOK_CALL + text[m.end():], encoding="utf-8", newline="\n")
        print(f"{name}: calls tools/graphify_post.py after graphify's rebuild")


if __name__ == "__main__":
    args = sys.argv[1:]
    if "--install-hooks" in args:
        install_hooks()
    elif "--outlines" in args:
        write_outlines()
    elif (OUT / "graph.json").exists():
        refresh(recluster="--recluster" in args)
    else:
        print("[graphify_post] no graphify-out/graph.json yet: build the graph first (/graphify)")

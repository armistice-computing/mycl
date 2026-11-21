#!/usr/bin/env python3
"""Differential oracle DRIVER — the compare() gate of the RDF<->ULTRA roundtrip,
applied to tie the proved Lean reconcile MODEL to the Rust RUNTIME.

It runs BOTH evaluators on the SAME corpus and asserts set-equality of their
canon-deltas, per scenario, per axis (diff / l5-face / required-sweep). A
divergence is THE finding: it reports the exact (s, p, canon) triple present on
one side and absent on the other, labels the direction (RUST-ONLY / LEAN-ONLY)
and the axis, and exits non-zero. It never papers over.

  python3 driver/oracle.py            # run both evaluators + compare all axes
  python3 driver/oracle.py --no-build # skip cargo/lean, compare existing out/

The diff axis is the load-bearing equivalence: if Lean MODEL == Rust RUNTIME
over the corpus, the proved L1-L5 laws transfer to the runtime EMPIRICALLY over
the corpus (strong differential evidence, not a proof of the Rust). The l5-face
axis USED to diverge on the fragment-subtree / bare-predicate-restricted
scenarios (the flagged L5 gap); that gap is now CLOSED — the Lean faceOf was
refined to the runtime owned-span (subtree-prefix STRSTARTS OR bare-restricted-
predicate) and L5_disjoint_face_compose re-proved sorry-free over scope-
disjointness, so L5 is 4/4 set-equal. Any L5 divergence is now a real bug.
"""
import json
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
DIFF_DIR = HERE.parent                       # .../differential
CORPUS = DIFF_DIR / "corpus"
OUT = DIFF_DIR / "out"
REPO = DIFF_DIR.parent.parent.parent         # garden-proto-mo root
LEAN_DIR = DIFF_DIR.parent / "lean"          # the proved Vocab lib
ELAN = "/Users/someone/.elan/bin"

# axes that DIVERGE by design — reported, not failed. EMPTY now: the L5 face-scope
# gap is CLOSED (Lean faceOf refined to the runtime owned-span: subtree-prefix OR
# bare-restricted-predicate, Reconcile.lean; L5_disjoint_face_compose re-proved
# sorry-free over scope-disjointness). L5 is 4/4 set-equal — any L5 divergence is
# now a real BUG, not an expected gap.
EXPECTED_DIVERGENT_AXES = set()


def run_rust():
    print("[driver] building + running the Rust evaluator (cargo test diff_oracle)...")
    r = subprocess.run(
        ["cargo", "test",
         "--manifest-path", str(REPO / "src-tauri" / "Cargo.toml"),
         "--no-default-features", "--features", "headless",
         "--lib", "diff_oracle"],
        capture_output=True, text=True)
    if r.returncode != 0:
        print(r.stdout[-3000:]); print(r.stderr[-3000:])
        raise SystemExit("[driver] Rust evaluator FAILED to build/run")


def run_lean(axis, corpus_file, out_file):
    import os
    env = dict(os.environ)
    env["PATH"] = ELAN + ":" + env.get("PATH", "")
    print(f"[driver] running the Lean evaluator (axis={axis})...")
    with open(CORPUS / corpus_file) as f:
        corpus_text = f.read()
    r = subprocess.run(
        ["lake", "env", "lean", "--run",
         str(DIFF_DIR / "lean" / "Main.lean"), axis],
        cwd=str(LEAN_DIR), input=corpus_text,
        capture_output=True, text=True, env=env)
    if r.returncode != 0:
        print(r.stdout[-2000:]); print(r.stderr[-2000:])
        raise SystemExit(f"[driver] Lean evaluator FAILED (axis={axis})")
    OUT.mkdir(exist_ok=True)
    (OUT / out_file).write_text(r.stdout)


def lean_build():
    import os
    env = dict(os.environ)
    env["PATH"] = ELAN + ":" + env.get("PATH", "")
    print("[driver] lake build (compile Vocab oleans)...")
    r = subprocess.run(["lake", "build"], cwd=str(LEAN_DIR),
                       capture_output=True, text=True, env=env)
    if r.returncode != 0:
        print(r.stdout[-2000:]); print(r.stderr[-2000:])
        raise SystemExit("[driver] lake build FAILED")


# ── normal form + set comparison ──────────────────────────────────────────

def cv_key(cv):
    rank = {"uri": 0, "num": 1, "dt": 2, "bool": 3, "lit": 4, "placeholder": 5}[cv["k"]]
    val = "" if cv["k"] == "placeholder" else str(cv["v"])
    return (rank, val)


def triple_key(t):
    return (t[0], t[1]) + cv_key(t[2])


def to_set(triples):
    return {triple_key(t): t for t in triples}


def fmt(t):
    return json.dumps(t, separators=(",", ":"))


def compare_delta(name, axis, rust_delta, lean_delta, findings):
    ok = True
    for side in ("removes", "adds"):
        rs = to_set(rust_delta.get(side, []))
        ls = to_set(lean_delta.get(side, []))
        rust_only = [rs[k] for k in rs.keys() - ls.keys()]
        lean_only = [ls[k] for k in ls.keys() - rs.keys()]
        for t in rust_only:
            findings.append((axis, name, side, "RUST-ONLY", t)); ok = False
        for t in lean_only:
            findings.append((axis, name, side, "LEAN-ONLY", t)); ok = False
    return ok


def compare_axis(axis, rust_file, lean_file):
    rust = {e["name"]: e["delta"] for e in json.loads((OUT / rust_file).read_text())}
    lean = {e["name"]: e["delta"] for e in json.loads((OUT / lean_file).read_text())}
    findings = []
    names = sorted(set(rust) | set(lean))
    agree = 0
    for n in names:
        if n not in rust:
            findings.append((axis, n, "-", "MISSING-RUST", None)); continue
        if n not in lean:
            findings.append((axis, n, "-", "MISSING-LEAN", None)); continue
        if compare_delta(n, axis, rust[n], lean[n], findings):
            agree += 1
    return findings, agree, len(names)


def compare_required():
    rust = {e["name"]: e for e in json.loads((OUT / "rust-required-delta.json").read_text())}
    lean = {e["name"]: e for e in json.loads((OUT / "lean-required-delta.json").read_text())}
    findings = []
    agree = 0
    names = sorted(set(rust) | set(lean))
    for n in names:
        rc = rust[n]["complete"]
        lc = lean[n]["complete"]
        if rc == lc:
            agree += 1
        else:
            findings.append(("required", n, "complete",
                             f"RUST={rc} LEAN={lc}", None))
    return findings, agree, len(names)


def main():
    build = "--no-build" not in sys.argv
    if build:
        run_rust()
        lean_build()
        run_lean("diff", "diff.json", "lean-diff-delta.json")
        run_lean("l5", "l5-face.json", "lean-l5-delta.json")
        run_lean("required", "required-sweep.json", "lean-required-delta.json")

    print("\n" + "=" * 72)
    print("DIFFERENTIAL ORACLE — Lean MODEL  vs  Rust RUNTIME  (over the corpus)")
    print("=" * 72)

    all_findings = []
    hard_fail = False

    for axis, rust_f, lean_f in [
        ("diff", "rust-diff-delta.json", "lean-diff-delta.json"),
        ("l5", "rust-l5-delta.json", "lean-l5-delta.json"),
    ]:
        findings, agree, total = compare_axis(axis, rust_f, lean_f)
        status = "AGREE" if not findings else ("DIVERGE (expected)" if axis in EXPECTED_DIVERGENT_AXES else "DIVERGE")
        print(f"\n[{axis}] {agree}/{total} scenarios set-equal — {status}")
        all_findings += findings
        if findings and axis not in EXPECTED_DIVERGENT_AXES:
            hard_fail = True

    rfind, ragree, rtotal = compare_required()
    rstatus = "AGREE" if not rfind else "DIVERGE"
    print(f"\n[required] {ragree}/{rtotal} class-instances agree on requiredComplete — {rstatus}")
    all_findings += rfind
    if rfind:
        hard_fail = True

    if all_findings:
        print("\n" + "-" * 72)
        print("SYMMETRIC DIFFERENCE (the findings — never papered over):")
        print("-" * 72)
        for axis, name, side, direction, t in all_findings:
            tag = "[GAP]" if axis in EXPECTED_DIVERGENT_AXES else "[BUG?]"
            loc = fmt(t) if t is not None else ""
            print(f"  {tag} axis={axis} scenario={name} {side} {direction} {loc}")

    # The L5 gap explanation (the flagged finding, reported precisely).
    if any(a == "l5" for a, *_ in all_findings):
        print("\n" + "-" * 72)
        print("L5 FACE-SCOPE FINDING (model infidelity, EXPECTED):")
        print("-" * 72)
        print("  The Rust RUNTIME scopes each face by STRSTARTS(STR(?s),\"{subject}#\")")
        print("  (subtree) OR (?s=<bare> && ?p in this-face-predicates)")
        print("  [document_meaningful_object.rs:84-106]. The Lean MODEL's faceOf uses")
        print("  EXACT subject-IRI membership [Reconcile.lean:123]. So the proved L5")
        print("  law's faceOf scope UNDER-APPROXIMATES the runtime subtree+predicate")
        print("  scope: it diverges on fragment-subtree orphans and bare-predicate-")
        print("  restricted triples, and the L5 guarantee transfers to the runtime")
        print("  ONLY on the corpus subset where the two scopes coincide (bare-subject-")
        print("  only, single-fragment docs). FIX (NEXT, not this spec): refine Lean")
        print("  faceOf to a prefix/predicate predicate and re-prove L5.")

    # Required-sweep scope note (referential conjuncts are out of the Lean model).
    print("\n" + "-" * 72)
    print("REQUIRED-SWEEP SCOPE (reported, not papered-over):")
    print("-" * 72)
    print("  The oracle compares ONLY the required-presence conjunct")
    print("  (Lean requiredComplete <-> the Rust 'missing required' failures).")
    print("  The Rust sweep ALSO does referential/type checks with NO Lean")
    print("  counterpart: AgentRun wf:node must be an AgentNode of THIS workflow")
    print("  [asserts.rs:303-315], and Contract/Archetype rdf:type-discovery +")
    print("  re-sweep [asserts.rs:318+]. Those are OUT of the current Lean model")
    print("  (a scoped-out face) — aligning them is NEXT.")

    print("\n" + "=" * 72)
    if hard_fail:
        print("RESULT: UNEXPECTED DIVERGENCE — model<->runtime disagree (Rust bug or")
        print("        model infidelity). See findings above. EXIT 1.")
        sys.exit(1)
    else:
        print("RESULT: diff + l5 + required AGREE over the corpus. The L5 face-scope")
        print("        gap is CLOSED — Lean faceOf refined to the runtime owned-span")
        print("        (subtree-prefix OR bare-restricted-predicate), L5 re-proved")
        print("        sorry-free. Lean MODEL == Rust RUNTIME on ALL axes (strong")
        print("        differential evidence). EXIT 0.")
        sys.exit(0)


if __name__ == "__main__":
    main()

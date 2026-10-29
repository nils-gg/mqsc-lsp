#!/usr/bin/env python3
"""Oracle differential: validate model/mqsc-model.json + mqsc-overlay.json
against a live IBM MQ queue manager (runmqsc), per user decision 2026-10-29:
"treat the oracle as authoritative".

How the oracle works (verified on MQ 10.0.1, /opt/mqm, qmgr ORACLE):
appending an unknown parameter `ZORACLEBAD(1)` to any MQSC command is always
a syntax error — nothing executes — but runmqsc then prints the command's
FULL syntax: AMQ8427I "Valid syntax" railroad (every parameter, value type,
enum list, FLAG|NOFLAG pair) or AMQ8426I "Valid ... commands are" list.
A failing parse never runs, so even STOP/DELETE/SET probes are safe.

Outputs (in --out, default /tmp/mq-oracle):
  raw-*.txt        raw runmqsc transcripts (IBM output — never committed)
  oracle.json      parsed reference model: {pair: {params, positional, objects...}}
  report.json      diff oracle vs our merged model, machine-readable
  report.md        human summary
Every param name in the report also carries its usage count in the local
gitignored dmpmqcfg/ corpus, so platform-noise is visible per entry.

Usage: python3 tools/mq_oracle_check.py [--qm ORACLE] [--mqm-bin /opt/mqm/bin]
Run from the repo root. Read-only against the model; fixes are applied by
hand (or follow-up commits) with the oracle cited as evidence.
"""
import argparse, collections, json, os, re, subprocess, sys

BOGUS = "ZORACLEBAD(1)"          # never a valid parameter: forces a parse error
NONAME = "ZZORACLENOOBJ"         # object-discovery probe object
POS = "ZZORACLEPOS"              # positional name filler
EXCLUDE_VERBS = {"END", "EXIT", "QUIT"}   # session-control: never probe

AMQ_ANCHOR = re.compile(r"^\s*(\d+)\s*:\s*(.*)$")
# NAME( values )  — name must be 2+ uppercase/digits, directly followed by (
ITEM_PARAM = re.compile(r"\b([A-Z][A-Z0-9]{1,19})\(([^()]*(?:\([^()]*\)[^()]*)*)\)")
# FLAG | NOFLAG pairs: word before the bar must be all-uppercase
ITEM_PAIR = re.compile(r"\b([A-Z][A-Z0-9]{1,19})\s*\|\s*([A-Z][A-Z0-9]{1,19})\b")


def run_session(qm, lines, mqm_bin, timeout=300):
    """Feed lines to runmqsc, return raw stdout+stderr text."""
    proc = subprocess.run(
        [os.path.join(mqm_bin, "runmqsc"), qm],
        input="\n".join(lines) + "\nEND\n",
        capture_output=True, text=True, timeout=timeout)
    return proc.stdout + proc.stderr


def split_transcript(raw):
    """runmqsc echoes `     N : command` per line; block = output until next."""
    out, num_line, cmd, block = {}, None, "", []
    for line in raw.splitlines():
        m = AMQ_ANCHOR.match(line)
        if m:
            if num_line is not None:
                out[num_line] = (cmd, block)
            num_line, cmd, block = int(m.group(1)), m.group(2), []
        elif num_line is not None:
            block.append(line)
    if num_line is not None:
        out[num_line] = (cmd, block)
    return out


def parse_railroad(block, obj_name=None):
    """AMQ8427I block -> {params: {name: {raw, kind, values, required}},
    flag_pairs, positional}. Railroad enum lists wrap across lines
    ("SDR | SVR |" / "CLUSSDR | ..."); join the body into one line so
    NAME( ... ) items reassemble. runmqsc truncates the print at the parse
    failure point, so items before the first '[' are the REQUIRED ones.
    obj_name excludes the header's own `OBJECT( q_name )` token from being
    read as a parameter; FLAG|NOFLAG pairs only count OUTSIDE any NAME( ... )
    span, so enum values (ACCTQ( QMGR | ON )) never pose as flags."""
    try:
        start = next(i for i, l in enumerate(block) if "AMQ8427I" in l)
    except StopIteration:
        return None
    body = block[start + 1:]
    text = " ".join(l.rstrip() for l in body)
    params, pairs = {}, {}
    first_br = text.find("[")
    spans = []
    for m in ITEM_PARAM.finditer(text):
        name, raw = m.group(1), m.group(2).strip()
        spans.append((m.start(), m.end()))
        if obj_name and name == obj_name:
            continue
        if name in ("WHERE",):  # DISPLAY WHERE/SELECT are free-text filters
            kind, values = "filter", []
        elif "|" in raw:
            kind, values = "enum", [v.strip() for v in raw.split("|")]
        elif "integer" in raw:
            kind, values = "integer", []
        elif raw == "string":
            kind, values = "string", []
        elif raw.endswith(("_name", "_qname", "q_name")) or "name" in raw:
            kind, values = "name", []
        else:
            kind, values = "other", []
        required = first_br == -1 or m.start() < first_br
        params.setdefault(name, {"raw": raw, "kind": kind, "values": values,
                                 "required": required})
    for m in ITEM_PAIR.finditer(text):
        if any(s <= m.start() < e for s, e in spans):
            continue
        pairs.setdefault(m.group(1), []).append(m.group(2))
    hdr = next((l for l in body if l.startswith("  ")), "")
    hm = re.match(r"^\s*([A-Z][A-Z0-9]*)\s+([A-Z][A-Z0-9]*)\s*\(([^)]*)\)", hdr)
    positional = hm.group(3).strip() if hm else None
    return {"params": params, "flag_pairs": pairs, "positional": positional}


def fill_value(p):
    """A plausible filler for a required parameter, so the next probe shape
    gets past it and the railroad continues printing."""
    return {"enum": p["values"][0] if p["values"] else "ZZORACLEPOS",
            "integer": "1", "filter": "zzz", "other": "ZZORACLEPOS",
            "name": "ZZORACLEPOS", "string": "ZZORACLEPOS"}[p["kind"]]


def absorb(acc, rr):
    """Merge one railroad print into the pair's accumulated facts."""
    if acc.get("unknown_command") or acc.get("no_railroad"):
        acc.clear()
    acc.setdefault("params", {}).update(rr["params"])
    fp = acc.setdefault("flag_pairs", {})
    for k, v in rr["flag_pairs"].items():
        fp.setdefault(k, []).extend(x for x in v if x not in fp[k])
    acc["positional"] = acc.get("positional") or rr["positional"]


def parse_cmdlist(block):
    """AMQ8426I 'Valid ... commands are' lists: indented lines of one or
    two uppercase words (verb lists bare, object lists prefixed by the
    verb). The unindented AMQ8405I segment echo can never match \\s{4,}."""
    if not any("AMQ8426I" in l for l in block):
        return None
    items = []
    for l in block:
        m = re.match(r"^\s{4,}(?:[A-Z][A-Z0-9]*\s+)?([A-Z][A-Z0-9]+)\s*$", l)
        if m:
            items.append(m.group(1))
    return items or None


def merged_model(root):
    """base model + overlay, mirroring server/src/model.rs load semantics."""
    model = json.load(open(f"{root}/model/mqsc-model.json"))
    ov = json.load(open(f"{root}/model/mqsc-overlay.json"))
    cmds = {v: dict(objs) for v, objs in model["commands"].items()}
    for verb, objs in ov.get("commands", {}).items():
        cmds.setdefault(verb, {}).update(objs)
    for key, pat in ov.get("patch", {}).items():
        verb, obj = key.split(" ", 1)
        entry = cmds.get(verb, {}).get(obj)
        if entry is None:
            continue
        entry = json.loads(json.dumps(entry))  # deep copy
        if (rp := pat.get("replace_params")) is not None:
            entry["params"] = list(rp)
        for np in pat.get("add_params", []):
            for i, p in enumerate(entry["params"]):
                if p["name"] == np["name"]:
                    entry["params"][i] = np
                    break
            else:
                entry["params"].append(np)
        for pname, vals in pat.get("extend_enum", {}).items():
            for p in entry["params"]:
                if p["name"] == pname:
                    p["values"] = list(dict.fromkeys(p.get("values", []) + vals))
                    p["kind"] = "enum"
        cmds[verb][obj] = entry
    return cmds


def corpus_census(root):
    """count NAME( occurrences per parameter name across the local corpus."""
    counts = collections.Counter()
    cdir = f"{root}/dmpmqcfg"
    if not os.path.isdir(cdir):
        return counts
    for fn in os.listdir(cdir):
        if not fn.endswith(".mqsc"):
            continue
        with open(f"{cdir}/{fn}", errors="replace") as f:
            for m in re.finditer(r"\b([A-Z][A-Z0-9]{1,19})\(", f.read()):
                counts[m.group(1)] += 1
    return counts


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--qm", default="ORACLE")
    ap.add_argument("--mqm-bin", default="/opt/mqm/bin")
    ap.add_argument("--out", default="/tmp/mq-oracle")
    args = ap.parse_args()
    root = subprocess.run(["git", "rev-parse", "--show-toplevel"],
                          capture_output=True, text=True).stdout.strip()
    os.makedirs(args.out, exist_ok=True)

    # 1. discover verbs the qmgr's parser knows
    raw = run_session(args.qm, [f"ZZNOTAVERB {BOGUS}"], args.mqm_bin)
    open(f"{args.out}/raw-discover.txt", "w").write(raw)
    blocks = split_transcript(raw)
    verbs = []
    for _, (_, block) in blocks.items():
        verbs = parse_cmdlist(block) or verbs
    verbs = [v for v in verbs if v not in EXCLUDE_VERBS]
    if not verbs:
        sys.exit("no verbs discovered — check qmgr is running (strmqm %s)" % args.qm)

    # 2. discover objects per verb
    lines = [f"{v} {NONAME} {BOGUS}" for v in verbs]
    raw = run_session(args.qm, lines, args.mqm_bin)
    open(f"{args.out}/raw-objects.txt", "w").write(raw)
    blocks = split_transcript(raw)
    oracle_objs = {}
    for n in sorted(blocks):
        cmd, block = blocks[n]
        verb = cmd.split()[0]
        objs = parse_cmdlist(block)
        if objs:
            oracle_objs[verb] = set(objs)

    # 3. probe every pair: oracle-discovered ∪ model-known (model-only pairs
    #    return AMQ8426I lists = command-level oracle gaps / z/OS commands)
    cmds = merged_model(root)
    model_pairs = {(v, o) for v, objs in cmds.items() for o in objs}
    oracle_pairs = {(v, o) for v, objs in oracle_objs.items() for o in objs}
    all_pairs = sorted(model_pairs | oracle_pairs)

    def oracle_entry(acc, key):
        acc.setdefault(key, {"params": {}, "flag_pairs": {}, "positional": None})
        return acc[key]

    def probe(lines, raw_name):
        raw = run_session(args.qm, [l for l, _, _ in lines], args.mqm_bin, timeout=1200)
        open(f"{args.out}/{raw_name}", "w").write(raw)
        line_of = {}
        for i, v, o in lines:
            line_of[i.strip().upper()] = (v, o)
        acc, unknown = {}, set()
        for _, (cmd, block) in split_transcript(raw).items():
            pair = line_of.get(cmd.strip().upper())
            if pair is None:
                continue
            key = f"{pair[0]} {pair[1]}"
            if rr := parse_railroad(block, pair[1]):
                absorb(oracle_entry(acc, key), rr)
            elif parse_cmdlist(block):
                unknown.add(key)
            else:
                oracle_entry(acc, key)  # placeholder; other shape may deliver
        return acc, unknown

    # shape 1: bare (commands without a positional name print their full
    # railroad here); shape 2: positional filler.
    lines = []
    for v, o in all_pairs:
        lines.append((f"{v} {o} {BOGUS}", v, o))
        lines.append((f"{v} {o}({POS}) {BOGUS}", v, o))
    acc, unknown = probe(lines, "raw-railroads.txt")

    # shape 3: fill in the required params shape 2 exposed, so the railroad
    # continues past them (runmqsc truncates the print at the failure
    # point). For a required ENUM (CHANNEL's CHLTYPE) the printed syntax is
    # branch-specific, so probe every value and union the results.
    lines3 = []
    for v, o in all_pairs:
        key = f"{v} {o}"
        e = acc.get(key)
        if e is None or e.get("unknown_command") or not e.get("params"):
            continue
        req = [p for p in e["params"].values() if p["required"]]
        if not req:
            continue
        big_enum = next((p for p in req
                         if p["kind"] == "enum" and 2 <= len(p["values"]) <= 16), None)
        req_names = [(n, p) for n, p in e["params"].items() if p["required"]]
        if big_enum is None:
            filler = " ".join(f"{n}({fill_value(p)})" for n, p in req_names)
            lines3.append((f"{v} {o}({POS}) {filler} {BOGUS}", v, o))
        else:
            for val in big_enum["values"]:
                filler = " ".join(
                    f"{n}({val if p is big_enum else fill_value(p)})"
                    for n, p in req_names)
                lines3.append((f"{v} {o}({POS}) {filler} {BOGUS}", v, o))
    if lines3:
        acc3, _ = probe(lines3, "raw-railroads3.txt")
        for key, e3 in acc3.items():
            absorb(oracle_entry(acc, key), e3)
            unknown.discard(key)

    for key in unknown:
        acc.setdefault(key, {"params": {}, "flag_pairs": {},
                             "positional": None, "unknown_command": True})
    for key, e in acc.items():
        if not e.get("params") and not e.get("unknown_command"):
            e["no_railroad"] = True
    oracle = acc

    # 4. diff
    census = corpus_census(root)
    report = {"oracle_verbs": verbs, "pairs": {}}
    summary = collections.Counter()
    for key in sorted(set(oracle) | {f"{v} {o}" for v, o in model_pairs}):
        o_e = oracle.get(key)
        verb, obj = key.split(" ", 1)
        m_e = cmds.get(verb, {}).get(obj)
        entry = {"oracle_only": [], "model_only": [], "enum_diff": {},
                 "kind_diff": {}, "corpus_used": 0, "flags": []}
        if o_e and o_e.get("unknown_command"):
            entry["flags"].append("command_not_in_oracle")
            summary["cmd_not_in_oracle"] += 1
        if m_e is None and o_e and not o_e.get("unknown_command"):
            entry["flags"].append("command_not_in_model")
            summary["cmd_not_in_model"] += 1
        if m_e is None or o_e is None or o_e.get("unknown_command"):
            report["pairs"][key] = entry
            summary["commands"] += 1
            continue
        m_params = {p["name"].upper(): p for p in m_e.get("params", [])}
        o_params = o_e.get("params", {})
        o_flags = o_e.get("flag_pairs", {})
        # a FLAG|NOFLAG pair makes BOTH words oracle-known (model keeps the
        # negatives as separate params: NOTRIGGER, NOSHARE, ...)
        o_flag_words = set(o_flags) | {v for vals in o_flags.values() for v in vals}
        for name in sorted(set(m_params) | set(o_params) | set(o_flag_words)):
            mp, op = m_params.get(name), o_params.get(name)
            in_oracle = op is not None or name in o_flag_words
            if not mp and in_oracle:
                entry["oracle_only"].append(name)
            elif mp and not in_oracle:
                entry["model_only"].append(name)
            elif mp and op:
                mk, ok = mp.get("kind"), op["kind"]
                if mk == "enum" and ok == "enum":
                    mv, ovv = set(mp.get("values", [])), set(op["values"])
                    if mv != ovv:
                        entry["enum_diff"][name] = {"model_missing": sorted(ovv - mv),
                                                    "model_extra": sorted(mv - ovv)}
                elif ok == "enum" and mk in ("string", "integer", "name") :
                    entry["kind_diff"][name] = f"model={mk} oracle=enum({','.join(op['values'])})"
                elif mk == "enum" and ok in ("string", "name", "integer") and name not in ("SELECT", "WHERE"):
                    entry["kind_diff"][name] = f"model=enum oracle={ok}"
        entry["corpus_used"] = sum(census.get(n, 0) for n in entry["model_only"])
        if entry["oracle_only"]: summary["oracle_only_params"] += len(entry["oracle_only"])
        if entry["model_only"]: summary["model_only_params"] += len(entry["model_only"])
        if entry["enum_diff"]: summary["enum_diffs"] += len(entry["enum_diff"])
        if entry["kind_diff"]: summary["kind_diffs"] += len(entry["kind_diff"])
        if "command_not_in_oracle" in entry["flags"]: summary["cmd_not_in_oracle"] += 1
        if "command_not_in_model" in entry["flags"]: summary["cmd_not_in_model"] += 1
        report["pairs"][key] = entry

    report["summary"] = dict(summary)
    json.dump(oracle, open(f"{args.out}/oracle.json", "w"), indent=1)
    json.dump(report, open(f"{args.out}/report.json", "w"), indent=1)

    md = [f"# Oracle differential — {args.qm} via {args.mqm_bin}", "",
          f"summary: {dict(summary)}", ""]
    for key, e in report["pairs"].items():
        if not (e["oracle_only"] or e["model_only"] or e["enum_diff"]
                or e["kind_diff"] or e["flags"]):
            continue
        md.append(f"## {key}" + (f"  (model-only params used {e['corpus_used']}x in corpus)"
                                 if e["corpus_used"] else ""))
        for f_ in e["flags"]: md.append(f"- FLAG {f_}")
        if e["oracle_only"]: md.append(f"- oracle-only params: {', '.join(e['oracle_only'])}")
        if e["model_only"]:
            used = {n: census.get(n, 0) for n in e["model_only"]}
            md.append(f"- model-only params: " + ", ".join(f"{n}({c}x)" for n, c in used.items()))
        for n, d in e["enum_diff"].items():
            md.append(f"- enum {n}: oracle adds {d['model_missing']}, model extra {d['model_extra']}")
        for n, d in e["kind_diff"].items():
            md.append(f"- kind {n}: {d}")
        md.append("")
    open(f"{args.out}/report.md", "w").write("\n".join(md))
    print(f"summary: {dict(summary)}")
    print(f"report: {args.out}/report.md")


if __name__ == "__main__":
    main()

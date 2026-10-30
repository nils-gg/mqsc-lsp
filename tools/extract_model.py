#!/usr/bin/env python3
"""Extract a machine-readable MQSC command model from the Script (MQSC)
Command Reference markdown (PDF print output converted to text).

One command topic looks like:

    2.31.2. DEFINE QLOCAL                      <- numbered topic title
    Use DEFINE QLOCAL to define ...            <- description
    Synonym : DEF QL                           <- short forms (optional)
    Syntax diagram format Railroad diagram Dotted decimal
    ...dotted-decimal diagram as concatenated  <- (converter cuts the first
    '...<newline-escape>' JS string literals     physical line)
    ');break; default:document.write('
    ...railroad diagram (ASCII art literals)...
    ');
    1. footnote text ...                       <- footnotes
    Parent topic: ...

Dotted-decimal notation:
    N! VAL   default branch (MQ supplies this value when parameter omitted)
    N? VAL   optional branch
    N VAL    alternative branch of the same choice point
    N?       marker-only line applying to following lines with same N
    N.k VAL  sub-lines: k=1 enum values, k=2 ')' close, k=0 attribute items
    %name    reference to a subgroup defined later in the same diagram
    (n)      footnote reference

Some topics print only the railroad diagram (the dotted branch came out
empty in the print session): parameters are then recovered from railroad
'NAME--(' occurrences, and DISPLAY/RESET attribute boxes ('+-NAME-...-+')
are intersected with the known parameter universe to rebuild attr lists.

Emits model/mqsc-model.json consumed by the LSP server.
"""

import json
import re
import sys
from collections import OrderedDict
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DOC_PATH = REPO / "MQSC_ScriptCommandReference.md"
OUT_PATH = REPO / "model" / "mqsc-model.json"

CONTENT_START = 700          # topics begin after the table of contents

VERBS = [
    "ALTER", "ARCHIVE", "BACKUP", "CLEAR", "DEFINE", "DELETE", "DISPLAY",
    "MOVE", "PING", "RECOVER", "REFRESH", "RESET", "RESOLVE", "RESUME",
    "RVERIFY", "SET", "START", "STOP", "SUSPEND",
]

TOPIC_RE = re.compile(
    r"(" + "|".join(VERBS) + r")\s+([A-Z][A-Z0-9_/-]*)\s*$"
)
NUMBERED_ANY_RE = re.compile(r"^(\d+(?:\.\d+)+)\.\s+(.+?)\s*$")

DOTTED_DQ_MARKER = "Dotted decimal"
DEFAULT_SWITCH = ");break; default:document.write("


# ---------------------------------------------------------------- literals

def collect_literals(text):
    """Yield the contents of every well-formed '...' literal in *text*.

    An apostrophe inside content is written backslash-apostrophe and does
    not close the literal.
    """
    i, n = 0, len(text)
    while True:
        q = text.find("'", i)
        if q < 0:
            return
        j = q + 1
        while True:
            q2 = text.find("'", j)
            if q2 < 0:
                return
            if text[q2 - 1] == "\\":
                j = q2 + 1
                continue
            break
        yield text[q + 1:q2]
        i = q2 + 1


def decode_literal(content):
    """Backslash-n becomes the diagram newline; backslash-quote an apostrophe."""
    return content.replace("\\n", "\n").replace("\\'", "'")


def diagram_text(section_raw):
    parts = [decode_literal(c) for c in collect_literals(section_raw)]
    return "".join(parts)


def grab_diagram_section(lines, marker_idx):
    """Raw dotted-decimal lines after the marker; returns (raw, switch_line)."""
    chunks = []
    for i in range(marker_idx + 1, len(lines)):
        line = lines[i]
        if DEFAULT_SWITCH in line:
            chunks.append(line.split(DEFAULT_SWITCH)[0])
            return " ".join(chunks), i
        chunks.append(line)
        if i - marker_idx > 4000:
            break
    return " ".join(chunks), len(lines)


def grab_railroad_section(lines, start, end):
    chunks = []
    for i in range(start, min(start + 500, end)):
        line = lines[i]
        for lit in collect_literals(line):
            chunks.append(decode_literal(lit))
        if line.rstrip().endswith("');") and chunks:
            break
    return "".join(chunks)


# ---------------------------------------------------------------- dotted parse

MAIN_RE = re.compile(r"^(\d+)([!?]?)\s*(.*)$")
SUB_RE = re.compile(r"^(\d+)\.(\d+)([+?]?)\s*(.*)$")
ENUM_TOKEN_RE = re.compile(r"^[A-Z][A-Z0-9_/$#%.@-]*$")
VALUE_SPLIT_RE = re.compile(r"^([A-Z][A-Z0-9_/-]*)\s*(?:\((.*)\)|=\s*(.+)|\($)?$", re.S)


def strip_footnotes(text):
    """'SCOPE(QMGR) (10) (3)' -> ('SCOPE(QMGR)', [10, 3])"""
    fns = []
    while True:
        m = re.search(r"\s\((\d{1,2})\)\s*$", text)
        if not m:
            break
        fns.insert(0, int(m.group(1)))
        text = text[: m.start()]
    return text.strip(), fns


def classify(raw_value):
    """Classify the value text inside NAME( value ) -> (kind, values, default)."""
    v = raw_value.strip()
    if v == "":
        return ("flag", [], None)
    m = re.match(r"^\((.*)\)$", v, re.S)
    inner = m.group(1).strip() if m else re.sub(r"^=\s*", "", v)
    if re.match(r"^\(.*\)$", inner):
        inner = inner[1:-1].strip()          # ( (generic-name) )
    if inner == "":
        return ("flag", [], None)
    if inner == "*":
        return ("star", [], None)
    low = inner.lower()
    if low == "integer":
        return ("integer", [], None)
    if low == "string":
        return ("string", [], None)
    if low in ("date", "time", "hh.mm.ss", "hhmm", "yyyy/mm/dd"):
        return ("string", [], None)
    if re.match(r"^\d[\d ]*$", inner):
        return ("intval", [], inner.replace(" ", ""))
    if inner.startswith("'") and inner.endswith("'"):
        return ("strval", [], inner[1:-1].strip())
    if re.match(r"^[a-z]", inner):
        return ("name", [], None)           # qlocal-name, structure-name, ...
    toks = inner.replace("\n", " ").split()
    if toks and all(ENUM_TOKEN_RE.match(t) for t in toks):
        return ("enum", toks, toks[0] if len(toks) == 1 else None)
    return ("string", [], None)


def new_param(name, kind, values, default, optional, has_default, footnotes):
    return {
        "name": name.upper(),
        "kind": kind,
        "values": list(values),
        "default": default,
        "optional": optional,
        "has_default": has_default,
        "footnotes": list(footnotes),
    }


def merge_into(q, p):
    """Merge param p into existing param q (same name, other diagram/branch)."""
    q["values"] = list(dict.fromkeys(q["values"] + p["values"]))
    if q["default"] is None:
        q["default"] = p["default"]
    q["optional"] = q["optional"] and p["optional"]
    q["has_default"] = q["has_default"] or p["has_default"]
    if p["kind"] in ("integer", "string", "enum", "name") and q["kind"] in ("flag", "star"):
        q["kind"] = p["kind"]
    elif {q["kind"], p["kind"]} == {"enum", "integer"}:
        # One diagram shows NAME(ASPARENT) as enum, another NAME(integer):
        # the value is the UNION of both alternatives. The old code kept q's
        # kind and lost the integer branch (DEFINE TOPIC DEFPRTY parsed as
        # enum[ASPARENT], so every DEFPRTY(0) warned). Keep enum kind — so
        # the keywords still validate — and list "integer" as one more
        # permitted value (model.rs turns that into alt_integer). Same
        # convention as the railroad fix; values were unioned above, so the
        # integer-first direction (empty q list) already carries the keywords.
        q["kind"] = "enum"
        if "integer" not in q["values"]:
            q["values"].append("integer")
    q["footnotes"] = list(dict.fromkeys(q["footnotes"] + p["footnotes"]))


def parse_dotted(section_raw, footnotes):
    """Parse one dotted-decimal diagram -> (params, attrs, positional_name)."""
    text = diagram_text("'" + section_raw)   # leading quote restores truncated literal
    lines = text.split("\n")

    main_rows = []
    sub_map = {}
    subgroups = OrderedDict()
    cur = main_rows

    for ln in lines:
        s = ln.strip()
        if not s:
            continue
        m = SUB_RE.match(s)
        if m:
            num, k, mod, rest = int(m.group(1)), int(m.group(2)), m.group(3), m.group(4).strip()
            rest, _ = strip_footnotes(rest)
            if mod == "?" or rest.startswith("?") or rest.startswith("("):
                rest = "?" + rest.lstrip("?")
            sub_map.setdefault((num, k), []).append(rest)
            continue
        m = MAIN_RE.match(s)
        if m:
            num, marker, text2 = int(m.group(1)), m.group(2), m.group(3).strip()
            text2, fns = strip_footnotes(text2)
            cur.append({"num": num, "marker": marker, "text": text2, "fns": fns})
            continue
        if re.match(r"^[A-Z][A-Za-z0-9 ()/-]*$", s) and not s.startswith(("+", "|", ">")):
            key = s.lower()
            subgroups.setdefault(key, [])
            cur = subgroups[key]
            continue

    all_rows = main_rows + [r for rows in subgroups.values() for r in rows]
    by_num = OrderedDict()
    for r in all_rows:
        by_num.setdefault(r["num"], []).append(r)
    marker_by_num = {}
    for r in all_rows:
        if r["marker"]:
            marker_by_num.setdefault(r["num"], set()).add(r["marker"])

    for (num, k), items in sub_map.items():
        rows = by_num.get(num)
        if not rows:
            continue
        anchor = rows[-1]
        clean = [t.rstrip("+").strip() for t in items]
        clean = [t for t in clean if t and t not in ("+", ",") and not t.startswith("?")]
        if k == 1:
            anchor.setdefault("enum1", []).extend(clean)
        elif k == 0:
            anchor.setdefault("list0", []).extend(clean)

    params = OrderedDict()
    positional = None

    def add(row, name, kind, values, default):
        key = name.upper()
        markers = marker_by_num.get(row["num"], set())
        p = new_param(key, kind, values, default,
                      optional=bool(markers) or default is not None,
                      has_default="!" in markers,
                      footnotes=[footnotes.get(f, "") for f in row["fns"] if f in footnotes])
        if key in params:
            merge_into(params[key], p)
        else:
            params[key] = p

    def handle_rows(rows):
        nonlocal positional
        for r in rows:
            text = r["text"]
            if not text:
                continue
            if text.startswith("%") or text.upper() in VERBS:
                continue
            if text.startswith("("):
                if r["num"] <= 2 and positional is None and re.search(r"[a-z]", text):
                    positional = text.strip("() ")
                continue
            m = re.match(r"^([A-Z][A-Z0-9_/-]*)\s*\($", text)
            if m:                              # NAME ( ... enum follows in .1 rows
                add(r, m.group(1), "enum", r.get("enum1", []), None)
                continue
            m = VALUE_SPLIT_RE.match(text)
            if m and m.group(1) and m.group(1)[0].isupper():
                name = m.group(1)
                if name in VERBS:
                    continue
                rawval = m.group(2) if m.group(2) is not None else (m.group(3) or "")
                kind, values, default = classify(rawval)
                if r.get("enum1"):
                    values = list(dict.fromkeys(values + r["enum1"]))
                    kind = "enum"
                add(r, name, kind, values, default)
                continue
            if ENUM_TOKEN_RE.match(text):      # bare keyword: REPLACE, NOEORMDB...
                vals = r.get("enum1", [])
                add(r, text, "enum" if vals else "flag", vals, None)

    handle_rows(main_rows)
    for rows in subgroups.values():
        handle_rows(rows)

    attrs = []
    for r in all_rows:
        if r.get("list0"):
            attrs.extend(r["list0"])

    out = []
    for p in params.values():
        if p["kind"] == "intval":
            p["kind"] = "integer"
        elif p["kind"] == "strval":
            p["kind"] = "string"
        elif p["kind"] == "enum" and not p["values"]:
            p["kind"] = "flag"
        out.append(p)
    return out, sorted(set(attrs)), positional


# ---------------------------------------------------------------- railroad

RAIL_POS_RE = re.compile(r">>-?\w+(?: \w+)*--\(--\s*([a-z][a-z0-9-]*)\s*--\)")
RAIL_BOX_RE = re.compile(r"[+-]-([A-Z][A-Z0-9_]{2,})-{3,}\+?")


def railroad_pos_name(rail):
    m = RAIL_POS_RE.search(rail)
    return m.group(1) if m else None


def railroad_box_tokens(rail):
    return [m.group(1) for m in RAIL_BOX_RE.finditer(rail) if m.group(1) not in VERBS]


RAIL_NAME_DASH = re.compile(r"(?<![A-Z0-9_])([A-Z][A-Z0-9_]{2,})--")
RAIL_CHIP_RE = re.compile(
    r"\(-{1,2}([A-Z][A-Z0-9_]{1,})-{1,2}\)|[+']-([A-Z][A-Z0-9_]{1,})-+['+]"
)


def railroad_params(rail, cmd_obj=""):
    """Recover parameters from railroad art (flat literal stream).

    A name is kept when it is directly followed by an option marker:
    NAME--(--value--)  or  NAME--+-(--v1--)  etc.  Enum chips that merely
    follow a dash run (``+-TCP-+``, ``'-PUBSUB--'``, ``(--ADVANCELOG--)``)
    are values, not parameters, and are collected as values instead.
    """
    chips = []
    for cm in RAIL_CHIP_RE.finditer(rail):
        tok = cm.group(1) or cm.group(2)
        if tok and tok not in chips:
            chips.append(tok)
    chip_set = set(chips)
    box_set = set(railroad_box_tokens(rail))

    params = OrderedDict()
    for m in RAIL_NAME_DASH.finditer(rail):
        name = m.group(1)
        if name in VERBS or name == cmd_obj or name in params:
            continue
        after = rail[m.end(): m.end() + 24].lstrip("-+| \n")
        strong = after.startswith("(")          # NAME--(-- value --)
        if not strong:
            continue                             # chips/boxes are not parameters
        region = rail[m.end(): m.end() + 300]
        # cut at the next *parameter* name (skip chip-shaped occurrences)
        nxt_pos = None
        for nm in RAIL_NAME_DASH.finditer(region):
            if nm.group(1) == name:
                continue
            a2 = region[nm.end(): nm.end() + 24].lstrip("-+| \n")
            if a2.startswith("(") and nm.group(1) not in chip_set:
                nxt_pos = nm.start()
                break
        if nxt_pos is not None:
            region = region[:nxt_pos]
        # Collect this region's keyword chips FIRST, then decide the kind. The
        # old order tested \binteger\b first and, when it matched, returned a
        # bare `integer` kind and silently dropped the chips — so a parameter
        # the reference shows as `( integer | CICS | MVS | ... )` came out
        # `kind=integer, values=[]` and every keyword value then warned. Keep
        # integer/string precedence, but when integer is mixed with keyword
        # alternatives emit an enum that lists "integer" as one more value
        # (model.rs maps that to alt_integer, accepting plain integers too).
        vals = []
        for cm in RAIL_CHIP_RE.finditer(region):
            tok = cm.group(1) or cm.group(2)
            if tok and tok != name and tok not in vals:
                vals.append(tok)
        vals = [v for v in vals if v not in ("integer", "string")]
        has_int = bool(re.search(r"\binteger\b", region))
        has_str = bool(re.search(r"\bstring\b", region) or re.search(r"'[^']*'", region))
        if has_int and vals:
            kind, values, default = "enum", ["integer"] + vals, None
        elif has_int:
            kind, values, default = "integer", [], None
        elif has_str:
            kind, values, default = "string", [], None
        elif vals:
            kind, values, default = "enum", vals, None
        else:
            kind, values, default = "flag", [], None
        params[name] = new_param(name, kind, values, default, True, False, [])
    return params


# ---------------------------------------------------------------- topics

# Words that make up the 2011 pages' platform-support table rows
# ("i5/OS® UNIX systems Windows z/OS®") — a line made only of these is a
# table row, never footnote prose.
_PLAT_WORDS = {"AIX", "HP", "HP-UX", "OpenVMS", "Linux", "i5/OS", "Solaris",
               "Windows", "z/OS", "UNIX", "systems", "WebSphere", "MQ",
               "CICS", "IMS", "CR", "and"}


def _cellish(t):
    """A platform-table cell: a lone ASCII letter (the 'n m l k j' column
    dumps) or a punctuation/quote artifact run.
    """
    return (len(t) == 1 and t.isascii() and t.isalpha()) or (
        t and all(ch in "‘’‛`'.," for ch in t))


def _is_table_row(s):
    """A platform-support table row — 'i5/OS® UNIX systems Windows z/OS®',
    'n m l k j n m l k j', stray quote glyphs. Never prose: the reference's
    real platform lists are comma-separated ('Linux, Solaris, and Windows'),
    the tables are bare space-separated columns.
    """
    if "," in s:
        return False
    toks = [t.strip("®™") for t in re.split(r"\s+", s) if t]
    if len(toks) < 2:
        return False
    return all(_cellish(t) or t in _PLAT_WORDS for t in toks)


def _is_fn_stop(s):
    """A line that ends footnote collection: new note, section title, real
    heading, page junk, or a platform/column table row.
    """
    if not s:
        return False
    if re.match(r"^\d{1,2}\.\s", s):
        return True
    if s.startswith(("Parent topic", "Related", "**", "Syntax diagram", "Page ",
                     "This topic", "This build", "Notices", "Copyright", "sc1",
                     "Parameter descriptions", "Keyword and parameter",
                     "Usage notes", "Synonym", "---", "<!--", "http")):
        return True
    if re.match(r"^\d\d/\d\d/\d{4}", s):
        return True
    if s.startswith("##"):
        # real headings are "## SOMEWORD"; table dumps are "## X X X CR"
        # or "## 12C" — no >=3-letter word right after the marker
        return not re.match(r"^##\s*[A-Z][A-Z0-9_/-]{2,}", s)
    return _is_table_row(s)


def extract_footnotes(lines, lo, hi):
    notes = {}
    i = lo
    while i < hi:
        m = re.match(r"^(\d{1,2})\.\s+(\S.*)$", lines[i])
        if m:
            n = int(m.group(1))
            text = m.group(2)
            j = i + 1
            while j < hi:
                s = lines[j].strip()
                if _is_fn_stop(s):
                    break
                if not s:
                    # The PDF conversion splits a wrapped footnote across a
                    # blank line ("...where it is 999 999 999), but your" /
                    # "" / "installation might have changed it."). Join, but
                    # ONLY across a blanks-only gap and only while the text
                    # so far ends mid-sentence: the pages' platform tables
                    # sit past the same gap (and are stops), so a table can
                    # never be swallowed into a footnote.
                    k = j
                    while k < hi and not lines[k].strip():
                        k += 1
                    if k >= hi:
                        break
                    t = lines[k].strip()
                    if (text.rstrip().endswith((".", ":", ";", "!", "?"))
                            or _is_fn_stop(t)):
                        break
                    text += " " + t
                    j = k + 1
                    continue
                text += " " + s
                j += 1
            notes[n] = re.sub(r"\s+", " ", text).strip()
            i = j
            continue
        i += 1
    return notes


def extract_topic_diagrams(lines, lo, hi, notes, obj=""):
    """Merge every dotted+railroad diagram pair inside the topic window."""
    params = OrderedDict()
    attrs = set()
    positional = None
    box_tokens = []
    has_rail = False

    i = lo
    while i < hi:
        ln = lines[i]
        if DOTTED_DQ_MARKER in ln and ln.startswith("Syntax diagram"):
            raw, end = grab_diagram_section(lines, i)
            if raw:
                try:
                    ps, as_, pos = parse_dotted(raw, notes)
                except Exception as e:
                    print(f"WARN dotted parse at line {i}: {e}", file=sys.stderr)
                    ps, as_, pos = [], [], None
                for p in ps:
                    if p["name"] in params:
                        merge_into(params[p["name"]], p)
                    else:
                        params[p["name"]] = p
                attrs.update(as_)
                if pos and positional is None:
                    positional = pos
            rail = grab_railroad_section(lines, end, min(hi, end + 500))
            if rail.strip():
                has_rail = True
                if positional is None:
                    positional = railroad_pos_name(rail)
                box_tokens.extend(railroad_box_tokens(rail))
                for name, rp in railroad_params(rail, obj).items():
                    if name not in params:
                        rp["recovered"] = True
                        params[name] = rp
            i = end + 1
            continue
        i += 1

    return list(params.values()), sorted(attrs), positional, has_rail, box_tokens


# Cap for a parameter's rendered hover description. The 2011 pages'
# "Parameter descriptions" sections bleed into each other (table dumps run
# to 6.5k chars), so some cap is needed — but a hard [:500] cut produced
# hover cards ending mid-word ("…does not limit the lis"). The cap must
# therefore land after real sentence punctuation.
DOC_LIMIT = 1000


def clamp_doc(d):
    if len(d) <= DOC_LIMIT:
        return d
    cut = -1
    for pat in (". ", "? ", "! ", ": "):
        i = d.rfind(pat, 0, DOC_LIMIT)
        if i != -1 and i + 1 > cut:
            cut = i + 1
    return d[:cut] if cut > 200 else d[:DOC_LIMIT]


def _is_cell_heading(ln, lines, j, end):
    """True when a '## WORD' line inside a parameter-descriptions region is
    really a platform-table cell: the marker text itself is table junk
    ('## X X X CR'), or the very next line is another table row ('## CR' /
    'n m l k j n m l k j'). Real headings — including the genuine two-letter
    filter operators '## GE'/'## LK'/'## CT' — are followed by prose.
    """
    if _is_table_row(ln.strip()[2:].strip()):
        return True
    k = j + 1
    while k < end and not lines[k].strip():
        k += 1
    return k < end and _is_table_row(lines[k].strip())


def main():
    if not DOC_PATH.exists():
        sys.exit(
            f"error: {DOC_PATH.name} not found at {REPO}\n"
            "  The extractor input is a markdown conversion of the IBM\n"
            "  'Script (MQSC) Command Reference' (© IBM, not redistributed in this\n"
            "  repo; see ATTRIBUTION.md). Obtain your own copy, save it at that\n"
            "  path, then rerun. The committed model/mqsc-model.json already\n"
            "  contains the extraction result — regenerating it is optional."
        )
    text = DOC_PATH.read_text(encoding="utf-8")
    lines = text.splitlines()

    topics = []
    for i in range(CONTENT_START, len(lines)):
        ln = lines[i]
        if ln.startswith(("#", "*", "+", ">")):
            continue
        m = NUMBERED_ANY_RE.match(ln)
        if m:
            title = m.group(2)
            if title.startswith(("Notices", "Copyright", "This build")):
                continue
            topics.append((i, m.group(1), title))

    def is_boundary(k):
        _, num, title = topics[k]
        if title.startswith(("Parameter descriptions", "Keyword and parameter")):
            return True
        if TOPIC_RE.match(title):               # another command topic
            return True
        return "." not in num                   # top-level topic

    def bounds(k):
        lo = topics[k][0]
        for j in range(k + 1, len(topics)):
            if is_boundary(j):
                return lo, topics[j][0]
        return lo, len(lines)

    commands = OrderedDict()
    param_docs = {}
    HEADING_DOC_RE = re.compile(r"^## ([A-Z][A-Z0-9_/-]+)\b[ XCRPN2D0-9/—-]*$")
    SYNTAX_ECHO_RE = re.compile(r"^[A-Z][A-Z0-9_/-]*\( ?(?:string|integer|value|[a-z-]+) ?\)$")

    for k, (idx, num, title) in enumerate(topics):
        lo, hi = bounds(k) if is_boundary(k) else (idx, idx)

        if title.startswith("Parameter descriptions") or title.startswith("Keyword and parameter"):
            context = title
            cur, buf = None, []
            end = min(hi if hi > lo else len(lines), lo + 8000)
            for j in range(lo + 1, end):
                ln = lines[j]
                m = HEADING_DOC_RE.match(ln)
                if m and _is_cell_heading(ln, lines, j, end):
                    # the platform tables print their cell column as fake
                    # headings ('## CR', '## X X X CR') that would otherwise
                    # steal the following page's continuation prose; skip the
                    # line and keep the current parameter's doc open
                    continue
                if m:
                    if cur:
                        param_docs.setdefault(cur, []).append([context, " ".join(buf)])
                    cur, buf = m.group(1), []
                    continue
                # NOTE: page footers ("Parent topic:", "Page N of M", ...) are
                # *not* region ends in this conversion — content continues
                # after them; they are dropped by the junk filter below.
                s = ln.strip()
                if s and cur is not None and not s.startswith(("*", "|", "+", "#", ">", "[")):
                    if re.match(r"^\d{1,2}(?:\.\s|(?:\.\d+)+[.)]?\s)", s) or SYNTAX_ECHO_RE.match(s):
                        continue
                    if re.match(r"^(Page \d|\d\d/\d\d/\d{4}|http|<!--|---|This topic|sc1\d|"
                                r"This build|Notices|Copyright|Related|Parent)", s):
                        continue
                    if _is_table_row(s):
                        continue
                    buf.append(s)
            if cur:
                param_docs.setdefault(cur, []).append([context, " ".join(buf)])
            continue

        m = TOPIC_RE.match(title)
        if not m:
            continue
        verb, obj = m.group(1), m.group(2)

        desc = ""
        for j in range(lo + 1, min(lo + 30, hi)):
            s = lines[j].strip()
            if s and not s.startswith(("#", "*", "|", "+", ">", "[")) and "document" not in s:
                desc = s
                j2 = j + 1
                while j2 < min(lo + 60, hi):
                    t = lines[j2].strip()
                    if not t or t.startswith(("Synonym", "Note", "Parent", "##", "*", "|", "+", ">", "Syntax")):
                        break
                    desc += " " + t
                    j2 += 1
                break
        desc = re.sub(r"\s+", " ", desc)

        syn = []
        for j in range(lo, min(lo + 40, hi)):
            mm = re.match(r"^\*{0,2}\s*Synonym\s*:\s*\*{0,2}\s*([A-Z0-9_ /]+)", lines[j])
            if mm:
                syn = mm.group(1).strip().split()
                break

        notes = extract_footnotes(lines, lo, min(hi, lo + 2500))

        params, attrs, pos, has_rail, box_tokens = extract_topic_diagrams(lines, lo, hi, notes, obj)
        if pos is None:
            pos = "name" if (not has_rail and verb in ("DEFINE", "ALTER", "DELETE")) else None

        entry = {
            "verb": verb,
            "object": obj,
            "title": title,
            "description": desc,
            "synonym": syn,
            "params": params,
            "attrs": attrs,
            "positional": pos,
            "footnotes": [notes[n] for n in sorted(notes)][:24],
            "_box_tokens": box_tokens,
        }
        objs = commands.setdefault(verb, OrderedDict())
        if obj in objs:
            # silent overwrite would hide a converter artifact (two topics
            # for one VERB+OBJ); make the model drift loud (REVIEW.md).
            print(f"WARN: duplicate {verb} {obj} topic; later wins",
                  file=sys.stderr)
        objs[obj] = entry

    # ---- attribute lists for DISPLAY/RESET built from railroad boxes:
    # keep box tokens that are real parameter names of this object elsewhere
    universe = set(param_docs)
    for verb, objs in commands.items():
        for obj, e in objs.items():
            universe.update(p["name"] for p in e["params"])
            universe.update(e["attrs"])

    for verb in ("DISPLAY", "RESET"):
        for obj, e in commands.get(verb, {}).items():
            skip = {p["name"] for p in e["params"]}
            related = set()
            for v2 in ("DEFINE", "ALTER"):
                if obj in commands.get(v2, {}):
                    related.update(p["name"] for p in commands[v2][obj]["params"])
            related.update(e["attrs"])
            for t in e["_box_tokens"]:
                if t in universe and t in related and t not in skip:
                    e["attrs"].append(t)
            e["attrs"] = sorted(set(e["attrs"]))

    for objs in commands.values():
        for e in objs.values():
            e.pop("_box_tokens", None)

    # ---- documentation text
    def param_doc(pname, obj):
        for cand in (pname, re.sub(r"^NOT", "", pname)):
            docs = param_docs.get(cand, [])
            for ctx, d in docs:
                if d and obj in ctx:
                    return clamp_doc(d)
            for ctx, d in docs:
                if d:
                    return clamp_doc(d)
        return ""

    n_empty = 0
    for verb, objs in commands.items():
        for obj, entry in objs.items():
            for p in entry["params"]:
                p["description"] = param_doc(p["name"], obj)
                if not p["description"]:
                    n_empty += 1

    model = {"commands": commands, "param_docs": param_docs}
    OUT_PATH.parent.mkdir(exist_ok=True)
    OUT_PATH.write_text(json.dumps(model, indent=1, ensure_ascii=False), encoding="utf-8")

    total = sum(len(v) for v in commands.values())
    print(f"commands: {total}", file=sys.stderr)
    for verb in sorted(commands):
        print(f"  {verb} ({len(commands[verb])})", file=sys.stderr)
    npar = sum(len(e["params"]) for v in commands.values() for e in v.values())
    print(f"params: {npar} (undocumented: {n_empty}), docs: {len(param_docs)}", file=sys.stderr)


if __name__ == "__main__":
    main()

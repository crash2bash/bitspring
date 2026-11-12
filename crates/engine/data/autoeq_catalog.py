#!/usr/bin/env python3
"""Pack AutoEQ's whole catalog into the one file the app downloads (apps/website/public/autoeq/catalog.json).

Usage: autoeq_catalog.py <AutoEq checkout>/results <commit> <out.json>

A checkout with just the parametric files is enough:
  git clone --depth 1 --filter=blob:none --no-checkout https://github.com/jaakkopasanen/AutoEq.git
  git sparse-checkout init --no-cone
  printf '/results/INDEX.md\\n/results/**/*ParametricEQ.txt\\n' > .git/info/sparse-checkout
  git checkout

Each profile's "eq" is "<preamp>;<filter>;<filter>…" with a filter as <kind><Fc>,<gain>,<Q>
(kind L = low shelf, P = peak, H = high shelf); hifi_engine::dsp::eq_io reads it back.
"""
import json, re, sys, urllib.parse
from pathlib import Path

results, commit, out = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
KINDS = {"LSC": "L", "PK": "P", "HSC": "H"}
PREAMP = re.compile(r"^Preamp: (-?[0-9.]+) dB$")
FILTER = re.compile(r"^Filter \d+: ON (LSC|PK|HSC) Fc ([0-9.]+) Hz Gain (-?[0-9.]+) dB Q ([0-9.]+)$")
ENTRY = re.compile(r"^- \[(.+)\]\(\./(.+)\) by (.+?)(?: on (.+))?$")


def num(s):
    f = float(s)
    return str(int(f)) if f == int(f) else s.rstrip("0")


profiles, skipped = [], []
for line in (results / "INDEX.md").read_text(encoding="utf-8").splitlines():
    m = ENTRY.match(line.strip())
    if not m:
        continue
    name, path, source, rig = m.groups()
    folder = Path(urllib.parse.unquote(path))
    if ".." in folder.parts or folder.is_absolute():
        skipped.append(name)
        continue
    f = results / folder / f"{folder.name} ParametricEQ.txt"
    if not f.is_file():
        skipped.append(name)
        continue
    preamp, filters = None, []
    for l in f.read_text(encoding="utf-8").splitlines():
        l = l.strip()
        if not l:
            continue
        if p := PREAMP.match(l):
            preamp = num(p.group(1))
        elif q := FILTER.match(l):
            kind, fc, gain, qv = q.groups()
            filters.append(f"{KINDS[kind]}{num(fc)},{num(gain)},{num(qv)}")
        else:
            filters = []  # a line we don't know: leave the profile out rather than guess
            break
    if preamp is None or not filters:
        skipped.append(name)
        continue
    entry = {"name": name, "source": source, "eq": ";".join([preamp] + filters)}
    if rig:
        entry["rig"] = rig
    profiles.append(entry)

profiles.sort(key=lambda p: (p["name"].casefold(), p["source"].casefold(), p.get("rig", "")))
data = {
    "version": 1,
    "about": "Parametric EQ profiles from AutoEQ (github.com/jaakkopasanen/AutoEq, MIT licence, by Jaakko Pasanen) "
    "for every headphone it has results for, measured by crinacle, oratory1990, Rtings, Innerfidelity and others.",
    "commit": commit,
    "profiles": profiles,
}
out.parent.mkdir(parents=True, exist_ok=True)
out.write_text(json.dumps(data, ensure_ascii=False, separators=(",", ":")), encoding="utf-8")
print(len(profiles), "profiles;", len(skipped), "skipped", skipped[:10])

#!/usr/bin/env python3
"""Resolve a list of popular headphones against the AutoEQ index, preferring the best
measurement source; print name|source|path for each, and what didn't resolve."""
import re, sys, json, urllib.parse

WANT = """
Sennheiser HD 600
Sennheiser HD 650
Sennheiser HD 6XX
Sennheiser HD 660 S
Sennheiser HD 660S2
Sennheiser HD 800
Sennheiser HD 800 S
Sennheiser HD 560S
Sennheiser HD 58X
Sennheiser HD 599
Sennheiser HD 490 PRO
Sennheiser Momentum 4 Wireless
Sennheiser IE 900
Sennheiser IE 600
Sennheiser IE 300
Sennheiser IE 200
Beyerdynamic DT 770 PRO 80 Ohm
Beyerdynamic DT 880
Beyerdynamic DT 990 PRO
Beyerdynamic DT 1990 PRO
Beyerdynamic DT 1770 PRO
Beyerdynamic DT 900 PRO X
Beyerdynamic DT 700 PRO X
Beyerdynamic T1
Audeze LCD-2
Audeze LCD-2 (Fazor)
Audeze LCD-2 Classic
Audeze LCD-X
Audeze LCD-3
Audeze LCD-4
Audeze LCD-5
Audeze Maxwell
Audeze MM-500
Audeze Sine
HIFIMAN Sundara
HIFIMAN Arya
HIFIMAN Arya Stealth
HIFIMAN Ananda
HIFIMAN Edition XS
HIFIMAN HE400se
HIFIMAN HE6se
HIFIMAN Susvara
HIFIMAN HE1000se
HIFIMAN Deva
Focal Clear
Focal Clear MG
Focal Utopia
Focal Elex
Focal Elegia
Focal Stellia
Focal Bathys
Focal Celestee
AKG K371
AKG K702
AKG K712 PRO
AKG K612 PRO
AKG K240 Studio
AKG K361
Audio-Technica ATH-M50x
Audio-Technica ATH-M40x
Audio-Technica ATH-R70x
Audio-Technica ATH-ADX5000
Sony WH-1000XM4
Sony WH-1000XM5
Sony MDR-7506
Sony MDR-Z1R
Sony MDR-MV1
Sony WF-1000XM4
Sony WF-1000XM5
Apple AirPods Pro 2
Apple AirPods Pro
Apple AirPods Max
Apple AirPods (3rd generation)
Bose QuietComfort 45
Bose QuietComfort Ultra Headphones
Bose QuietComfort Earbuds II
Philips SHP9500
Philips Fidelio X2HR
Grado SR80e
Grado SR325x
Meze 99 Classics
Meze Elite
Meze Empyrean
Meze Liric
Meze 109 PRO
Dan Clark Audio Aeon 2 Noire
Dan Clark Audio Ether 2
Dan Clark Audio Stealth
Dan Clark Audio Expanse
Shure SE215
Shure SE535
Shure SE846
Etymotic ER2XR
Etymotic ER4XR
Etymotic ER4SR
Moondrop Blessing 2
Moondrop Blessing 3
Moondrop Aria
Moondrop Chu
Moondrop Variations
Moondrop Starfield
Moondrop Kato
Truthear HEXA
7Hz Timeless
7Hz Salnotes Zero
Campfire Audio Andromeda
Samsung Galaxy Buds2 Pro
Koss KSC75
Koss Porta Pro
FiiO FT3
Fostex TH900
Fostex T50RP MK3
Stax SR-L700
Stax SR-009
Sennheiser HD 25
Sennheiser HD 280 PRO
Audio-Technica ATH-M20x
Samson SR850
Superlux HD681
"""

PREFER = ["oratory1990", "crinacle", "Rtings", "Innerfidelity", "Headphone.com Legacy", "HypetheSonics", "Super Review", "Kuulokenurkka"]

def norm(s):
    return re.sub(r"[^a-z0-9]", "", s.lower().replace("hifiman", "hifiman"))

entries = []
for line in open(sys.argv[1], encoding="utf-8"):
    m = re.match(r"- \[(.+?)\]\((\./.+?)\) by (.+?)(?: on (.+))?$", line.strip())
    if m:
        entries.append({"name": m.group(1), "path": urllib.parse.unquote(m.group(2)[2:]), "source": m.group(3), "rig": m.group(4)})

out, missing = [], []
for want in [w.strip() for w in WANT.splitlines() if w.strip()]:
    cands = [e for e in entries if norm(e["name"]) == norm(want)]
    if not cands:
        # A name with a suffix the index adds ("(2020)" etc.): the shortest that starts with it.
        cands = [e for e in entries if norm(e["name"]).startswith(norm(want)) and "(" not in e["name"][len(want):][:1] + "x"]
        cands = sorted(cands, key=lambda e: len(e["name"]))[:3]
    if not cands:
        missing.append(want)
        continue
    rank = lambda e: (PREFER.index(e["source"]) if e["source"] in PREFER else 99, "in-ear" in e["path"] and "over-ear" in want.lower())
    best = sorted(cands, key=rank)[0]
    out.append(best)

json.dump(out, open(sys.argv[2], "w"), ensure_ascii=False, indent=1)
print(len(out), "resolved;", len(missing), "missing:", missing)

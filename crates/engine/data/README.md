# Headphone profiles

Two sets of AutoEQ parametric EQ profiles (github.com/jaakkopasanen/AutoEq, MIT licence, by
Jaakko Pasanen):

- `headphones.json`, built into the engine: a hand-picked list of popular audiophile headphones
  and IEMs, one measurement each (oratory1990's first, then crinacle's, Rtings', Innerfidelity's),
  so the most common models work offline. To change the list, edit `autoeq_pick.py`, run it on
  AutoEq's `results/INDEX.md`, and store each pick's `ParametricEQ.txt` as `{"name", "source", "eq"}`.
- The whole catalog (every model, every measurement: about 8,850 profiles), which the app
  downloads on demand from the website (`apps/website/public/autoeq/catalog.json`, served at
  `hifi_engine::dsp::eq_io::AUTOEQ_CATALOG`) and keeps for a month. Rebuild it with
  `autoeq_catalog.py` (its docstring has the steps), then deploy the website. The format has a
  `version`; bump it, and teach `parse_catalog` the new one, only for a change older apps
  can't read.

# The assistant's test set

`aptimizer-studio eval` measures the AI assistant the way the floor-plan test set in Aptimizer measures the planner.
It asks a small set of requests per mode (`eval/cases.json`) of a real Aptimizer server and its model, exactly as the
panel asks them: the same command catalog, the same summary of the drawing or model, and the same automatic **Fix it**
follow-ups. It runs the commands in a fresh copy of AptCAD or AptBIM, through the same command path as the window
(`src/host.rs`), and checks the result: what the drawing or model holds afterwards, not the words of the reply.

A case passes when all of the following hold:
- every command it was given ran;
- none were left out for not being in the catalog;
- its checks hold.

Each case is scored twice: on the model's first answer, and again after the fixes that "Run as they come" asks for
on its own (at most two).

## Running it

1. Run the Aptimizer server with a model configured (`GEMINI_API_KEY`, or another provider `ai.py` reads).
2. Make an access key in Aptimizer: Profile > Aptimizer Studio > Make a key.
3. Then:

```bash
APTIMIZER_URL=https://your-aptimizer-server APTIMIZER_KEY=apts_... aptimizer-studio eval
```

It prints a line per case and writes `eval/results/report.md` (the table) and `eval/results/results.json`, which
holds every request's answers. Other options:
- `--only bim-` runs only the cases whose name contains `bim-`.
- `--out DIR` writes the results to another folder.
- `--replay eval/results/results.json` scores the kept answers again without the model, for example after a change
  to an app.
- `--reference` answers every case with its own reference answer. Use it to check the test set itself; the test
  `every_case_has_a_reference_answer_that_passes_it` does the same in `cargo test`.

Each answer is one request against the account's AI allowance: a full run is about 17 to 51 requests.

## The cases

Each case has the following fields:
- `name` and `mode` (`cad` or `bim`).
- `request`: the user's words.
- `setup`: commands run first, so the request has something to work on.
- `checks`.
- `reference`: commands a right answer could give. `$state:PATH` in a reference answer is the state's value at
  that path, such as the ids of the walls.

A check reads the state the assistant was sent: AptCAD's `drawing.inspect`, or AptBIM's `inspect` in brief (see
`bim_brief`). It reads that state before the request and after it, or the answer of a query named with `in`:

| check | holds when |
|---|---|
| `{"grows": PATH, "by": n}` | the number at PATH grew by exactly n (`at_least` for n or more) |
| `{"equals": PATH, "value": v}` | the value at PATH is v afterwards (a missing count is 0) |
| `{"has": PATH, "value": v}` | some value at PATH is v (text compared without case) |
| `{"near": PATH, "value": x, "tol": t}` | the number at PATH is within t of x |
| `{"no_commands": true}` | the model gave no commands (a question) |
| `{"any_of": [checks]}` | one of them holds |

A path is dotted: `counts.Circle`, `layers.*.name` (`*` takes every member), `extents.1.0` (the second item's
first).

The set has 9 drafting and 8 modelling cases. They cover drawing, layers, text and dimensions, and selecting before
a change (erase or move by type). On the modelling side: walls, levels, doors and windows in a wall, a named room, a
floor, deleting by id. Each mode also has one plain question, which must be answered without commands.

## Tuning the prompt

The prompt lives in Aptimizer's `backend/studio.py` (`SYSTEM`, `MODE_RULES`). Each change bumps `PROMPT_VERSION`,
which the server returns with every answer. To tune it:
1. Run the set.
2. Read the failures in `report.md`, and each failed command's error in `results.json`.
3. Change the prompt, or the state Studio sends (`src/host.rs`).
4. Run the set again.
5. Record the result below.

What a request can see matters as much as the prompt's words. A request can only name the objects the state lists,
so the state holds:
- AptCAD: the first 25 objects with their handles.
- AptBIM: the first 30 ids of each category, in a brief that fits within what the server reads of it (AptBIM's full
  `inspect` runs past it).

| date | prompt | model | first answer | after fixes | notes |
|---|---|---|---|---|---|
| 2026-11-05 | 2 | reference answers | 17 / 17 | 17 / 17 | the set itself: every case can pass |
| 2026-11-05 | 2 | stand-in (fixed answers) | 1 / 9 cad | 1 / 9 cad | the live path end to end over HTTP; no real model run yet |

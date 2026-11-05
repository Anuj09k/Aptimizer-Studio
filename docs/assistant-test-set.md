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
| 2026-11-05 | 2 | stand-in (fixed answers) | 1 / 9 cad | 1 / 9 cad | the live path end to end over HTTP |
| 2026-11-05 | 2 | gemini-4-argon | 14 / 17 | 14 / 17 | 2 rate-limited (three models run at once); stopped to look up a wall's length for the windows |
| 2026-11-05 | 2 | gemini-3.5-flash | 14 / 17 | 14 / 17 | 1 rate-limited; same windows look-up; arrayed a just-drawn circle, which isn't selected (copied nothing) |
| 2026-11-05 | 2 | gemini-3.8-flash | 13 / 17 | 13 / 17 | 3 timed out on rate limits; same windows look-up |
| 2026-11-05 | 3 | gemini-4-argon | 16 / 17 | 17 / 17 | prompt 3 and the AptBIM state with where elements are; the miss: a closed wall loop that repeats its first point, which AptBIM refused, fixed by Fix it |
| 2026-11-05 | 3 | gemini-3.5-flash | 16 / 17 | 17 / 17 | the same closed-loop miss, fixed by Fix it |
| 2026-11-05 | 3 | gemini-3.8-flash | 16 / 17 | 17 / 17 | the same closed-loop miss, fixed by Fix it (four separate walls) |
| 2026-11-05 | 3 | gemini-3.5-flash | 17 / 17 | 17 / 17 | with AptBIM closing a loop once (2b2db2d) |
| 2026-11-05 | 3 | gemini-3.8-flash | 17 / 17 | 17 / 17 | with AptBIM closing a loop once |
| 2026-11-05 | 3 | gemini-4-argon | 17 / 17 | 17 / 17 | with AptBIM closing a loop once; its 8 modelling cases run again after Google reported Argon unavailable on two of them (the server's fallback, Flash-Lite, gave an unreadable answer on one) |

What the runs taught:
- A model asks, or looks things up, when the state lacks what it needs. Every model stopped to read a wall's length
  until the AptBIM state said where each wall runs.
- When every model writes something the same way (a closed loop back to its first point), change the app, not the
  prompt. AptBIM now closes such a loop once.
- Fix it pays: each first-answer miss in round 3 was corrected by the one automatic fix.
- On this set the three models score the same; Argon was the one Google reported busy, and its fallback chain ends in
  Flash-Lite models whose answers are weaker. Flash is the better default for the assistant until the set grows hard
  enough to tell them apart.
- Run one model at a time. Three at once ran into Google's per-minute limits (429 and 503 answers), which the report
  shows as "no answer".

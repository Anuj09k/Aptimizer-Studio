//! The assistant's test set: `aptimizer-studio eval`.
//!
//! A small set of requests per mode (`eval/cases.json`), each asked of the real server and its model as the panel
//! asks them (the same catalog, state, history and fixes), its commands run in a fresh copy of AptCAD or AptBIM
//! through the same command path as the window (`host`), and the result checked: the counts, layers, levels or
//! elements the request should have made. A case passes when its commands all ran, none were left out, and its
//! checks hold; it is scored on the first answer and again after the fixes "Run as they come" asks for by itself.
//!
//! Every answer is kept in `results.json`, so a run can be scored again without the model (`--replay`). Every case
//! carries a reference answer, which a test runs through the same checks, so a failure is the model's, not the
//! case's.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::assistant::{self, AUTO_FIXES, Assistant, Target};
use crate::host::{self, Host};

const USAGE: &str =
    "USAGE: aptimizer-studio eval [--cases FILE] [--out DIR] [--only TEXT] [--replay RESULTS]

Asks the Aptimizer server's assistant every request of the test set and checks what its commands do.
  --cases FILE      the test set (default: eval/cases.json)
  --out DIR         where results.json and report.md go (default: eval/results)
  --only TEXT       only the cases whose name contains TEXT
  --replay RESULTS  score the answers kept in an earlier results.json instead of asking the server
  --reference       answer every case with its reference answer (checks the test set itself)

The server and key come from APTIMIZER_URL and APTIMIZER_KEY (an access key from Aptimizer's
Profile > Aptimizer Studio).";

/// The cases the repository holds, for the tests.
#[cfg(test)]
const CASES: &str = include_str!("../eval/cases.json");

/// One request of the test set.
pub struct Case {
    pub name: String,
    pub target: Target,
    pub request: String,
    /// Commands run before the request, so it has something to work on.
    pub setup: Vec<(String, Value)>,
    pub checks: Vec<Value>,
    /// One of the harder requests: several steps, or work on what is already drawn.
    pub hard: bool,
    /// Commands a right answer could give.
    pub reference: Vec<(String, Value)>,
}

fn commands(v: &Value) -> Vec<(String, Value)> {
    v.as_array()
        .map(|cs| {
            cs.iter()
                .filter_map(|c| {
                    let id = c["id"].as_str()?.to_string();
                    Some((id, c.get("params").cloned().unwrap_or_else(|| json!({}))))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn read_cases(text: &str) -> Result<Vec<Case>, String> {
    let all: Value =
        serde_json::from_str(text).map_err(|e| format!("the test set isn't JSON: {e}"))?;
    let mut out = Vec::new();
    for c in all["cases"]
        .as_array()
        .ok_or("the test set has no `cases` list")?
    {
        let name = c["name"]
            .as_str()
            .ok_or("a case has no `name`")?
            .to_string();
        let target = match c["mode"].as_str() {
            Some("cad") => Target::Cad,
            Some("bim") => Target::Bim,
            _ => return Err(format!("{name}: `mode` must be cad or bim")),
        };
        let request = c["request"]
            .as_str()
            .ok_or(format!("{name}: no `request`"))?
            .to_string();
        let checks = c["checks"].as_array().cloned().unwrap_or_default();
        if checks.is_empty() {
            return Err(format!("{name}: no `checks`"));
        }
        out.push(Case {
            name,
            target,
            request,
            setup: commands(&c["setup"]),
            checks,
            hard: c["level"].as_str() == Some("hard"),
            reference: commands(&c["reference"]),
        });
    }
    Ok(out)
}

/// Where answers come from: the server, or a run kept before.
pub trait Source {
    fn answer(&mut self, case: &str, attempt: usize, body: &Value) -> Result<Value, String>;
    fn name(&self) -> String;
    /// Whether it can answer a fix (a replay only has the answers the run it keeps was given).
    fn has(&self, _case: &str, _attempt: usize) -> bool {
        true
    }
}

struct Live {
    url: String,
    key: String,
}

impl Source for Live {
    fn answer(&mut self, _case: &str, _attempt: usize, body: &Value) -> Result<Value, String> {
        assistant::call("POST", &self.url, &self.key, Some(body))
    }

    fn name(&self) -> String {
        self.url.clone()
    }
}

struct Replay {
    answers: BTreeMap<String, Vec<Value>>,
    from: String,
}

impl Source for Replay {
    fn answer(&mut self, case: &str, attempt: usize, _body: &Value) -> Result<Value, String> {
        let kept = self.answers.get(case).and_then(|a| a.get(attempt)).cloned();
        match kept {
            Some(v) if v.get("error").is_some_and(Value::is_string) && v.get("reply").is_none() => {
                Err(v["error"].as_str().unwrap_or("").to_string())
            }
            Some(v) => Ok(v),
            None => Err(format!(
                "{} kept no answer {attempt} for this case",
                self.from
            )),
        }
    }

    fn name(&self) -> String {
        format!("replay of {}", self.from)
    }

    fn has(&self, case: &str, attempt: usize) -> bool {
        self.answers.get(case).is_some_and(|a| a.len() > attempt)
    }
}

/// A fresh copy of the case's app, with its first drawing open.
fn fresh(target: Target) -> Box<dyn Host> {
    match target {
        Target::Cad => {
            let mut cad =
                aptcad_ui_egui::CadApp::new(aptcad_engine::Session::default(), Default::default());
            if cad.session.docs.is_empty() {
                let metric = cad.session.settings.measureinit;
                cad.session.new_drawing(metric);
            }
            Box::new(cad)
        }
        Target::Bim => Box::new(aptbim_ui_egui::BimApp::new(
            aptbim_core::engine::Session::new(),
        )),
    }
}

/// The values at a dotted path: `counts.Circle`, `layers.*.name`, `extents.1.0`. `*` takes every member.
fn at<'a>(v: &'a Value, path: &str) -> Vec<&'a Value> {
    let mut here = vec![v];
    for seg in path.split('.').filter(|s| !s.is_empty()) {
        here = here
            .into_iter()
            .flat_map(|v| -> Vec<&Value> {
                match (seg, v) {
                    ("*", Value::Array(a)) => a.iter().collect(),
                    ("*", Value::Object(o)) => o.values().collect(),
                    (_, Value::Array(a)) => seg
                        .parse::<usize>()
                        .ok()
                        .and_then(|i| a.get(i))
                        .into_iter()
                        .collect(),
                    (_, Value::Object(o)) => o.get(seg).into_iter().collect(),
                    _ => Vec::new(),
                }
            })
            .collect();
    }
    here
}

/// The number at a path; none there counts as 0 (a count of a kind not yet drawn).
fn number(v: &Value, path: &str) -> f64 {
    at(v, path).first().and_then(|n| n.as_f64()).unwrap_or(0.0)
}

fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(x), Value::String(y)) => x.eq_ignore_ascii_case(y),
        (Value::Number(_), Value::Number(_)) => {
            (a.as_f64().unwrap_or(f64::NAN) - b.as_f64().unwrap_or(f64::NAN)).abs() < 1e-6
        }
        _ => a == b,
    }
}

/// What a check reads, before the request and after: the state, or a query's answer when it says `in`.
struct Seen {
    before: Value,
    after: Value,
}

fn query(host: &mut dyn Host, check: &Value) -> Option<Value> {
    let q = check.get("in")?;
    let id = q["id"].as_str()?;
    Some(
        host.run(id, q.get("params").cloned().unwrap_or_else(|| json!({})))
            .unwrap_or(Value::Null),
    )
}

/// The queries the checks ask, keyed by where they sit, so each runs before and after.
fn queries(
    host: &mut dyn Host,
    checks: &[Value],
    into: &mut BTreeMap<String, Value>,
    prefix: &str,
) {
    for (i, c) in checks.iter().enumerate() {
        let key = format!("{prefix}{i}");
        if let Some(v) = query(host, c) {
            into.insert(key.clone(), v);
        }
        for nested in ["any_of", "all_of"] {
            if let Some(subs) = c[nested].as_array() {
                queries(host, subs, into, &format!("{key}."));
            }
        }
    }
}

fn check(
    c: &Value,
    key: &str,
    state: &Seen,
    before_q: &BTreeMap<String, Value>,
    after_q: &BTreeMap<String, Value>,
    proposed: usize,
) -> Result<(), String> {
    let why = c["why"]
        .as_str()
        .map(|w| format!(" ({w})"))
        .unwrap_or_default();
    let (before, after) = match (before_q.get(key), after_q.get(key)) {
        (Some(b), Some(a)) => (b, a),
        _ => (&state.before, &state.after),
    };
    if let Some(any) = c["any_of"].as_array() {
        let mut misses = Vec::new();
        for (i, sub) in any.iter().enumerate() {
            match check(
                sub,
                &format!("{key}.{i}"),
                state,
                before_q,
                after_q,
                proposed,
            ) {
                Ok(()) => return Ok(()),
                Err(e) => misses.push(e),
            }
        }
        return Err(format!("none of: {}{why}", misses.join("; ")));
    }
    if let Some(all) = c["all_of"].as_array() {
        for (i, sub) in all.iter().enumerate() {
            check(
                sub,
                &format!("{key}.{i}"),
                state,
                before_q,
                after_q,
                proposed,
            )
            .map_err(|e| format!("{e}{why}"))?;
        }
        return Ok(());
    }
    if c["no_commands"].as_bool() == Some(true) {
        return if proposed == 0 {
            Ok(())
        } else {
            Err(format!("{proposed} commands given for a question{why}"))
        };
    }
    if let Some(path) = c["grows"].as_str() {
        let grew = number(after, path) - number(before, path);
        if let Some(by) = c["by"].as_f64() {
            return if (grew - by).abs() < 1e-6 {
                Ok(())
            } else {
                Err(format!("{path} grew by {grew}, not {by}{why}"))
            };
        }
        let least = c["at_least"].as_f64().unwrap_or(1.0);
        return if grew + 1e-6 >= least {
            Ok(())
        } else {
            Err(format!("{path} grew by {grew}, not {least} or more{why}"))
        };
    }
    if let Some(path) = c["equals"].as_str() {
        let want = &c["value"];
        let got = at(after, path)
            .first()
            .map_or(Value::Null, |v| (*v).clone());
        let ok = same(&got, want) || (got.is_null() && want.as_f64() == Some(0.0));
        return if ok {
            Ok(())
        } else {
            Err(format!("{path} is {got}, not {want}{why}"))
        };
    }
    if let Some(path) = c["has"].as_str() {
        let want = &c["value"];
        return if at(after, path).iter().any(|v| same(v, want)) {
            Ok(())
        } else {
            Err(format!("no {want} in {path}{why}"))
        };
    }
    if let Some(path) = c["near"].as_str() {
        let (want, tol) = (
            c["value"].as_f64().unwrap_or(0.0),
            c["tol"].as_f64().unwrap_or(1.0),
        );
        let got = at(after, path).first().and_then(|v| v.as_f64());
        return match got {
            Some(g) if (g - want).abs() <= tol => Ok(()),
            _ => Err(format!(
                "{path} is {}, not {want} ± {tol}{why}",
                got.map_or("missing".into(), |g| g.to_string())
            )),
        };
    }
    Err(format!("a check this runner doesn't know: {c}"))
}

/// How one answer of a case went.
struct Attempt {
    report: Value,
    passed: bool,
    problems: Vec<String>,
}

pub struct Outcome {
    pub name: String,
    pub target: Target,
    pub hard: bool,
    pub first: bool,
    pub last: bool,
    pub attempts: Vec<Value>,
    pub problems: Vec<String>,
}

fn judge(
    case: &Case,
    host: &mut dyn Host,
    before: &Value,
    before_q: &BTreeMap<String, Value>,
    turn: &Value,
    proposed: usize,
) -> (bool, Vec<String>) {
    let mut problems = Vec::new();
    let failed: Vec<String> = turn["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            c["outcome"]["failed"]
                .as_str()
                .map(|e| format!("{} failed: {e}", c["id"].as_str().unwrap_or("?")))
        })
        .collect();
    problems.extend(failed);
    if let Some(d) = turn["dropped"].as_array().filter(|d| !d.is_empty()) {
        problems.push(format!(
            "left out (not in the catalog): {}",
            d.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let seen = Seen {
        before: before.clone(),
        after: host.state(),
    };
    let mut after_q = BTreeMap::new();
    queries(host, &case.checks, &mut after_q, "");
    for (i, c) in case.checks.iter().enumerate() {
        if let Err(e) = check(c, &i.to_string(), &seen, before_q, &after_q, proposed) {
            problems.push(e);
        }
    }
    (problems.is_empty(), problems)
}

/// Asks one case, runs what comes back, fixes as "Run as they come" would, and checks after each answer.
pub fn run_case(case: &Case, source: &mut dyn Source) -> Outcome {
    let mut host = fresh(case.target);
    let mut out = Outcome {
        name: case.name.clone(),
        target: case.target,
        hard: case.hard,
        first: false,
        last: false,
        attempts: Vec::new(),
        problems: Vec::new(),
    };
    for (id, params) in &case.setup {
        if let Err(e) = host.run(id, params.clone()) {
            out.problems
                .push(format!("the case's setup failed: {id}: {e}"));
            return out;
        }
    }
    let before = host.state();
    let mut before_q = BTreeMap::new();
    queries(host.as_mut(), &case.checks, &mut before_q, "");
    let catalog = crate::catalog(case.target);
    let mut assistant = Assistant::new(String::new(), String::new());
    assistant.auto_run = true;
    let mut body = assistant.request(
        case.target,
        Some(case.request.clone()),
        catalog.clone(),
        before.clone(),
    );
    let mut proposed = 0;
    for attempt in 0..=AUTO_FIXES {
        let answer = source.answer(&case.name, attempt, &body);
        let kept = answer.clone().unwrap_or_else(|e| json!({"error": e}));
        let turn = assistant.answer(case.target, answer);
        let Some(turn) = turn else {
            // No commands: a question answered, a request declined, or no answer at all.
            let report = json!({"answer": kept, "turn": Value::Null});
            let (passed, mut problems) = judge(
                case,
                host.as_mut(),
                &before,
                &before_q,
                &json!({}),
                proposed,
            );
            if let Some(e) = assistant.error() {
                problems.insert(0, format!("no answer: {e}"));
            }
            let passed = passed && assistant.error().is_none();
            out.attempts
                .push(json!({"answer": report["answer"], "passed": passed}));
            if attempt == 0 {
                out.first = passed;
            }
            out.last = passed;
            out.problems = problems;
            return out;
        };
        let commands = assistant
            .commands_of(turn)
            .map(|(_, c)| c)
            .unwrap_or_default();
        proposed += commands.len();
        let ran = host::run_all(host.as_mut(), &commands);
        let fix = assistant.record(turn, ran.outcomes);
        let report = assistant.turn_report(turn);
        let (passed, problems) = judge(case, host.as_mut(), &before, &before_q, &report, proposed);
        let a = Attempt {
            report,
            passed,
            problems,
        };
        out.attempts.push(
            json!({"answer": kept, "turn": a.report, "passed": a.passed, "problems": a.problems}),
        );
        if attempt == 0 {
            out.first = a.passed;
        }
        out.last = a.passed;
        out.problems = a.problems;
        if !fix || a.passed || !source.has(&case.name, attempt + 1) {
            break;
        }
        body = assistant.request(case.target, None, catalog.clone(), host.state());
    }
    out
}

/// Answers every case with its reference commands, and a case without one with none.
pub struct Reference(pub BTreeMap<String, Vec<(String, Value)>>);

impl Source for Reference {
    fn answer(&mut self, case: &str, attempt: usize, body: &Value) -> Result<Value, String> {
        assert!(
            body["catalog"].as_array().is_some_and(|c| c.len() > 50),
            "the catalog goes with every request"
        );
        assert!(body["state"].is_object(), "and the state");
        if attempt > 0 {
            return Ok(json!({"reply": "Nothing more to fix.", "commands": []}));
        }
        let commands: Vec<Value> = self
            .0
            .get(case)
            .into_iter()
            .flatten()
            .map(|(id, params)| json!({"id": id, "params": from_state(params, &body["state"])}))
            .collect();
        Ok(json!({"reply": "Done.", "commands": commands}))
    }

    fn name(&self) -> String {
        "the reference answers".into()
    }
}

/// `$state:PATH` strings swapped for the state's value there.
fn from_state(v: &Value, state: &Value) -> Value {
    match v {
        Value::String(s) if s.starts_with("$state:") => at(state, &s["$state:".len()..])
            .first()
            .map_or(Value::Null, |v| (*v).clone()),
        Value::Array(a) => Value::Array(a.iter().map(|x| from_state(x, state)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.clone(), from_state(x, state)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn code(t: Target) -> &'static str {
    match t {
        Target::Cad => "cad",
        Target::Bim => "bim",
    }
}

fn report(outcomes: &[Outcome], source: &str) -> String {
    let n = outcomes.len();
    let first = outcomes.iter().filter(|o| o.first).count();
    let last = outcomes.iter().filter(|o| o.last).count();
    let model = outcomes
        .iter()
        .flat_map(|o| o.attempts.iter())
        .find_map(|a| a["answer"]["model"].as_str().map(str::to_string))
        .unwrap_or_else(|| "not given".into());
    let mut md = String::new();
    let _ = writeln!(md, "# Assistant test set\n");
    let _ = writeln!(md, "Answers from {source}; model: {model}.\n");
    let _ = writeln!(
        md,
        "**{first} of {n}** pass on the first answer, **{last} of {n}** after the fixes asked for by themselves (at most {AUTO_FIXES}).\n"
    );
    let groups: [(&str, fn(&Outcome) -> bool); 4] = [
        ("cad", |o| o.target == Target::Cad),
        ("bim", |o| o.target == Target::Bim),
        ("the basic requests", |o| !o.hard),
        ("the hard requests", |o| o.hard),
    ];
    for (name, keep) in groups {
        let of: Vec<&Outcome> = outcomes.iter().filter(|o| keep(o)).collect();
        if !of.is_empty() {
            let _ = writeln!(
                md,
                "- {}: {} of {} first, {} of {} after fixes",
                name,
                of.iter().filter(|o| o.first).count(),
                of.len(),
                of.iter().filter(|o| o.last).count(),
                of.len()
            );
        }
    }
    let _ = writeln!(
        md,
        "\n| case | mode | first | after fixes | answers | problems |\n|---|---|---|---|---|---|"
    );
    for o in outcomes {
        let mark = |b: bool| if b { "pass" } else { "FAIL" };
        let problems = o.problems.join("; ").replace('|', "/");
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | {} | {} |",
            o.name,
            code(o.target),
            mark(o.first),
            mark(o.last),
            o.attempts.len(),
            problems
        );
    }
    md
}

fn write(out: &Path, outcomes: &[Outcome], source: &str) -> Result<(), String> {
    std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let results = json!({
        "source": source,
        "cases": outcomes.iter().map(|o| json!({"name": o.name, "mode": code(o.target), "hard": o.hard, "first": o.first, "last": o.last, "problems": o.problems, "attempts": o.attempts})).collect::<Vec<_>>(),
    });
    let pretty = serde_json::to_string_pretty(&results).map_err(|e| e.to_string())?;
    std::fs::write(out.join("results.json"), pretty).map_err(|e| format!("results.json: {e}"))?;
    std::fs::write(out.join("report.md"), report(outcomes, source))
        .map_err(|e| format!("report.md: {e}"))?;
    Ok(())
}

fn replay(path: &Path) -> Result<Replay, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut answers = BTreeMap::new();
    for c in v["cases"].as_array().into_iter().flatten() {
        let Some(name) = c["name"].as_str() else {
            continue;
        };
        let kept: Vec<Value> = c["attempts"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|a| a["answer"].clone())
            .collect();
        answers.insert(name.to_string(), kept);
    }
    Ok(Replay {
        answers,
        from: path.display().to_string(),
    })
}

pub fn main(args: &[String]) -> ExitCode {
    let mut cases_path = PathBuf::from("eval/cases.json");
    let mut out = PathBuf::from("eval/results");
    let (mut only, mut replay_from, mut reference) = (None, None, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().cloned();
        match a.as_str() {
            "--cases" => cases_path = value().map(PathBuf::from).unwrap_or(cases_path),
            "--out" => out = value().map(PathBuf::from).unwrap_or(out),
            "--only" => only = value(),
            "--replay" => replay_from = value().map(PathBuf::from),
            "--reference" => reference = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("aptimizer-studio eval: unknown argument `{other}`\n\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let cases = match std::fs::read_to_string(&cases_path)
        .map_err(|e| format!("{}: {e}", cases_path.display()))
        .and_then(|t| read_cases(&t))
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("aptimizer-studio eval: {e}");
            return ExitCode::from(2);
        }
    };
    let mut source: Box<dyn Source> = match replay_from {
        _ if reference => Box::new(Reference(
            cases
                .iter()
                .map(|c| (c.name.clone(), c.reference.clone()))
                .collect(),
        )),
        Some(path) => match replay(&path) {
            Ok(r) => Box::new(r),
            Err(e) => {
                eprintln!("aptimizer-studio eval: {e}");
                return ExitCode::from(2);
            }
        },
        None => {
            let (server, key) = (
                std::env::var("APTIMIZER_URL").unwrap_or_default(),
                std::env::var("APTIMIZER_KEY").unwrap_or_default(),
            );
            if server.trim().is_empty() || !key.trim().starts_with("apts_") {
                eprintln!(
                    "aptimizer-studio eval: needs APTIMIZER_URL (the Aptimizer server) and APTIMIZER_KEY (an access key from Profile > Aptimizer Studio), or --replay RESULTS"
                );
                return ExitCode::FAILURE;
            }
            let mut a = Assistant::new(server, key.trim().to_string());
            a.auto_run = true;
            Box::new(Live {
                url: a.request_url(),
                key: a.key.clone(),
            })
        }
    };
    let mut outcomes = Vec::new();
    for case in cases
        .iter()
        .filter(|c| only.as_ref().is_none_or(|o| c.name.contains(o.as_str())))
    {
        let o = run_case(case, source.as_mut());
        println!(
            "{:<28} first {:<4}  after fixes {:<4}  {}",
            o.name,
            if o.first { "pass" } else { "FAIL" },
            if o.last { "pass" } else { "FAIL" },
            o.problems.join("; ")
        );
        outcomes.push(o);
    }
    let name = source.name();
    if let Err(e) = write(&out, &outcomes, &name) {
        eprintln!("aptimizer-studio eval: {e}");
        return ExitCode::FAILURE;
    }
    let passed = outcomes.iter().filter(|o| o.last).count();
    println!(
        "\n{passed} of {} pass; the report is {}",
        outcomes.len(),
        out.join("report.md").display()
    );
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_case_has_a_reference_answer_that_passes_it() {
        let cases = read_cases(CASES);
        assert!(cases.is_ok(), "{:?}", cases.as_ref().err());
        let cases = cases.unwrap_or_default();
        assert!(cases.iter().filter(|c| c.target == Target::Cad).count() >= 6);
        assert!(cases.iter().filter(|c| c.target == Target::Bim).count() >= 6);
        assert!(
            cases.iter().filter(|c| c.hard).count() >= 10,
            "the harder requests"
        );
        let mut source = Reference(
            cases
                .iter()
                .map(|c| (c.name.clone(), c.reference.clone()))
                .collect(),
        );
        let mut failures = Vec::new();
        for case in &cases {
            let asks = case
                .checks
                .iter()
                .any(|c| c["no_commands"].as_bool() == Some(true));
            assert!(
                asks || !case.reference.is_empty(),
                "{}: no reference answer",
                case.name
            );
            let o = run_case(case, &mut source);
            if !o.first {
                failures.push(format!("{}: {}", o.name, o.problems.join("; ")));
            }
        }
        assert!(
            failures.is_empty(),
            "reference answers that fail their case:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn a_wrong_answer_fails_and_is_fixed_by_the_next() {
        let cases = read_cases(CASES).unwrap_or_default();
        let case = cases.iter().find(|c| c.name == "cad-circle-on-layer");
        assert!(case.is_some(), "the case is gone");
        let Some(case) = case else { return };
        struct Wrong(usize);
        impl Source for Wrong {
            fn answer(&mut self, _: &str, attempt: usize, body: &Value) -> Result<Value, String> {
                self.0 += 1;
                if attempt == 0 {
                    return Ok(json!({"reply": "x", "commands": [
                        {"id": "layer.new", "params": {"name": "WALLS", "color": "red", "current": true}},
                        {"id": "circle", "params": {"centre": [1000, 1000], "r": 500}},
                        {"id": "circle", "params": {"center": [0, 0], "radius": 1}}]}));
                }
                let results = &body["results"];
                assert_eq!(results[1]["ok"], json!(false), "{results}");
                assert!(results[2]["skipped"].is_string(), "{results}");
                Ok(
                    json!({"reply": "Fixed.", "commands": [{"id": "circle", "params": {"center": [1000, 1000], "radius": 500}}]}),
                )
            }
            fn name(&self) -> String {
                "wrong".into()
            }
        }
        let o = run_case(case, &mut Wrong(0));
        assert!(!o.first, "the first answer failed");
        assert!(o.last, "the fix passed: {:?}", o.problems);
        assert_eq!(o.attempts.len(), 2);
    }

    #[test]
    fn paths_reach_into_lists_and_maps() {
        let v = json!({"counts": {"Circle": 2}, "layers": [{"name": "0"}, {"name": "WALLS"}], "extents": [[0, 0], [4000, 3000]]});
        assert_eq!(number(&v, "counts.Circle"), 2.0);
        assert_eq!(number(&v, "counts.Line"), 0.0);
        assert_eq!(number(&v, "extents.1.0"), 4000.0);
        assert!(
            at(&v, "layers.*.name")
                .iter()
                .any(|n| same(n, &json!("walls")))
        );
    }
}

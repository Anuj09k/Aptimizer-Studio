//! Running the assistant's commands in an app, and taking them back.
//!
//! Studio runs a turn's commands through the app's own command path (`CadApp::run`, `BimApp::run_command`), as its
//! menus do, and the test set (`eval`) runs them the same way in a fresh copy of each app. While they run, Studio
//! counts the undo steps they make: each command either leaves the drawing as it was, or makes exactly one undo
//! step (the step back to the drawing it started from). Anything else (an undo, a new drawing, undo turned off)
//! makes the turn one that "Undo all" can't take back cleanly, so it isn't offered.
//!
//! "Undo all" is offered only while the drawing is still the one the turn left: holding that drawing (an `Arc`)
//! and comparing it with the open one says whether anything changed since, so it never takes back the user's own
//! later edits.

use std::sync::Arc;

use serde_json::{Value, json};

/// The drawing or model open in an app, held so it can be compared with the one open later.
#[derive(Clone)]
pub enum Doc {
    /// AptCAD's open drawing: its document's id, and the drawing.
    Cad(u64, Arc<aptcad_engine::doc::Drawing>),
    Bim(Arc<aptbim_core::model::Model>),
    None,
}

impl Doc {
    /// Whether two are the same drawing, unchanged.
    pub fn same(&self, other: &Doc) -> bool {
        match (self, other) {
            (Doc::Cad(a, x), Doc::Cad(b, y)) => a == b && Arc::ptr_eq(x, y),
            (Doc::Bim(x), Doc::Bim(y)) => Arc::ptr_eq(x, y),
            _ => false,
        }
    }
}

/// What one command did to the drawing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// Nothing (a query, a refused command).
    None,
    /// One undo step, back to the drawing it started from.
    Step,
    /// Something "Undo all" can't count on: an undo, another drawing, a change with no step.
    Other,
}

/// An app the assistant's commands run in.
pub trait Host {
    fn run(&mut self, id: &str, params: Value) -> Result<Value, String>;
    fn doc(&self) -> Doc;
    /// What the command `id` did, given the drawing it started from.
    fn change(&self, before: &Doc, id: &str) -> Change;
    /// Takes back `steps` undo steps.
    fn undo(&mut self, steps: usize) -> Result<(), String>;
    /// The drawing or model in brief, as the assistant is told it.
    fn state(&mut self) -> Value;
}

impl Host for aptcad_ui_egui::CadApp {
    fn run(&mut self, id: &str, params: Value) -> Result<Value, String> {
        aptcad_ui_egui::CadApp::run(self, id, params)
    }

    fn doc(&self) -> Doc {
        self.session
            .state()
            .map_or(Doc::None, |s| Doc::Cad(s.uid, Arc::clone(&s.doc)))
    }

    fn change(&self, before: &Doc, _id: &str) -> Change {
        let Doc::Cad(uid, was) = before else {
            return Change::Other;
        };
        let Ok(now) = self.session.state() else {
            return Change::Other;
        };
        if now.uid != *uid {
            return Change::Other;
        }
        if Arc::ptr_eq(&now.doc, was) {
            return Change::None;
        }
        // An undoable change pushes the drawing it started from as the newest step.
        match now.undo.last() {
            Some(step) if Arc::ptr_eq(&step.doc, was) => Change::Step,
            _ => Change::Other,
        }
    }

    fn undo(&mut self, steps: usize) -> Result<(), String> {
        for _ in 0..steps {
            aptcad_ui_egui::CadApp::run(self, "u", json!({}))?;
        }
        Ok(())
    }

    fn state(&mut self) -> Value {
        let mut state = aptcad_ui_egui::CadApp::run(
            self,
            "drawing.inspect",
            json!({"entities": true, "limit": BRIEF_ENTITIES}),
        )
        .unwrap_or(Value::Null);
        // What a file's reader left out says nothing about drawing in it.
        if let Some(o) = state.as_object_mut() {
            o.remove("skipped");
            o.remove("readNotes");
        }
        state
    }
}

impl Host for aptbim_ui_egui::BimApp {
    fn run(&mut self, id: &str, params: Value) -> Result<Value, String> {
        self.run_command(id, params)
    }

    fn doc(&self) -> Doc {
        Doc::Bim(Arc::clone(&self.session.model))
    }

    fn change(&self, before: &Doc, id: &str) -> Change {
        let Doc::Bim(was) = before else {
            return Change::Other;
        };
        if Arc::ptr_eq(&self.session.model, was) {
            return Change::None;
        }
        // The session pushes a step for every undoable command that changed the model, and none for the rest.
        match aptbim_core::engine::find(id) {
            Some(c) if c.undoable => Change::Step,
            _ => Change::Other,
        }
    }

    fn undo(&mut self, steps: usize) -> Result<(), String> {
        self.run_command("undo", json!({"count": steps}))
            .map(|_| ())
    }

    fn state(&mut self) -> Value {
        let full = self
            .run_command("inspect", json!({}))
            .unwrap_or(Value::Null);
        let mut brief = bim_brief(&full, &self.selection);
        // The ids commands take, the first few of each category, so "delete the walls" needs no question back.
        let mut ids: std::collections::BTreeMap<&str, Vec<Value>> =
            std::collections::BTreeMap::new();
        for (id, e) in self.session.model.elements.iter() {
            let of = ids.entry(e.category()).or_default();
            if of.len() < BRIEF_IDS {
                of.push(json!(id));
            }
        }
        brief["ids"] = json!(ids);
        brief
    }
}

/// The ids of each category the brief lists.
const BRIEF_IDS: usize = 30;
/// The objects AptCAD's state lists (handle, type, layer, geometry), so a request can name them.
const BRIEF_ENTITIES: u64 = 25;

/// AptBIM's `inspect` in brief: the server reads the first few thousand characters of a state, and the full one
/// (every type's layers, every material, the electrical settings) runs past them before the parts a request needs.
/// Kept: the project, its levels, what it holds by category, its types by name, the selection, the last steps.
pub fn bim_brief(full: &Value, selection: &[aptbim_core::ids::Id]) -> Value {
    let names = |v: &Value| -> Value {
        v.as_object()
            .map(|o| Value::Array(o.keys().map(|k| Value::String(k.clone())).collect()))
            .unwrap_or(Value::Null)
    };
    let mut types = serde_json::Map::new();
    if let Some(all) = full["types"].as_object() {
        for (category, of) in all {
            if category == "runs" {
                for (run, of) in of.as_object().into_iter().flatten() {
                    types.insert(run.clone(), names(of));
                }
            } else if of.as_object().is_some_and(|o| !o.is_empty()) {
                types.insert(category.clone(), names(of));
            }
        }
    }
    let warnings = full["warnings"].as_array().map_or(0, Vec::len);
    let history: Vec<Value> = full["history"]
        .as_array()
        .map(|h| h.iter().rev().take(10).rev().cloned().collect())
        .unwrap_or_default();
    json!({
        "name": full["name"], "units": "mm", "project_units": full["project_units"], "levels": full["levels"],
        "counts": full["counts"], "types": types, "families": names(&full["families"]),
        "selection": selection.iter().map(|id| json!(id)).collect::<Vec<_>>(),
        "warnings": warnings, "last_steps": history,
    })
}

/// How one command went.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Ran(Value),
    Failed(String),
    /// Not run, because one before it failed.
    Skipped,
}

/// A turn's commands, run.
pub struct Ran {
    pub outcomes: Vec<Outcome>,
    /// The undo steps they made, if "Undo all" can take them back (see the module's notes).
    pub steps: Option<usize>,
    /// The drawing they left.
    pub after: Doc,
}

/// Runs `commands` in order; after one fails, the rest are skipped (they were planned on it).
pub fn run_all(host: &mut dyn Host, commands: &[(String, Value)]) -> Ran {
    let mut outcomes = Vec::with_capacity(commands.len());
    let (mut steps, mut clean, mut failed) = (0usize, true, false);
    for (id, params) in commands {
        if failed {
            outcomes.push(Outcome::Skipped);
            continue;
        }
        let before = host.doc();
        let result = host.run(id, params.clone());
        match host.change(&before, id) {
            Change::None => {}
            Change::Step => steps += 1,
            Change::Other => clean = false,
        }
        outcomes.push(match result {
            Ok(v) => Outcome::Ran(v),
            Err(e) => {
                failed = true;
                Outcome::Failed(e)
            }
        });
    }
    Ran {
        outcomes,
        steps: clean.then_some(steps),
        after: host.doc(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cad() -> aptcad_ui_egui::CadApp {
        aptcad_ui_egui::CadApp::new(aptcad_engine::Session::default(), Default::default())
    }

    fn bim() -> aptbim_ui_egui::BimApp {
        aptbim_ui_egui::BimApp::new(aptbim_core::engine::Session::new())
    }

    fn circles(app: &mut aptcad_ui_egui::CadApp) -> u64 {
        app.state()["counts"]["Circle"].as_u64().unwrap_or(0)
    }

    fn walls(app: &mut aptbim_ui_egui::BimApp) -> u64 {
        app.state()["counts"]["wall"].as_u64().unwrap_or(0)
    }

    #[test]
    fn a_cad_turn_counts_its_steps_and_undo_all_takes_back_only_those() {
        let mut app = cad();
        let _ = Host::run(&mut app, "circle", json!({"center": [0, 0], "radius": 1}));
        let start = circles(&mut app);
        let commands = vec![
            (
                "circle".to_string(),
                json!({"center": [10, 0], "radius": 2}),
            ),
            ("drawing.inspect".to_string(), json!({"entities": false})),
            (
                "circle".to_string(),
                json!({"center": [20, 0], "radius": 3}),
            ),
        ];
        let ran = run_all(&mut app, &commands);
        assert!(
            ran.outcomes.iter().all(|o| matches!(o, Outcome::Ran(_))),
            "{:?}",
            ran.outcomes
        );
        assert_eq!(ran.steps, Some(2), "two circles, two steps; the query none");
        assert!(ran.after.same(&app.doc()));
        assert_eq!(circles(&mut app), start + 2);
        assert!(app.undo(2).is_ok());
        assert_eq!(
            circles(&mut app),
            start,
            "the circle drawn before the turn stays"
        );
        assert!(
            !ran.after.same(&app.doc()),
            "after the undo it's another drawing"
        );
    }

    #[test]
    fn a_failed_command_skips_the_rest() {
        let mut app = cad();
        let commands = vec![
            ("circle".to_string(), json!({"center": [0, 0], "radius": 1})),
            ("circle".to_string(), json!({"radius": "big"})),
            ("circle".to_string(), json!({"center": [5, 0], "radius": 1})),
        ];
        let ran = run_all(&mut app, &commands);
        assert!(matches!(ran.outcomes[0], Outcome::Ran(_)));
        assert!(matches!(ran.outcomes[1], Outcome::Failed(_)));
        assert_eq!(ran.outcomes[2], Outcome::Skipped);
        assert_eq!(ran.steps, Some(1));
    }

    #[test]
    fn an_undo_inside_a_turn_makes_it_one_undo_all_cant_count() {
        let mut app = cad();
        let commands = vec![
            ("circle".to_string(), json!({"center": [0, 0], "radius": 1})),
            ("u".to_string(), json!({})),
        ];
        assert_eq!(run_all(&mut app, &commands).steps, None);
    }

    #[test]
    fn a_bim_turn_counts_its_steps_and_undo_all_takes_back_only_those() {
        let mut app = bim();
        let _ = app.run_command("wall", json!({"points": [[0, 0], [4000, 0]]}));
        let start = walls(&mut app);
        assert!(start > 0);
        let commands = vec![
            (
                "wall".to_string(),
                json!({"points": [[0, 3000], [4000, 3000]]}),
            ),
            ("inspect".to_string(), json!({})),
            (
                "wall".to_string(),
                json!({"points": [[0, 6000], [4000, 6000]]}),
            ),
        ];
        let ran = run_all(&mut app, &commands);
        assert!(
            ran.outcomes.iter().all(|o| matches!(o, Outcome::Ran(_))),
            "{:?}",
            ran.outcomes
        );
        assert_eq!(ran.steps, Some(2));
        assert_eq!(walls(&mut app), start + 2);
        assert!(app.undo(2).is_ok());
        assert_eq!(walls(&mut app), start);
    }

    #[test]
    fn the_bim_brief_keeps_what_a_request_needs_and_stays_short() {
        let mut app = bim();
        let brief = app.state();
        assert!(
            brief["levels"].as_array().is_some_and(|l| !l.is_empty()),
            "{brief}"
        );
        assert!(
            brief["types"]["wall"]
                .as_array()
                .is_some_and(|t| !t.is_empty()),
            "{brief}"
        );
        assert!(
            brief.to_string().len() < 4000,
            "{} characters",
            brief.to_string().len()
        );
    }
}

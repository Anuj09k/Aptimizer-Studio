//! The AI assistant: a panel beside either mode where the user types what they want ("draw a 4 x 3 m room
//! with 230 mm walls", "tag every door"). Aptimizer's server asks its model and answers with a reply and a
//! list of the open app's own commands; the user runs them (or lets them run as they come), and each goes
//! through the same command path as the app's menus, so it is checked by the app and can be undone. The
//! model never edits a drawing.
//!
//! When a command fails, the ones after it are skipped (they were planned on it) and the turn offers **Fix it**: the
//! failures go back to the model, which answers with the commands that finish the request from the drawing as it is
//! now. With "Run as they come" on, that happens by itself, at most [`AUTO_FIXES`] times in a row. **Undo all**
//! takes back a turn's commands together (see `host`).
//!
//! Studio holds no AI key: it connects to the user's Aptimizer account with an access key made in
//! Aptimizer's Profile > Aptimizer Studio, and the server calls the model and counts the call against the
//! account's allowance. Network calls run on a thread of their own, so the window never waits on them.

use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use egui::{Color32, RichText, ScrollArea, TextEdit};
use serde_json::{Value, json};

use crate::host::Outcome;

/// The longest a call to the server may take.
const TIMEOUT: Duration = Duration::from_secs(90);
/// Turns of conversation sent back with each request.
const HISTORY: usize = 12;
/// The most characters of a command's result kept and shown.
const RESULT_CHARS: usize = 300;
/// Fixes asked for by themselves, with "Run as they come" on, before the assistant waits for the user.
pub const AUTO_FIXES: usize = 2;
/// What a fix asks the model, and what the panel shows for it.
const FIX_REQUEST: &str = "Fix it: some of your commands failed or were skipped (see the results of the commands run \
    last). The ones that ran have already changed the drawing, so don't repeat them: give the commands that finish my \
    last request from the state now, or say what is in the way.";
const FIX_SHOWN: &str = "Fix it";

/// Which app a request is for, as the server names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Cad,
    Bim,
}

impl Target {
    fn code(self) -> &'static str {
        match self {
            Target::Cad => "cad",
            Target::Bim => "bim",
        }
    }
}

/// How a command went, as the panel shows it.
#[derive(Clone, Debug, PartialEq)]
pub enum Done {
    Ran(String),
    Failed(String),
    Skipped,
}

/// A command the model proposed, and how it went once run.
#[derive(Clone, Debug)]
pub struct Proposed {
    pub id: String,
    pub params: Value,
    pub outcome: Option<Done>,
}

#[derive(Clone, Debug)]
enum Turn {
    User(String),
    Assistant {
        text: String,
        target: Target,
        commands: Vec<Proposed>,
        dropped: Vec<String>,
        /// Its commands were taken back with Undo all.
        undone: bool,
    },
    Note(String),
}

enum Reply {
    Connected(Result<(String, bool), String>),
    Answer(Target, Result<Value, String>),
}

/// What the panel asks Studio to do this frame: things only Studio, which owns both apps, can do.
pub enum Action {
    /// Send this request, with the open app's catalog and state.
    Ask(String),
    /// Run the commands of the turn at this index.
    Run(usize),
    /// Take back the commands the turn at this index ran.
    Undo(usize),
    /// Send the failures of the turn at this index back to the model.
    Fix(usize),
}

/// The panel's state: the connection, the conversation, the call in flight.
pub struct Assistant {
    pub open: bool,
    pub server: String,
    pub key: String,
    /// Whose account the key connects to, once the server said so.
    pub connected: Option<String>,
    /// Run the commands as they arrive, rather than on a click.
    pub auto_run: bool,
    input: String,
    turns: Vec<Turn>,
    pending: Option<Receiver<Reply>>,
    error: Option<String>,
    show_settings: bool,
    /// Fixes asked for by themselves since the user's last request.
    fixes: usize,
}

impl Assistant {
    pub fn new(server: String, key: String) -> Assistant {
        let show_settings = server.trim().is_empty() || key.trim().is_empty();
        Assistant {
            open: false,
            server,
            key,
            connected: None,
            auto_run: false,
            input: String::new(),
            turns: Vec::new(),
            pending: None,
            error: None,
            show_settings,
            fixes: 0,
        }
    }

    /// What went wrong with the last call, if it failed.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/api{path}",
            self.server
                .trim()
                .trim_end_matches('/')
                .trim_end_matches("/api")
        )
    }

    /// Checks the key with the server, on a thread.
    pub fn connect(&mut self, ctx: &egui::Context) {
        let (url, key, ctx) = (
            self.url("/studio/me"),
            self.key.trim().to_string(),
            ctx.clone(),
        );
        let (tx, rx) = channel();
        self.pending = Some(rx);
        self.error = None;
        std::thread::spawn(move || {
            let reply = call("GET", &url, &key, None).map(|v| {
                let who = v["email"].as_str().unwrap_or("your account").to_string();
                (who, v["assistant"].as_bool().unwrap_or(false))
            });
            let _ = tx.send(Reply::Connected(reply));
            ctx.request_repaint();
        });
    }

    /// Sends the failures of the last turn that ran back to the model, as the user's next request.
    pub fn fix(&mut self, ctx: &egui::Context, target: Target, catalog: Value, state: Value) {
        let body = self.request(target, None, catalog, state);
        self.post(ctx, target, body);
    }

    /// Sends a request with the open app's catalog and state, on a thread.
    pub fn ask(
        &mut self,
        ctx: &egui::Context,
        target: Target,
        message: String,
        catalog: Value,
        state: Value,
    ) {
        let body = self.request(target, Some(message), catalog, state);
        self.post(ctx, target, body);
    }

    /// The server's address for a request.
    pub fn request_url(&self) -> String {
        self.url("/studio/assistant")
    }

    /// The results of the last turn whose commands ran, as the server reads them: each command, and whether it ran,
    /// failed (with the app's reason and what it was given) or was skipped.
    fn last_results(&self) -> Vec<Value> {
        self.turns
            .iter()
            .rev()
            .find_map(|t| match t {
                Turn::Assistant { commands, .. } if commands.iter().any(|c| c.outcome.is_some()) => Some(commands),
                _ => None,
            })
            .map(|cs| {
                cs.iter()
                    .filter_map(|c| {
                        c.outcome.as_ref().map(|o| match o {
                            Done::Ran(r) => json!({"id": c.id, "ok": true, "result": r}),
                            Done::Failed(e) => json!({"id": c.id, "params": c.params, "ok": false, "error": e}),
                            Done::Skipped => json!({"id": c.id, "params": c.params, "ok": false, "skipped": "not run: an earlier command failed"}),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A request's body, as the server reads it, and the user's turn in the conversation: `message`, or with none a
    /// fix ([`FIX_REQUEST`]). The user's own request starts the count of fixes that come by themselves again.
    pub fn request(
        &mut self,
        target: Target,
        message: Option<String>,
        catalog: Value,
        state: Value,
    ) -> Value {
        let (shown, message) = match message {
            Some(m) => {
                self.fixes = 0;
                (m.clone(), m)
            }
            None => (FIX_SHOWN.to_string(), FIX_REQUEST.to_string()),
        };
        let history: Vec<Value> = self
            .turns
            .iter()
            .filter_map(|t| match t {
                Turn::User(text) => Some(json!({"role": "user", "text": text})),
                Turn::Assistant { text, .. } => Some(json!({"role": "assistant", "text": text})),
                Turn::Note(_) => None,
            })
            .rev()
            .take(HISTORY)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let results = self.last_results();
        self.turns.push(Turn::User(shown));
        json!({"mode": target.code(), "message": message, "catalog": catalog, "state": state,
               "history": history, "results": results})
    }

    /// Sends a request's body to the server, on a thread.
    fn post(&mut self, ctx: &egui::Context, target: Target, body: Value) {
        let (url, key, ctx) = (self.request_url(), self.key.trim().to_string(), ctx.clone());
        let (tx, rx) = channel();
        self.pending = Some(rx);
        self.error = None;
        std::thread::spawn(move || {
            let _ = tx.send(Reply::Answer(target, call("POST", &url, &key, Some(&body))));
            ctx.request_repaint();
        });
    }

    /// Takes what a thread finished, if one did. Returns the index of a turn whose commands should run now.
    pub fn poll(&mut self) -> Option<usize> {
        let reply = self.pending.as_ref()?.try_recv().ok()?;
        self.pending = None;
        match reply {
            Reply::Connected(Ok((who, has_model))) => {
                self.connected = Some(who);
                self.show_settings = false;
                if !has_model {
                    self.turns.push(Turn::Note(
                        "Connected, but the server has no AI model configured: ask its administrator to set one.".into(),
                    ));
                }
                None
            }
            Reply::Connected(Err(e)) => {
                self.connected = None;
                self.error = Some(e);
                None
            }
            Reply::Answer(target, answer) => self.answer(target, answer),
        }
    }

    /// Takes the server's answer to a request into the conversation. Returns the index of its turn when its commands
    /// should run now ("Run as they come").
    pub fn answer(&mut self, target: Target, answer: Result<Value, String>) -> Option<usize> {
        match answer {
            Ok(v) => {
                let commands: Vec<Proposed> = v["commands"]
                    .as_array()
                    .map(|cs| {
                        cs.iter()
                            .filter_map(|c| {
                                Some(Proposed {
                                    id: c["id"].as_str()?.to_string(),
                                    params: c.get("params").cloned().unwrap_or_else(|| json!({})),
                                    outcome: None,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let dropped = v["dropped"]
                    .as_array()
                    .map(|d| {
                        d.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let text = v["reply"].as_str().unwrap_or("").to_string();
                let has_commands = !commands.is_empty();
                self.turns.push(Turn::Assistant {
                    text,
                    target,
                    commands,
                    dropped,
                    undone: false,
                });
                (self.auto_run && has_commands).then(|| self.turns.len() - 1)
            }
            Err(e) => {
                self.error = Some(e);
                None
            }
        }
    }

    /// How many turns the conversation has.
    #[cfg(test)]
    pub fn turns_len(&self) -> usize {
        self.turns.len()
    }

    /// The app a turn's commands are for.
    pub fn target_of(&self, turn: usize) -> Option<Target> {
        match self.turns.get(turn)? {
            Turn::Assistant { target, .. } => Some(*target),
            _ => None,
        }
    }

    /// A turn as the test set reports it: the reply, each command and how it went, what was left out.
    pub fn turn_report(&self, turn: usize) -> Value {
        let Some(Turn::Assistant {
            text,
            commands,
            dropped,
            ..
        }) = self.turns.get(turn)
        else {
            return Value::Null;
        };
        let commands: Vec<Value> = commands
            .iter()
            .map(|c| {
                let outcome = match &c.outcome {
                    None => json!(null),
                    Some(Done::Ran(_)) => json!("ran"),
                    Some(Done::Failed(e)) => json!({"failed": e}),
                    Some(Done::Skipped) => json!("skipped"),
                };
                json!({"id": c.id, "params": c.params, "outcome": outcome})
            })
            .collect();
        json!({"reply": text, "commands": commands, "dropped": dropped})
    }

    /// The commands of a turn, for Studio to run, and the app they are for; none if they already ran.
    pub fn commands_of(&self, turn: usize) -> Option<(Target, Vec<(String, Value)>)> {
        match self.turns.get(turn)? {
            Turn::Assistant {
                target, commands, ..
            } if commands.iter().all(|c| c.outcome.is_none()) => Some((
                *target,
                commands
                    .iter()
                    .map(|c| (c.id.clone(), c.params.clone()))
                    .collect(),
            )),
            _ => None,
        }
    }

    /// Records how each of a turn's commands went. True when a fix should be asked for now, by itself: one failed,
    /// "Run as they come" is on, and fewer than [`AUTO_FIXES`] were asked for since the user's last request.
    pub fn record(&mut self, turn: usize, outcomes: Vec<Outcome>) -> bool {
        if let Some(Turn::Assistant { commands, .. }) = self.turns.get_mut(turn) {
            for (c, o) in commands.iter_mut().zip(outcomes) {
                c.outcome = Some(match o {
                    Outcome::Ran(v) => Done::Ran(shorten(&v.to_string())),
                    Outcome::Failed(e) => Done::Failed(shorten(&e)),
                    Outcome::Skipped => Done::Skipped,
                });
            }
        }
        let auto = self.auto_run && self.failed(turn) && self.fixes < AUTO_FIXES;
        if auto {
            self.fixes += 1;
        }
        auto
    }

    /// Whether one of the turn's commands failed.
    pub fn failed(&self, turn: usize) -> bool {
        matches!(self.turns.get(turn), Some(Turn::Assistant { commands, undone: false, .. })
            if commands.iter().any(|c| matches!(c.outcome, Some(Done::Failed(_)))))
    }

    /// The last of the assistant's turns.
    fn last_answer(&self) -> Option<usize> {
        self.turns
            .iter()
            .rposition(|t| matches!(t, Turn::Assistant { .. }))
    }

    /// Notes how Undo all went for a turn: the steps it took back, or why it couldn't.
    pub fn undone(&mut self, turn: usize, result: Result<usize, String>) {
        match result {
            Ok(steps) => {
                if let Some(Turn::Assistant { undone, .. }) = self.turns.get_mut(turn) {
                    *undone = true;
                }
                let s = if steps == 1 { "" } else { "s" };
                self.turns.push(Turn::Note(format!(
                    "Undid that turn: {steps} step{s} taken back."
                )));
            }
            Err(e) => self
                .turns
                .push(Turn::Note(format!("Undo all didn't finish: {e}"))),
        }
    }

    /// The panel, in `ui`. `target` is the mode shown, `undoable` the turn Undo all can take back now (the last that
    /// ran, while the drawing is still the one it left); what the user asks for comes back as an Action.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        target: Target,
        undoable: Option<usize>,
    ) -> Option<Action> {
        let mut action = None;
        let last = self.last_answer();
        let can_send = self.connected.is_some() && !self.busy();
        ui.horizontal(|ui| {
            ui.heading("Assistant");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Close").clicked() {
                    self.open = false;
                }
                if ui.small_button("Connection").clicked() {
                    self.show_settings = !self.show_settings;
                }
            });
        });
        let where_ = match target {
            Target::Cad => "AptCAD",
            Target::Bim => "AptBIM",
        };
        ui.label(RichText::new(format!("Works in {where_}: it proposes {where_}'s own commands, which you run and can undo.")).small());
        ui.separator();
        if self.show_settings || self.connected.is_none() {
            self.settings_ui(ui);
            ui.separator();
        }
        let input_height = 70.0;
        ScrollArea::vertical()
            .max_height((ui.available_height() - input_height - 40.0).max(80.0))
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if self.turns.is_empty() {
                    ui.label(RichText::new("Ask for something to draw or change, e.g. \"draw a 4 x 3 m room with 230 mm walls\" or \"tag every door\".").weak());
                }
                for (i, turn) in self.turns.iter().enumerate() {
                    match turn {
                        Turn::User(text) => {
                            ui.label(RichText::new(text).strong());
                        }
                        Turn::Note(text) => {
                            ui.label(RichText::new(text).italics().weak());
                        }
                        Turn::Assistant { text, commands, dropped, undone, .. } => {
                            if !text.is_empty() {
                                ui.label(text);
                            }
                            for c in commands {
                                let params = shorten(&c.params.to_string());
                                let (mark, colour) = match &c.outcome {
                                    None => ("•", ui.visuals().weak_text_color()),
                                    Some(Done::Ran(_)) => ("✔", Color32::from_rgb(40, 150, 70)),
                                    Some(Done::Failed(_)) => ("✘", Color32::from_rgb(200, 60, 50)),
                                    Some(Done::Skipped) => ("–", ui.visuals().weak_text_color()),
                                };
                                ui.label(RichText::new(format!("{mark} {} {params}", c.id)).monospace().small().color(colour));
                                match &c.outcome {
                                    Some(Done::Failed(e)) => {
                                        ui.label(RichText::new(e).small().color(colour));
                                    }
                                    Some(Done::Skipped) => {
                                        ui.label(RichText::new("Skipped: an earlier command failed.").small().weak());
                                    }
                                    _ => {}
                                }
                            }
                            if !dropped.is_empty() {
                                ui.label(RichText::new(format!("Left out (not this app's commands): {}", dropped.join(", "))).small().weak());
                            }
                            if !commands.is_empty() && commands.iter().all(|c| c.outcome.is_none()) && ui.button("Run these").clicked() {
                                action = Some(Action::Run(i));
                            }
                            let failed = commands.iter().any(|c| matches!(c.outcome, Some(Done::Failed(_))));
                            ui.horizontal(|ui| {
                                if *undone {
                                    ui.label(RichText::new("Undone").small().weak());
                                } else if undoable == Some(i)
                                    && ui.button("Undo all").on_hover_text("Take back every change this turn made").clicked()
                                {
                                    action = Some(Action::Undo(i));
                                }
                                if failed && !*undone && last == Some(i)
                                    && ui.add_enabled(can_send, egui::Button::new("Fix it"))
                                        .on_hover_text("Send the failures back to the assistant to correct")
                                        .clicked()
                                {
                                    action = Some(Action::Fix(i));
                                }
                            });
                        }
                    }
                    ui.add_space(6.0);
                }
                if self.busy() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Thinking…");
                    });
                }
                // Before it connects, the connection's form shows what went wrong.
                if let Some(e) = self.error.as_ref().filter(|_| self.connected.is_some()) {
                    ui.label(RichText::new(e).color(Color32::from_rgb(200, 60, 50)));
                }
            });
        ui.separator();
        let ready = self.connected.is_some() && !self.busy();
        let field = ui.add_enabled(
            self.connected.is_some(),
            TextEdit::multiline(&mut self.input)
                .desired_rows(2)
                .desired_width(f32::INFINITY)
                .hint_text("What should it do?"),
        );
        let enter = field.has_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
        ui.horizontal(|ui| {
            let send = ui
                .add_enabled(
                    ready && !self.input.trim().is_empty(),
                    egui::Button::new("Send"),
                )
                .clicked();
            ui.checkbox(&mut self.auto_run, "Run as they come");
            if (send || (enter && ready)) && !self.input.trim().is_empty() {
                let text = std::mem::take(&mut self.input).trim().to_string();
                action = Some(Action::Ask(text));
            }
        });
        action
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Connect to your Aptimizer account").strong());
        ui.label(
            RichText::new(
                "Make an access key in Aptimizer: Profile > Aptimizer Studio > Make a key.",
            )
            .small(),
        );
        egui::Grid::new("assistant-connection")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label("Server");
                ui.add(
                    TextEdit::singleline(&mut self.server)
                        .hint_text("https://your-aptimizer-server")
                        .desired_width(200.0),
                );
                ui.end_row();
                ui.label("Access key");
                ui.add(
                    TextEdit::singleline(&mut self.key)
                        .password(true)
                        .hint_text("apts_…")
                        .desired_width(200.0),
                );
                ui.end_row();
            });
        ui.horizontal(|ui| {
            let ready = !self.server.trim().is_empty()
                && self.key.trim().starts_with("apts_")
                && !self.busy();
            if ui
                .add_enabled(ready, egui::Button::new("Connect"))
                .clicked()
            {
                self.connect(ui.ctx());
            }
            if let Some(who) = &self.connected {
                ui.label(RichText::new(format!("Connected as {who}")).small());
            }
        });
        if let Some(e) = &self.error
            && self.connected.is_none()
        {
            ui.label(
                RichText::new(e)
                    .small()
                    .color(Color32::from_rgb(200, 60, 50)),
            );
        }
    }
}

fn shorten(text: &str) -> String {
    if text.chars().count() <= RESULT_CHARS {
        return text.to_string();
    }
    let cut: String = text.chars().take(RESULT_CHARS).collect();
    format!("{cut}…")
}

/// One call to the server: its JSON, or the reason it failed (the server's own `detail` when it gave one).
pub fn call(method: &str, url: &str, key: &str, body: Option<&Value>) -> Result<Value, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(TIMEOUT))
        .build()
        .into();
    let auth = format!("Bearer {key}");
    let response = match (method, body) {
        ("POST", Some(b)) => agent
            .post(url)
            .header("Authorization", &auth)
            .header("Content-Type", "application/json")
            .send(b.to_string()),
        _ => agent.get(url).header("Authorization", &auth).call(),
    };
    let mut response = response.map_err(|e| format!("Could not reach the server: {e}"))?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .with_config()
        .limit(4 * 1024 * 1024)
        .read_to_string()
        .map_err(|e| format!("The server's answer could not be read: {e}"))?;
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if (200..300).contains(&status) {
        return Ok(value);
    }
    let detail = value["detail"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| format!("The server answered {status}."));
    Err(detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_address_takes_api_once_however_it_was_typed() {
        for typed in [
            "https://apt.example.com",
            "https://apt.example.com/",
            "https://apt.example.com/api",
            " https://apt.example.com/api/ ",
        ] {
            let a = Assistant::new(typed.into(), "apts_x".into());
            assert_eq!(
                a.url("/studio/me"),
                "https://apt.example.com/api/studio/me",
                "{typed}"
            );
        }
    }

    fn two_lines() -> Turn {
        Turn::Assistant {
            text: "Two lines.".into(),
            target: Target::Cad,
            commands: vec![
                Proposed {
                    id: "line".into(),
                    params: json!({"points": [[0, 0], [10, 0]]}),
                    outcome: None,
                },
                Proposed {
                    id: "line".into(),
                    params: json!({"points": [[0]]}),
                    outcome: None,
                },
                Proposed {
                    id: "circle".into(),
                    params: json!({"center": [0, 0], "radius": 1}),
                    outcome: None,
                },
            ],
            dropped: vec![],
            undone: false,
        }
    }

    fn ran_with_a_failure() -> Vec<Outcome> {
        vec![
            Outcome::Ran(json!({"handle": "1A"})),
            Outcome::Failed("needs two points".into()),
            Outcome::Skipped,
        ]
    }

    #[test]
    fn a_turns_commands_run_once_and_their_outcomes_are_kept() {
        let mut a = Assistant::new("https://s".into(), "apts_x".into());
        a.turns.push(two_lines());
        let (target, commands) = a.commands_of(0).unwrap_or((Target::Bim, vec![]));
        assert_eq!((target, commands.len()), (Target::Cad, 3));
        assert!(
            !a.record(0, ran_with_a_failure()),
            "no fix by itself unless the commands run as they come"
        );
        assert!(a.commands_of(0).is_none(), "they don't run twice");
        let Some(Turn::Assistant { commands, .. }) = a.turns.first() else {
            return;
        };
        assert!(matches!(&commands[1].outcome, Some(Done::Failed(e)) if e == "needs two points"));
        assert_eq!(commands[2].outcome, Some(Done::Skipped));
        assert!(a.failed(0));
    }

    #[test]
    fn the_model_is_told_what_failed_with_what_it_was_given_and_what_was_skipped() {
        let mut a = Assistant::new("https://s".into(), "apts_x".into());
        a.turns.push(two_lines());
        a.record(0, ran_with_a_failure());
        let results = a.last_results();
        assert_eq!(results[0]["ok"], json!(true));
        assert_eq!(results[1]["error"], json!("needs two points"));
        assert_eq!(results[1]["params"], json!({"points": [[0]]}));
        assert!(results[2]["skipped"].is_string());
    }

    #[test]
    fn fixes_come_by_themselves_only_twice_in_a_row() {
        let mut a = Assistant::new("https://s".into(), "apts_x".into());
        a.auto_run = true;
        let mut asked = 0;
        for turn in 0..4 {
            a.turns.push(two_lines());
            if a.record(turn, ran_with_a_failure()) {
                asked += 1;
            }
        }
        assert_eq!(asked, AUTO_FIXES);
        a.fixes = 0; // what the user's own request does (`ask`)
        a.turns.push(two_lines());
        assert!(a.record(4, ran_with_a_failure()));
    }

    #[test]
    fn an_undone_turn_says_so_and_offers_no_fix() {
        let mut a = Assistant::new("https://s".into(), "apts_x".into());
        a.turns.push(two_lines());
        a.record(0, ran_with_a_failure());
        a.undone(0, Ok(1));
        assert!(!a.failed(0));
        assert!(matches!(a.turns.last(), Some(Turn::Note(n)) if n.contains("1 step taken back")));
    }

    #[test]
    fn a_long_result_is_cut_short() {
        assert_eq!(shorten("abc"), "abc");
        assert_eq!(
            shorten(&"x".repeat(RESULT_CHARS + 50)).chars().count(),
            RESULT_CHARS + 1
        );
    }
}

//! The changes setup wants in each config file, and the code that applies, reverts and checks them without
//! touching anything else in the file (8).

use std::path::PathBuf;

use anyhow::Result;
use toml_edit::{DocumentMut, Item, Table, Value};

use super::Client;
use super::fsx::{Written, sha256_hex};
use super::json::{Json, Parsed, Style, parse_config};
use super::manifest::{Entry, Manifest, Op};

/// What kind of change a [`Want`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The object member (JSON) or key (TOML) at `path` holds `value`.
    Set,
    /// The JSON array at `path` contains `value`.
    Append,
    /// The whole file is `value` (a string).
    File,
}

/// One change setup wants.
#[derive(Clone, Debug, PartialEq)]
pub struct Want {
    /// Stable id, recorded in the manifest.
    pub id: String,
    pub kind: Kind,
    pub path: Vec<String>,
    pub value: Json,
    /// Words for messages: "the SessionStart hook".
    pub label: String,
    /// For `Set`: an existing object holding at least these members counts as present (a client may add fields).
    pub subset_ok: bool,
    /// Said instead of the generic note when a container on the path has the wrong type.
    pub blocked_note: Option<String>,
}

impl Want {
    /// A `Set` change.
    pub fn set(
        id: impl Into<String>,
        path: &[&str],
        value: Json,
        label: impl Into<String>,
    ) -> Want {
        Want {
            id: id.into(),
            kind: Kind::Set,
            path: path.iter().map(|s| s.to_string()).collect(),
            value,
            label: label.into(),
            subset_ok: false,
            blocked_note: None,
        }
    }

    /// An `Append` change.
    pub fn append(
        id: impl Into<String>,
        path: &[&str],
        value: Json,
        label: impl Into<String>,
    ) -> Want {
        Want {
            kind: Kind::Append,
            ..Want::set(id, path, value, label)
        }
    }

    /// A whole-file change.
    pub fn file(
        id: impl Into<String>,
        content: impl Into<String>,
        label: impl Into<String>,
    ) -> Want {
        Want {
            kind: Kind::File,
            ..Want::set(id, &[], Json::String(content.into()), label)
        }
    }
}

/// How a file is read and written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Json,
    Toml,
    Text,
}

/// The changes for one file.
#[derive(Clone, Debug)]
pub struct FilePlan {
    pub client: Client,
    pub file: PathBuf,
    pub format: Format,
    pub wants: Vec<Want>,
    /// Changes made only when setup creates the file (OpenCode's `$schema`).
    pub on_create: Vec<Want>,
    /// Never rewrite this file (an `opencode.jsonc`): print the block to add instead (8.4).
    pub never_write: bool,
}

/// One change made or planned.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    /// False for a first-time add, true for replacing setup's own earlier value.
    pub update: bool,
    pub label: String,
}

impl Change {
    /// "added the Stop hook" or "updated ...".
    pub fn past(&self) -> String {
        format!(
            "{} {}",
            if self.update { "updated" } else { "added" },
            self.label
        )
    }

    /// "add the Stop hook" or "update ...", for a dry run.
    pub fn planned(&self) -> String {
        format!(
            "{} {}",
            if self.update { "update" } else { "add" },
            self.label
        )
    }
}

/// What applying a plan to the current text did.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    pub changes: Vec<Change>,
    /// What was left alone and why; shown to the person.
    pub notes: Vec<String>,
    /// Manifest entries for the changes (recorded only once the file is written).
    pub entries: Vec<Entry>,
}

impl Outcome {
    fn note(note: String) -> Outcome {
        Outcome {
            notes: vec![note],
            ..Outcome::default()
        }
    }

    fn push_note(&mut self, note: String) {
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }
}

/// Applies `plan` to the current text of its file (`None` when missing). Returns the new text, or `None` when
/// nothing changes or the file must be left alone (the outcome's notes say why).
pub fn apply(
    plan: &FilePlan,
    manifest: &Manifest,
    current: Option<&str>,
) -> Result<(Option<String>, Outcome)> {
    let shown = plan.file.display();
    match plan.format {
        Format::Json => {
            let (mut doc, style, creating) = match current {
                None => (Json::object(), Style::default(), true),
                Some(text) => match parse_config(text) {
                    Parsed::Plain(v) if v.is_object() && !plan.never_write => {
                        (v, Style::detect(text), false)
                    }
                    Parsed::Plain(v) if !v.is_object() => {
                        return Ok((
                            None,
                            Outcome::note(format!("{shown} isn't a JSON object; left it alone")),
                        ));
                    }
                    Parsed::Plain(v) | Parsed::Jsonc(v) => {
                        let missing: Vec<&Want> =
                            plan.wants.iter().filter(|w| !json_has(&v, w)).collect();
                        if missing.is_empty() {
                            return Ok((None, Outcome::default()));
                        }
                        return Ok((
                            None,
                            Outcome::note(format!(
                                "{shown} may hold comments (JSONC), so setup doesn't rewrite it. To finish wiring \
                                 {}, add this to it yourself:\n{}",
                                plan.client,
                                block_for(&missing)
                            )),
                        ));
                    }
                    Parsed::Invalid(e) => {
                        return Ok((
                            None,
                            Outcome::note(format!("{shown} doesn't parse ({e}); left it alone")),
                        ));
                    }
                },
            };
            let mut out = Outcome::default();
            let wants: Vec<&Want> = if creating {
                plan.on_create.iter().chain(&plan.wants).collect()
            } else {
                plan.wants.iter().collect()
            };
            for w in wants {
                apply_json(&mut doc, w, plan, manifest, &mut out);
            }
            let text = (!out.entries.is_empty()).then(|| style.render(&doc));
            Ok((text, out))
        }
        Format::Toml => {
            let mut doc = match current.unwrap_or("").parse::<DocumentMut>() {
                Ok(d) => d,
                Err(e) => {
                    let first = e.to_string().lines().next().unwrap_or("").to_string();
                    return Ok((
                        None,
                        Outcome::note(format!("{shown} doesn't parse ({first}); left it alone")),
                    ));
                }
            };
            let mut out = Outcome::default();
            for w in &plan.wants {
                apply_toml(&mut doc, w, plan, manifest, &mut out);
            }
            let text = (!out.entries.is_empty()).then(|| doc.to_string());
            Ok((text, out))
        }
        Format::Text => {
            let mut out = Outcome::default();
            let mut text = None;
            for w in &plan.wants {
                if let Some(t) = apply_file(current, w, plan, manifest, &mut out) {
                    text = Some(t);
                }
            }
            Ok((text, out))
        }
    }
}

/// Fills in what the write did (backup, created file and folders) on the entries an outcome produced.
pub fn finish_entries(entries: &mut [Entry], written: &Written) {
    for e in entries {
        e.target = written.target.clone();
        if written.backup.is_some() {
            e.backup = written.backup.clone();
        }
        if written.created_file {
            e.created_file = true;
            e.created_dirs = written.created_dirs.clone();
        }
    }
}

fn entry_for(
    w: &Want,
    plan: &FilePlan,
    op: Op,
    value: serde_json::Value,
    created: Vec<Vec<String>>,
) -> Entry {
    Entry {
        id: w.id.clone(),
        client: plan.client,
        file: plan.file.clone(),
        op,
        path: w.path.clone(),
        value,
        created,
        created_file: false,
        created_dirs: Vec::new(),
        backup: None,
        target: None,
    }
}

fn present(w: &Want, cur: &Json) -> bool {
    if w.subset_ok {
        w.value.is_subset_of(cur)
    } else {
        &w.value == cur
    }
}

/// Whether a JSON document already holds a want.
pub fn json_has(doc: &Json, w: &Want) -> bool {
    match w.kind {
        Kind::Set => doc.at(&w.path).is_some_and(|cur| present(w, cur)),
        Kind::Append => doc
            .at(&w.path)
            .and_then(Json::as_array)
            .is_some_and(|a| a.contains(&w.value)),
        Kind::File => false,
    }
}

fn apply_json(doc: &mut Json, w: &Want, plan: &FilePlan, manifest: &Manifest, out: &mut Outcome) {
    let shown = plan.file.display();
    let recorded = manifest.find(&w.id, &plan.file);
    let ours = recorded.map(|e| Json::from_value(&e.value));
    let op = if w.kind == Kind::Append {
        Op::JsonAppend
    } else {
        Op::JsonSet
    };
    let blocked = |what: &str| {
        w.blocked_note.clone().unwrap_or_else(|| {
            format!(
                "{shown}: can't add {}, because {what} isn't an object; left it alone",
                w.label
            )
        })
    };
    // Where setup's own earlier value sits, when it is still there and differs from the wanted one.
    enum State {
        Present,
        Replace(Option<usize>),
        Foreign(String),
        Absent,
    }
    let state = match (w.kind, doc.at(&w.path)) {
        (Kind::File, _) => return,
        (_, None) => State::Absent,
        (Kind::Set, Some(cur)) if present(w, cur) => State::Present,
        (Kind::Set, Some(cur)) if ours.as_ref() == Some(cur) => State::Replace(None),
        (Kind::Set, Some(_)) => State::Foreign(format!(
            "{shown}: {} is already set to something else; left it alone",
            w.path.join(".")
        )),
        (Kind::Append, Some(cur)) => match cur.as_array() {
            None => State::Foreign(w.blocked_note.clone().unwrap_or_else(|| {
                format!(
                    "{shown}: {} isn't a list, so {} can't be added; left it alone",
                    w.path.join("."),
                    w.label
                )
            })),
            Some(arr) if arr.contains(&w.value) => State::Present,
            Some(arr) => match ours
                .as_ref()
                .and_then(|old| arr.iter().position(|v| v == old))
            {
                Some(i) => State::Replace(Some(i)),
                None => State::Absent,
            },
        },
    };
    match state {
        State::Present => {}
        State::Foreign(note) => out.push_note(note),
        State::Replace(index) => {
            let slot = match index {
                None => doc.at_mut(&w.path),
                // In place: Codex ties a hook's trust to its position.
                Some(i) => doc
                    .at_mut(&w.path)
                    .and_then(Json::as_array_mut)
                    .and_then(|a| a.get_mut(i)),
            };
            if let Some(slot) = slot {
                *slot = w.value.clone();
            }
            let mut e = recorded
                .cloned()
                .unwrap_or_else(|| entry_for(w, plan, op, serde_json::Value::Null, vec![]));
            e.value = w.value.to_value();
            out.entries.push(e);
            out.changes.push(Change {
                update: true,
                label: w.label.clone(),
            });
        }
        State::Absent => {
            let (container_path, key) = match w.kind {
                Kind::Set => match w.path.split_last() {
                    Some((key, parent)) => (parent, Some(key)),
                    None => return,
                },
                _ => (w.path.as_slice(), None),
            };
            let Some((container, created)) = make_containers(doc, container_path, key.is_none())
            else {
                out.push_note(blocked(&container_path.join(".")));
                return;
            };
            match key {
                Some(key) => {
                    container.insert(key, w.value.clone());
                }
                None => {
                    if let Some(a) = container.as_array_mut() {
                        a.push(w.value.clone());
                    }
                }
            }
            out.entries
                .push(entry_for(w, plan, op, w.value.to_value(), created));
            out.changes.push(Change {
                update: false,
                label: w.label.clone(),
            });
        }
    }
}

/// Walks `path` from the root, creating missing objects (and, with `last_array`, a final array). Returns the
/// container at the end and the paths it created, or `None` when something on the way has the wrong type.
fn make_containers<'a>(
    doc: &'a mut Json,
    path: &[String],
    last_array: bool,
) -> Option<(&'a mut Json, Vec<Vec<String>>)> {
    let mut created = Vec::new();
    let mut cur = doc;
    for (i, seg) in path.iter().enumerate() {
        let is_last = i + 1 == path.len();
        let want_array = is_last && last_array;
        if cur.get(seg).is_none() {
            if !cur.is_object() {
                return None;
            }
            cur.insert(
                seg,
                if want_array {
                    Json::Array(Vec::new())
                } else {
                    Json::object()
                },
            );
            created.push(path[..=i].to_vec());
        }
        cur = cur.get_mut(seg)?;
        let ok = if want_array {
            cur.as_array().is_some()
        } else {
            cur.is_object()
        };
        if !ok {
            return None;
        }
    }
    Some((cur, created))
}

fn apply_toml(
    doc: &mut DocumentMut,
    w: &Want,
    plan: &FilePlan,
    manifest: &Manifest,
    out: &mut Outcome,
) {
    let shown = plan.file.display();
    let Some((key, parent)) = w.path.split_last() else {
        return;
    };
    let ours = manifest
        .find(&w.id, &plan.file)
        .map(|e| Json::from_value(&e.value));
    let Some(new_item) = json_to_toml(&w.value) else {
        out.notes
            .push(format!("{shown}: {} can't be written as TOML", w.label));
        return;
    };
    let mut created = Vec::new();
    let mut table = doc.as_table_mut();
    for (i, seg) in parent.iter().enumerate() {
        if !table.contains_key(seg) {
            let mut t = Table::new();
            t.set_implicit(true);
            table.insert(seg, Item::Table(t));
            created.push(parent[..=i].to_vec());
        }
        match table.get_mut(seg).and_then(Item::as_table_mut) {
            Some(t) => table = t,
            None => {
                out.notes.push(format!(
                    "{shown}: can't add {}, because {} isn't a table; left it alone",
                    w.label,
                    parent[..=i].join(".")
                ));
                return;
            }
        }
    }
    match table.get(key).map(toml_to_json) {
        Some(Some(cur)) if present(w, &cur) => {}
        Some(cur) => {
            if cur.is_some() && cur == ours {
                table.insert(key, new_item);
                let mut e = manifest
                    .find(&w.id, &plan.file)
                    .cloned()
                    .unwrap_or_else(|| {
                        entry_for(w, plan, Op::TomlSet, serde_json::Value::Null, Vec::new())
                    });
                e.value = w.value.to_value();
                out.entries.push(e);
                out.changes.push(Change {
                    update: true,
                    label: w.label.clone(),
                });
            } else {
                out.notes.push(format!(
                    "{shown}: {} is already set to something else; left it alone",
                    w.path.join(".")
                ));
            }
        }
        None => {
            table.insert(key, new_item);
            out.entries
                .push(entry_for(w, plan, Op::TomlSet, w.value.to_value(), created));
            out.changes.push(Change {
                update: false,
                label: w.label.clone(),
            });
        }
    }
}

fn apply_file(
    current: Option<&str>,
    w: &Want,
    plan: &FilePlan,
    manifest: &Manifest,
    out: &mut Outcome,
) -> Option<String> {
    let content = w.value.as_str()?;
    let value = serde_json::json!({ "sha256": sha256_hex(content.as_bytes()) });
    match current {
        Some(cur) if cur == content => None,
        Some(cur) => {
            let e = manifest.find(&w.id, &plan.file);
            let ours = e
                .and_then(|e| e.value.get("sha256"))
                .and_then(|v| v.as_str());
            if ours == Some(sha256_hex(cur.as_bytes()).as_str()) {
                let mut e = e
                    .cloned()
                    .unwrap_or_else(|| entry_for(w, plan, Op::File, value.clone(), Vec::new()));
                e.value = value;
                out.entries.push(e);
                out.changes.push(Change {
                    update: true,
                    label: w.label.clone(),
                });
                Some(content.to_string())
            } else {
                out.notes.push(format!(
                    "{} already exists and setup didn't write it; left it alone",
                    plan.file.display()
                ));
                None
            }
        }
        None => {
            out.entries
                .push(entry_for(w, plan, Op::File, value, Vec::new()));
            out.changes.push(Change {
                update: false,
                label: w.label.clone(),
            });
            Some(content.to_string())
        }
    }
}

/// Which wants of a plan are missing from the file's current text (`Err` when it can't be read as its format).
pub fn missing<'a>(plan: &'a FilePlan, current: Option<&str>) -> Result<Vec<&'a Want>, String> {
    match plan.format {
        Format::Json => {
            let doc = match current {
                None => Json::object(),
                Some(text) => match parse_config(text) {
                    Parsed::Plain(v) | Parsed::Jsonc(v) => v,
                    Parsed::Invalid(e) => {
                        return Err(format!("{} doesn't parse ({e})", plan.file.display()));
                    }
                },
            };
            Ok(plan.wants.iter().filter(|w| !json_has(&doc, w)).collect())
        }
        Format::Toml => {
            let doc = current.unwrap_or("").parse::<DocumentMut>().map_err(|e| {
                format!(
                    "{} doesn't parse ({})",
                    plan.file.display(),
                    e.to_string().trim()
                )
            })?;
            Ok(plan
                .wants
                .iter()
                .filter(|w| {
                    let item = w
                        .path
                        .iter()
                        .try_fold(doc.as_item(), |item, k| item.as_table()?.get(k));
                    !item
                        .and_then(toml_to_json)
                        .is_some_and(|cur| present(w, &cur))
                })
                .collect())
        }
        Format::Text => Ok(plan
            .wants
            .iter()
            .filter(|w| w.value.as_str() != current)
            .collect()),
    }
}

/// The JSON block a person adds by hand to a file setup won't rewrite, holding the given wants.
pub fn block_for(wants: &[&Want]) -> String {
    let mut doc = Json::object();
    for w in wants {
        match w.kind {
            Kind::Set => {
                if let Some((key, parent)) = w.path.split_last()
                    && let Some((c, _)) = make_containers(&mut doc, parent, false)
                {
                    c.insert(key, w.value.clone());
                }
            }
            Kind::Append => {
                if let Some((c, _)) = make_containers(&mut doc, &w.path, true)
                    && let Some(a) = c.as_array_mut()
                {
                    a.push(w.value.clone());
                }
            }
            Kind::File => {}
        }
    }
    Style::default().render(&doc).trim_end().to_string()
}

/// What reverting a file's entries did.
#[derive(Clone, Debug, Default)]
pub struct Reverted {
    /// Ids of entries undone (or already gone).
    pub undone: Vec<String>,
    /// Ids of entries left alone, with the reason.
    pub left: Vec<(String, String)>,
    /// The file should be deleted (setup created it and nothing else is in it).
    pub delete: bool,
}

/// Reverts `entries` (all for the same file, in the order they were recorded) on the current text. Returns the new
/// text when it changed.
pub fn revert(
    format: Format,
    entries: &[&Entry],
    current: Option<&str>,
) -> (Option<String>, Reverted) {
    let mut out = Reverted::default();
    let Some(text) = current else {
        out.undone = entries.iter().map(|e| e.id.clone()).collect();
        return (None, out);
    };
    let created_file = entries.iter().any(|e| e.created_file);
    match format {
        Format::Json => {
            let mut doc = match parse_config(text) {
                Parsed::Plain(v) if v.is_object() => v,
                _ => {
                    for e in entries {
                        out.left.push((
                            e.id.clone(),
                            "the file no longer parses as plain JSON".into(),
                        ));
                    }
                    return (None, out);
                }
            };
            let style = Style::detect(text);
            let mut changed = false;
            for e in entries.iter().rev() {
                match revert_json(&mut doc, e) {
                    Ok(true) => {
                        changed = true;
                        out.undone.push(e.id.clone());
                    }
                    Ok(false) => out.undone.push(e.id.clone()),
                    Err(why) => out.left.push((e.id.clone(), why)),
                }
            }
            if created_file && doc.is_empty_container() {
                out.delete = true;
            }
            (changed.then(|| style.render(&doc)), out)
        }
        Format::Toml => {
            let Ok(mut doc) = text.parse::<DocumentMut>() else {
                for e in entries {
                    out.left
                        .push((e.id.clone(), "the file no longer parses as TOML".into()));
                }
                return (None, out);
            };
            let mut changed = false;
            for e in entries.iter().rev() {
                match revert_toml(&mut doc, e) {
                    Ok(true) => {
                        changed = true;
                        out.undone.push(e.id.clone());
                    }
                    Ok(false) => out.undone.push(e.id.clone()),
                    Err(why) => out.left.push((e.id.clone(), why)),
                }
            }
            if created_file && doc.as_table().is_empty() && doc.to_string().trim().is_empty() {
                out.delete = true;
            }
            (changed.then(|| doc.to_string()), out)
        }
        Format::Text => {
            for e in entries {
                let ours = e.value.get("sha256").and_then(|v| v.as_str());
                if ours == Some(sha256_hex(text.as_bytes()).as_str()) {
                    out.delete = true;
                    out.undone.push(e.id.clone());
                } else {
                    out.left
                        .push((e.id.clone(), "the file changed since setup wrote it".into()));
                }
            }
            (None, out)
        }
    }
}

/// Undoes one JSON entry. `Ok(true)` when it changed the document, `Ok(false)` when there was nothing to undo,
/// `Err` with the reason when the current value isn't what setup wrote.
fn revert_json(doc: &mut Json, e: &Entry) -> Result<bool, String> {
    let ours = Json::from_value(&e.value);
    match e.op {
        Op::JsonSet | Op::ClaudeCli => {
            let Some((key, parent)) = e.path.split_last() else {
                return Ok(false);
            };
            match doc.at(&e.path) {
                None => return Ok(false),
                Some(cur) if *cur == ours => {}
                Some(_) => {
                    return Err(format!("{} changed since setup wrote it", e.path.join(".")));
                }
            }
            if let Some(p) = doc.at_mut(parent) {
                p.remove(key);
            }
        }
        Op::JsonAppend => {
            let Some(arr) = doc.at_mut(&e.path).and_then(Json::as_array_mut) else {
                return Err(format!(
                    "{} is gone or changed since setup",
                    e.path.join(".")
                ));
            };
            let Some(i) = arr.iter().position(|v| *v == ours) else {
                return Err(format!(
                    "setup's entry in {} changed or was removed since",
                    e.path.join(".")
                ));
            };
            arr.remove(i);
        }
        Op::TomlSet | Op::File => return Ok(false),
    }
    for c in e.created.iter().rev() {
        if let Some((key, parent)) = c.split_last()
            && doc.at(c).is_some_and(Json::is_empty_container)
            && let Some(p) = doc.at_mut(parent)
        {
            p.remove(key);
        }
    }
    Ok(true)
}

fn revert_toml(doc: &mut DocumentMut, e: &Entry) -> Result<bool, String> {
    let Some((key, parent)) = e.path.split_last() else {
        return Ok(false);
    };
    let ours = Json::from_value(&e.value);
    let table = parent.iter().try_fold(doc.as_table_mut(), |t, k| {
        t.get_mut(k).and_then(Item::as_table_mut)
    });
    let Some(table) = table else { return Ok(false) };
    match table.get(key).map(toml_to_json) {
        None => return Ok(false),
        Some(Some(cur)) if cur == ours => {}
        Some(_) => return Err(format!("{} changed since setup wrote it", e.path.join("."))),
    }
    table.remove(key);
    for c in e.created.iter().rev() {
        let Some((key, parent)) = c.split_last() else {
            continue;
        };
        let t = parent.iter().try_fold(doc.as_table_mut(), |t, k| {
            t.get_mut(k).and_then(Item::as_table_mut)
        });
        if let Some(t) = t
            && t.get(key)
                .and_then(Item::as_table)
                .is_some_and(Table::is_empty)
        {
            t.remove(key);
        }
    }
    Ok(true)
}

/// Converts a TOML item to JSON for comparison (tables and inline tables alike). `None` for datetimes and the like.
pub fn toml_to_json(item: &Item) -> Option<Json> {
    match item {
        Item::None => None,
        Item::Value(v) => toml_value_to_json(v),
        Item::Table(t) => {
            let mut obj = Json::object();
            for (k, v) in t.iter() {
                obj.insert(k, toml_to_json(v)?);
            }
            Some(obj)
        }
        Item::ArrayOfTables(a) => {
            let mut out = Vec::new();
            for t in a.iter() {
                out.push(toml_to_json(&Item::Table(t.clone()))?);
            }
            Some(Json::Array(out))
        }
    }
}

fn toml_value_to_json(v: &Value) -> Option<Json> {
    Some(match v {
        Value::String(s) => Json::String(s.value().clone()),
        Value::Integer(i) => Json::int(*i.value()),
        Value::Float(f) => Json::Number(serde_json::Number::from_f64(*f.value())?),
        Value::Boolean(b) => Json::Bool(*b.value()),
        Value::Datetime(_) => return None,
        Value::Array(a) => Json::Array(
            a.iter()
                .map(toml_value_to_json)
                .collect::<Option<Vec<_>>>()?,
        ),
        Value::InlineTable(t) => {
            let mut obj = Json::object();
            for (k, v) in t.iter() {
                obj.insert(k, toml_value_to_json(v)?);
            }
            obj
        }
    })
}

/// Converts JSON to a TOML item: objects become tables (nested objects sub-tables), the rest values.
fn json_to_toml(v: &Json) -> Option<Item> {
    match v {
        Json::Object(m) => {
            let mut t = Table::new();
            for (k, v) in m {
                t.insert(k, json_to_toml(v)?);
            }
            Some(Item::Table(t))
        }
        other => Some(Item::Value(json_to_toml_value(other)?)),
    }
}

fn json_to_toml_value(v: &Json) -> Option<Value> {
    Some(match v {
        Json::Null => return None,
        Json::Bool(b) => Value::from(*b),
        Json::Number(n) => match n.as_i64() {
            Some(i) => Value::from(i),
            None => Value::from(n.as_f64()?),
        },
        Json::String(s) => Value::from(s.as_str()),
        Json::Array(a) => Value::Array(
            a.iter()
                .map(json_to_toml_value)
                .collect::<Option<toml_edit::Array>>()?,
        ),
        Json::Object(m) => {
            let mut t = toml_edit::InlineTable::new();
            for (k, v) in m {
                t.insert(k, json_to_toml_value(v)?);
            }
            Value::InlineTable(t)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(format: Format, wants: Vec<Want>) -> FilePlan {
        FilePlan {
            client: Client::Claude,
            file: PathBuf::from("/home/sam/.claude/settings.json"),
            format,
            wants,
            on_create: vec![],
            never_write: false,
        }
    }

    fn run(p: &FilePlan, m: &mut Manifest, text: Option<&str>) -> (Option<String>, Outcome) {
        let (t, out) = apply(p, m, text).unwrap();
        if t.is_some() {
            for e in &out.entries {
                m.upsert(e.clone());
            }
        }
        (t, out)
    }

    #[test]
    fn json_set_and_append_keep_other_entries_and_revert_cleanly() {
        let original = "{\n  \"model\": \"x\",\n  \"permissions\": {\n    \"allow\": [\n      \"Bash(ls)\"\n    ]\n  }\n}\n";
        let p = plan(
            Format::Json,
            vec![
                Want::append(
                    "perm",
                    &["permissions", "allow"],
                    Json::str("mcp__mesh__who"),
                    "who",
                ),
                Want::set(
                    "srv",
                    &["mcpServers", "mesh"],
                    Json::obj([("command", Json::str("/x"))]),
                    "srv",
                ),
            ],
        );
        let mut m = Manifest::default();
        let (text, out) = run(&p, &mut m, Some(original));
        let text = text.unwrap();
        assert_eq!(out.changes.len(), 2);
        assert!(text.starts_with("{\n  \"model\": \"x\",\n  \"permissions\""));
        assert!(text.contains("\"Bash(ls)\",\n      \"mcp__mesh__who\""));
        let (again, out2) = run(&p, &mut m, Some(&text));
        assert!(again.is_none() && out2.changes.is_empty() && out2.notes.is_empty());

        let entries: Vec<&Entry> = m.entries.iter().collect();
        let (back, rev) = revert(Format::Json, &entries, Some(&text));
        assert_eq!(back.unwrap(), original);
        assert!(rev.left.is_empty() && !rev.delete);
    }

    #[test]
    fn a_foreign_value_is_left_alone_and_ours_is_updated_in_place() {
        let p1 = plan(
            Format::Json,
            vec![Want::set(
                "srv",
                &["mcpServers", "mesh"],
                Json::str("v1"),
                "srv",
            )],
        );
        let mut m = Manifest::default();
        let (none, out) = run(
            &p1,
            &mut m,
            Some("{\"mcpServers\": {\"mesh\": \"theirs\"}}"),
        );
        assert!(none.is_none());
        assert!(
            out.notes[0].contains("already set to something else"),
            "{:?}",
            out.notes
        );

        let (t1, _) = run(&p1, &mut m, Some("{\"a\": [1]}"));
        let p2 = plan(
            Format::Json,
            vec![Want::set(
                "srv",
                &["mcpServers", "mesh"],
                Json::str("v2"),
                "srv",
            )],
        );
        let (t2, out) = run(&p2, &mut m, t1.as_deref());
        assert_eq!(
            out.changes,
            vec![Change {
                update: true,
                label: "srv".into()
            }]
        );
        assert!(t2.unwrap().contains("\"v2\""));
        assert_eq!(m.entries[0].value, serde_json::json!("v2"));
        assert_eq!(
            m.entries[0].created,
            vec![vec!["mcpServers".to_string()]],
            "kept from the first write"
        );
    }

    #[test]
    fn a_wrong_container_type_is_reported() {
        let p = plan(
            Format::Json,
            vec![Want::append(
                "h",
                &["hooks", "Stop"],
                Json::int(1),
                "the Stop hook",
            )],
        );
        let (t, out) = apply(&p, &Manifest::default(), Some("{\"hooks\": []}")).unwrap();
        assert!(t.is_none());
        assert!(
            out.notes[0].contains("can't add the Stop hook"),
            "{:?}",
            out.notes
        );
    }

    #[test]
    fn revert_leaves_changed_values_and_reports_them() {
        let p = plan(
            Format::Json,
            vec![Want::set("k", &["a"], Json::int(1), "a")],
        );
        let mut m = Manifest::default();
        run(&p, &mut m, Some("{}"));
        let entries: Vec<&Entry> = m.entries.iter().collect();
        let (t, rev) = revert(Format::Json, &entries, Some("{\"a\": 2}"));
        assert!(t.is_none());
        assert_eq!(rev.left.len(), 1);
    }

    #[test]
    fn toml_tables_round_trip_with_comments_kept() {
        let original =
            "# my settings\nmodel = \"o4\" # keep me\n\n[mcp_servers.other]\ncommand = \"other\"\n";
        let value = Json::obj([
            ("command", Json::str("/opt/spew")),
            ("args", Json::strings(&["mesh", "mcp-server"])),
            ("required", Json::Bool(false)),
            ("env", Json::obj([("SPEW_MESH_CLIENT", Json::str("codex"))])),
        ]);
        let mut p = plan(
            Format::Toml,
            vec![Want::set(
                "mcp",
                &["mcp_servers", "mesh"],
                value,
                "the MCP server",
            )],
        );
        p.file = PathBuf::from("/home/sam/.codex/config.toml");
        let mut m = Manifest::default();
        let (text, _) = run(&p, &mut m, Some(original));
        let text = text.unwrap();
        assert!(text.starts_with(original), "{text}");
        assert!(
            text.contains("[mcp_servers.mesh]\ncommand = \"/opt/spew\""),
            "{text}"
        );
        assert!(
            text.contains("[mcp_servers.mesh.env]\nSPEW_MESH_CLIENT = \"codex\""),
            "{text}"
        );
        let (again, _) = run(&p, &mut m, Some(&text));
        assert!(again.is_none());
        let entries: Vec<&Entry> = m.entries.iter().collect();
        let (back, rev) = revert(Format::Toml, &entries, Some(&text));
        assert!(rev.left.is_empty());
        assert_eq!(back.unwrap(), original);
    }

    #[test]
    fn toml_into_an_empty_file_creates_and_removes_the_parent() {
        let mut p = plan(
            Format::Toml,
            vec![Want::set(
                "mcp",
                &["mcp_servers", "mesh"],
                Json::obj([("a", Json::int(1))]),
                "x",
            )],
        );
        p.file = PathBuf::from("/home/sam/.codex/config.toml");
        let mut m = Manifest::default();
        let (text, _) = run(&p, &mut m, None);
        let text = text.unwrap();
        assert_eq!(text.trim(), "[mcp_servers.mesh]\na = 1");
        let mut entries: Vec<Entry> = m.entries.clone();
        entries[0].created_file = true;
        let refs: Vec<&Entry> = entries.iter().collect();
        let (back, rev) = revert(Format::Toml, &refs, Some(&text));
        assert_eq!(back.unwrap().trim(), "");
        assert!(rev.delete);
    }

    #[test]
    fn whole_files_are_written_once_and_only_removed_when_unchanged() {
        let mut p = plan(
            Format::Text,
            vec![Want::file("skill", "hello\n", "the skill")],
        );
        p.file = PathBuf::from("/home/sam/.claude/skills/spew-mesh/SKILL.md");
        let mut m = Manifest::default();
        let (t, _) = run(&p, &mut m, None);
        assert_eq!(t.as_deref(), Some("hello\n"));
        assert!(run(&p, &mut m, Some("hello\n")).0.is_none());
        let (t, out) = run(&p, &mut m, Some("edited by sam\n"));
        assert!(t.is_none() && out.notes[0].contains("left it alone"));
        let refs: Vec<&Entry> = m.entries.iter().collect();
        assert!(revert(Format::Text, &refs, Some("hello\n")).1.delete);
        assert!(!revert(Format::Text, &refs, Some("edited\n")).1.delete);
    }

    #[test]
    fn jsonc_is_never_rewritten_and_the_block_is_offered() {
        let p = plan(
            Format::Json,
            vec![Want::set(
                "m",
                &["mcp", "mesh"],
                Json::obj([("enabled", Json::Bool(true))]),
                "m",
            )],
        );
        let (t, out) = apply(
            &p,
            &Manifest::default(),
            Some("{\n // mine\n \"theme\": \"x\",\n}\n"),
        )
        .unwrap();
        assert!(t.is_none());
        assert!(
            out.notes[0].contains("\"mcp\": {\n    \"mesh\": {\n      \"enabled\": true"),
            "{}",
            out.notes[0]
        );
        let (t, out) = apply(
            &p,
            &Manifest::default(),
            Some("{\n // mine\n \"mcp\": {\"mesh\": {\"enabled\": true}},\n}"),
        )
        .unwrap();
        assert!(t.is_none() && out.notes.is_empty(), "already added by hand");
    }
}

//! Tagging: which tab works on which node of the overview.
//!
//! Pure logic for the tagger run, so that `app.rs` only wires it: the
//! watermark stored on the tab's tmux session, the transcript delta since
//! that watermark and its significance, the prompt sent to the model, the
//! parsing of its answer, and the rate-limit rules. Transcript text, sizes
//! and uuids come in as values; the only I/O is `load_config`, kept thin.
//! No GTK, no tmux, nothing from the other overview modules.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// tmux environment key holding the tab's tags as JSON.
pub const ENV_LINKS: &str = "KABELSALAT_LINKS";
/// tmux environment key holding the tab's watermark (`Watermark::render`).
pub const ENV_TAG_MARK: &str = "KABELSALAT_TAG_MARK";
/// Roles when the configuration names none.
pub const DEFAULT_ROLES: [&str; 4] = ["planning", "implementing", "researching", "related"];
/// The transcript tail handed to the model, in characters.
pub const MAX_TRANSCRIPT_CHARS: usize = 40_000;
/// Minimum gap between two runs for the same tab.
pub const MIN_RUN_GAP_SECS: u64 = 120;
/// Period of the scan that looks for changed transcripts.
pub const SCAN_SECS: u32 = 60;
/// How long one tagger process may take.
pub const TIMEOUT_SECS: u64 = 60;

/// What the model says a tab works on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Tags {
    #[serde(default)]
    pub links: Vec<TagLink>,
    #[serde(default)]
    pub activity: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
}

/// One link from a tab to a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagLink {
    pub node: String,
    pub role: String,
}

/// `$XDG_CONFIG_HOME/kabelsalat/overview.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub tagger_command: Vec<String>,
    pub roles: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            tagger_command: [
                "claude",
                "-p",
                "--model",
                "haiku",
                "--output-format",
                "json",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            roles: DEFAULT_ROLES.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// `config_home/kabelsalat/overview.json`.
pub fn config_path(config_home: &Path) -> PathBuf {
    config_home.join("kabelsalat").join("overview.json")
}

/// Parses the configuration file. Missing keys take their defaults; unknown
/// keys are ignored; `tagger_command` must be a non-empty array of strings
/// and `roles` a non-empty array of strings.
pub fn parse_config(text: &str) -> Result<Config, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "the configuration must be a JSON object".to_string())?;
    let mut config = Config::default();
    if let Some(command) = object.get("tagger_command") {
        config.tagger_command = string_array(command, "tagger_command")?;
    }
    if let Some(roles) = object.get("roles") {
        config.roles = string_array(roles, "roles")?;
    }
    Ok(config)
}

fn string_array(value: &serde_json::Value, key: &str) -> Result<Vec<String>, String> {
    let items = value
        .as_array()
        .ok_or_else(|| format!("'{key}' must be an array of strings"))?;
    if items.is_empty() {
        return Err(format!("'{key}' must not be empty"));
    }
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("'{key}' must contain only strings"))
        })
        .collect()
}

/// Reads the configuration file; a missing file means defaults, an
/// unreadable or invalid one means defaults and a line on stderr.
pub fn load_config(path: &Path) -> Config {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Config::default(),
        Err(err) => {
            eprintln!("kabelsalat: cannot read {}: {err}", path.display());
            return Config::default();
        }
    };
    match parse_config(&text) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("kabelsalat: ignoring {}: {err}", path.display());
            Config::default()
        }
    }
}

/// Where the last run left off, stored on the tmux session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Watermark {
    /// The last transcript line the model saw and the transcript's byte
    /// length at that time.
    Transcript { uuid: String, offset: u64 },
    /// The tab title the model saw, for tabs without a transcript.
    Title(String),
}

const TITLE_PREFIX: &str = "title:";

impl Watermark {
    /// Reads `<uuid>:<offset>` or `title:<title>`.
    pub fn parse(s: &str) -> Option<Watermark> {
        if let Some(title) = s.strip_prefix(TITLE_PREFIX) {
            return Some(Watermark::Title(title.to_string()));
        }
        let (uuid, offset) = s.rsplit_once(':')?;
        if uuid.is_empty() {
            return None;
        }
        let offset = offset.parse::<u64>().ok()?;
        Some(Watermark::Transcript {
            uuid: uuid.to_string(),
            offset,
        })
    }

    pub fn render(&self) -> String {
        match self {
            Watermark::Transcript { uuid, offset } => format!("{uuid}:{offset}"),
            Watermark::Title(title) => format!("{TITLE_PREFIX}{title}"),
        }
    }
}

/// Cheap check before reading a transcript: `true` when its size and last
/// uuid both match the mark, so nothing new can be in it.
pub fn unchanged(mark: Option<&Watermark>, size: u64, last_uuid: Option<&str>) -> bool {
    match mark {
        Some(Watermark::Transcript { uuid, offset }) => {
            *offset == size && last_uuid == Some(uuid.as_str())
        }
        Some(Watermark::Title(_)) | None => false,
    }
}

/// The part of a transcript written since a watermark.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Delta {
    /// `user` messages that are not tool results.
    pub user_prompts: usize,
    pub assistant_messages: usize,
    /// Message text only, as "user: …\n" / "assistant: …\n" lines.
    pub text: String,
    /// The uuid of the last well-formed line in the new part.
    pub last_uuid: Option<String>,
    /// The transcript's byte length.
    pub end_offset: u64,
}

/// One transcript line, as far as tagging reads it.
#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(rename = "isSidechain", default)]
    is_sidechain: bool,
    #[serde(default)]
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    content: serde_json::Value,
}

/// A line's message text and whether it is a human prompt (text written by
/// the user, not a tool result).
struct Said {
    role: &'static str,
    text: String,
    is_prompt: bool,
}

fn parse_line(line: &str) -> Option<Line> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    serde_json::from_str::<Line>(line).ok()
}

/// What the line says, or `None` for lines that are not conversation
/// (summaries, system lines, sidechains, tool results only).
fn said(line: &Line) -> Option<Said> {
    if line.is_sidechain {
        return None;
    }
    let role = match line.kind.as_deref() {
        Some("user") => "user",
        Some("assistant") => "assistant",
        _ => return None,
    };
    let content = line.message.as_ref().map(|m| &m.content)?;
    let (text, has_text) = match content {
        serde_json::Value::String(s) => (s.clone(), true),
        serde_json::Value::Array(blocks) => {
            let texts: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect();
            (texts.join("\n"), !texts.is_empty())
        }
        _ => return None,
    };
    if role == "user" && !has_text {
        // A tool result handed back to the model, not a prompt.
        return None;
    }
    Some(Said {
        role,
        text,
        is_prompt: role == "user",
    })
}

/// Parses the transcript after `mark`: the lines after the one whose uuid
/// the mark names, or the whole transcript when the mark is a title or its
/// uuid is not found.
pub fn delta_since(transcript: &str, mark: Option<&Watermark>) -> Delta {
    let lines: Vec<Option<Line>> = transcript.lines().map(parse_line).collect();
    let start = match mark {
        Some(Watermark::Transcript { uuid, .. }) => lines
            .iter()
            .rposition(|l| l.as_ref().and_then(|l| l.uuid.as_deref()) == Some(uuid))
            .map(|i| i + 1)
            .unwrap_or(0),
        Some(Watermark::Title(_)) | None => 0,
    };
    let mut delta = Delta {
        end_offset: transcript.len() as u64,
        ..Delta::default()
    };
    for line in lines.into_iter().skip(start).flatten() {
        if let Some(uuid) = &line.uuid {
            delta.last_uuid = Some(uuid.clone());
        }
        let Some(said) = said(&line) else { continue };
        if said.is_prompt {
            delta.user_prompts += 1;
        } else {
            delta.assistant_messages += 1;
        }
        if !said.text.is_empty() {
            delta.text.push_str(said.role);
            delta.text.push_str(": ");
            delta.text.push_str(&said.text);
            delta.text.push('\n');
        }
    }
    delta
}

/// Enough new conversation to be worth a model call.
pub fn significant(d: &Delta) -> bool {
    d.user_prompts >= 1 || d.assistant_messages >= 6
}

/// The uuid of the last well-formed line.
pub fn last_uuid(transcript: &str) -> Option<String> {
    transcript
        .lines()
        .rev()
        .filter_map(parse_line)
        .find_map(|line| line.uuid)
}

/// The last `n` characters of `s`, cut on a character boundary.
pub fn tail_chars(s: &str, n: usize) -> &str {
    let count = s.chars().count();
    if count <= n {
        return s;
    }
    let start = s
        .char_indices()
        .nth(count - n)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    &s[start..]
}

/// What the prompt says about one node.
pub struct NodeSummary<'a> {
    pub id: &'a str,
    pub kind: &'a str,
    pub title: &'a str,
}

/// The prompt sent to the tagger's stdin.
pub fn prompt(
    roles: &[String],
    nodes: &[NodeSummary],
    current: &Tags,
    tab_title: &str,
    text: &str,
) -> String {
    let mut p = String::new();
    p.push_str(
        "You classify what a terminal tab is working on, against a list of nodes \
         (the units of a project map). Read the transcript below and answer with \
         ONLY a JSON object, no prose, no Markdown fences, exactly of this shape:\n",
    );
    p.push_str(
        r#"{"links":[{"node":"<node id>","role":"<role>"}],"activity":"<a few words on what the tab is doing right now>","topic":null}"#,
    );
    p.push_str("\n\nRules:\n");
    p.push_str("- \"node\" must be an id from the node list; \"role\" must be one of the roles.\n");
    p.push_str(
        "- Link only the nodes the tab actually works on; one link per node, the most specific nodes first.\n",
    );
    p.push_str(
        "- When the work matches no node, leave \"links\" empty and set \"topic\" to a short phrase naming the work; otherwise set \"topic\" to null.\n",
    );
    p.push_str(
        "- Start from the current links: keep those the transcript still supports, drop the others, add new ones.\n",
    );
    p.push_str("- \"activity\" is a short present-tense phrase, or null when unclear.\n\n");
    p.push_str("Roles: ");
    p.push_str(&roles.join(", "));
    p.push_str("\n\nNodes (id\tkind\ttitle):\n");
    if nodes.is_empty() {
        p.push_str("(none)\n");
    }
    for node in nodes {
        p.push_str(node.id);
        p.push('\t');
        p.push_str(node.kind);
        p.push('\t');
        p.push_str(node.title);
        p.push('\n');
    }
    p.push_str("\nCurrent links: ");
    p.push_str(&serde_json::to_string(current).unwrap_or_else(|_| "{}".to_string()));
    p.push_str("\n\nTab title: ");
    p.push_str(tab_title);
    p.push_str("\n\nTranscript (most recent part, user and assistant text only):\n");
    p.push_str(tail_chars(text, MAX_TRANSCRIPT_CHARS));
    p
}

/// Reads the tagger's stdout: the JSON object itself, or the
/// `claude --output-format json` envelope whose `result` string holds it,
/// possibly wrapped in Markdown fences or prose.
pub fn parse_response(stdout: &str) -> Result<Tags, String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err("empty response".to_string());
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return match value {
            serde_json::Value::Object(ref map) if map.contains_key("result") => {
                match map.get("result") {
                    Some(serde_json::Value::String(result)) => tags_within(result),
                    _ => Err("the response envelope has no result string".to_string()),
                }
            }
            serde_json::Value::Object(_) => tags_from(value),
            _ => Err("the response is not a JSON object".to_string()),
        };
    }
    tags_within(trimmed)
}

/// The tags in the first balanced `{…}` of a text that may hold prose or
/// Markdown fences around it.
fn tags_within(text: &str) -> Result<Tags, String> {
    let object = first_object(text).ok_or_else(|| "no JSON object in the response".to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(object).map_err(|e| format!("invalid JSON in the response: {e}"))?;
    tags_from(value)
}

fn tags_from(value: serde_json::Value) -> Result<Tags, String> {
    let serde_json::Value::Object(mut map) = value else {
        return Err("the response is not a JSON object".to_string());
    };
    // `null` for a list means "none", as a missing key does.
    if map.get("links").is_some_and(|v| v.is_null()) {
        map.remove("links");
    }
    serde_json::from_value(serde_json::Value::Object(map))
        .map_err(|e| format!("unexpected response shape: {e}"))
}

/// The first balanced `{…}` in `text`, honouring JSON strings and escapes.
fn first_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in text[start..].char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + i + c.len_utf8()]);
                }
            }
            _ => {}
        }
    }
    None
}

/// One tagger at a time, and at least `MIN_RUN_GAP_SECS` between runs for
/// the same tab (`None` = never run).
pub fn may_run(last_run_secs_ago: Option<u64>, running_elsewhere: bool) -> bool {
    !running_elsewhere && last_run_secs_ago.is_none_or(|secs| secs >= MIN_RUN_GAP_SECS)
}

/// Input for a tab without a transcript: the title stands in for both the
/// watermark and the text.
pub fn title_input(title: &str) -> (Watermark, String) {
    (
        Watermark::Title(title.to_string()),
        format!("(no transcript; the tab's title is all that is known)\ntitle: {title}\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(kind: &str, uuid: &str, content: &str) -> String {
        format!(
            r#"{{"type":"{kind}","uuid":"{uuid}","message":{{"role":"{kind}","content":{content}}}}}"#
        )
    }

    fn user(uuid: &str, text: &str) -> String {
        line("user", uuid, &format!("\"{text}\""))
    }

    fn assistant(uuid: &str, text: &str) -> String {
        line(
            "assistant",
            uuid,
            &format!(r#"[{{"type":"text","text":"{text}"}}]"#),
        )
    }

    fn tool_result(uuid: &str) -> String {
        line(
            "user",
            uuid,
            r#"[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]"#,
        )
    }

    fn joined(lines: &[String]) -> String {
        let mut s = lines.join("\n");
        s.push('\n');
        s
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kabelsalat-tagger-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // --- watermark ---

    #[test]
    fn transcript_watermark_renders_and_parses_back() {
        let mark = Watermark::Transcript {
            uuid: "9d1c-4e".to_string(),
            offset: 1234,
        };
        assert_eq!(mark.render(), "9d1c-4e:1234");
        assert_eq!(Watermark::parse(&mark.render()), Some(mark));
    }

    #[test]
    fn title_watermark_keeps_colons_in_the_title() {
        let mark = Watermark::Title("claude: fixing tests: round 2".to_string());
        assert_eq!(mark.render(), "title:claude: fixing tests: round 2");
        assert_eq!(Watermark::parse(&mark.render()), Some(mark));
    }

    #[test]
    fn watermark_parse_rejects_garbage() {
        assert_eq!(Watermark::parse(""), None);
        assert_eq!(Watermark::parse("abc"), None);
        assert_eq!(Watermark::parse("abc:"), None);
        assert_eq!(Watermark::parse(":12"), None);
        assert_eq!(Watermark::parse("abc:x"), None);
        assert_eq!(Watermark::parse("abc:-1"), None);
    }

    #[test]
    fn unchanged_when_size_and_last_uuid_match_the_mark() {
        let mark = Watermark::Transcript {
            uuid: "u2".to_string(),
            offset: 40,
        };
        assert!(unchanged(Some(&mark), 40, Some("u2")));
        assert!(!unchanged(Some(&mark), 41, Some("u2")));
        assert!(!unchanged(Some(&mark), 40, Some("u3")));
        assert!(!unchanged(Some(&mark), 40, None));
        assert!(!unchanged(None, 40, Some("u2")));
        assert!(!unchanged(
            Some(&Watermark::Title("x".to_string())),
            40,
            Some("u2")
        ));
    }

    // --- delta ---

    #[test]
    fn a_user_prompt_counts_and_is_rendered_as_text() {
        let t = joined(&[user("u1", "fix the build")]);
        let d = delta_since(&t, None);
        assert_eq!(d.user_prompts, 1);
        assert_eq!(d.assistant_messages, 0);
        assert_eq!(d.text, "user: fix the build\n");
        assert_eq!(d.last_uuid.as_deref(), Some("u1"));
        assert_eq!(d.end_offset, t.len() as u64);
    }

    #[test]
    fn assistant_text_blocks_count_and_render() {
        let t = joined(&[
            user("u1", "hi"),
            assistant("a1", "hello"),
            line(
                "assistant",
                "a2",
                r#"[{"type":"text","text":"first"},{"type":"tool_use","id":"x","name":"Bash","input":{}},{"type":"text","text":"second"}]"#,
            ),
        ]);
        let d = delta_since(&t, None);
        assert_eq!(d.assistant_messages, 2);
        assert_eq!(
            d.text,
            "user: hi\nassistant: hello\nassistant: first\nsecond\n"
        );
        assert_eq!(d.last_uuid.as_deref(), Some("a2"));
    }

    #[test]
    fn tool_result_only_user_lines_are_not_prompts() {
        let t = joined(&[
            tool_result("u1"),
            tool_result("u2"),
            assistant("a1", "done"),
        ]);
        let d = delta_since(&t, None);
        assert_eq!(d.user_prompts, 0);
        assert_eq!(d.assistant_messages, 1);
        assert_eq!(d.text, "assistant: done\n");
    }

    #[test]
    fn user_text_block_counts_as_prompt() {
        let t = joined(&[line(
            "user",
            "u1",
            r#"[{"type":"text","text":"please continue"}]"#,
        )]);
        let d = delta_since(&t, None);
        assert_eq!(d.user_prompts, 1);
        assert_eq!(d.text, "user: please continue\n");
    }

    #[test]
    fn sidechain_lines_are_skipped() {
        let side = r#"{"type":"user","uuid":"s1","isSidechain":true,"message":{"role":"user","content":"subagent task"}}"#.to_string();
        let t = joined(&[side, assistant("a1", "ok")]);
        let d = delta_since(&t, None);
        assert_eq!(d.user_prompts, 0);
        assert_eq!(d.assistant_messages, 1);
        assert_eq!(d.text, "assistant: ok\n");
    }

    #[test]
    fn malformed_and_other_lines_are_skipped() {
        let t = joined(&[
            "not json".to_string(),
            r#"{"type":"summary","summary":"x","leafUuid":"l"}"#.to_string(),
            "{\"type\":\"user\"".to_string(),
            user("u1", "go"),
            String::new(),
        ]);
        let d = delta_since(&t, None);
        assert_eq!(d.user_prompts, 1);
        assert_eq!(d.text, "user: go\n");
        assert_eq!(d.last_uuid.as_deref(), Some("u1"));
    }

    #[test]
    fn delta_starts_after_the_marked_uuid() {
        let t = joined(&[
            user("u1", "one"),
            assistant("a1", "r1"),
            user("u2", "two"),
            assistant("a2", "r2"),
        ]);
        let mark = Watermark::Transcript {
            uuid: "a1".to_string(),
            offset: 10,
        };
        let d = delta_since(&t, Some(&mark));
        assert_eq!(d.user_prompts, 1);
        assert_eq!(d.assistant_messages, 1);
        assert_eq!(d.text, "user: two\nassistant: r2\n");
        assert_eq!(d.last_uuid.as_deref(), Some("a2"));
        assert_eq!(d.end_offset, t.len() as u64);
    }

    #[test]
    fn nothing_after_the_mark_gives_an_empty_delta() {
        let t = joined(&[user("u1", "one"), assistant("a1", "r1")]);
        let mark = Watermark::Transcript {
            uuid: "a1".to_string(),
            offset: 0,
        };
        let d = delta_since(&t, Some(&mark));
        assert_eq!(d.user_prompts, 0);
        assert_eq!(d.assistant_messages, 0);
        assert_eq!(d.text, "");
        assert_eq!(d.last_uuid, None);
        assert!(!significant(&d));
    }

    #[test]
    fn unknown_mark_uuid_takes_the_whole_transcript() {
        let t = joined(&[user("u1", "one"), assistant("a1", "r1")]);
        let mark = Watermark::Transcript {
            uuid: "gone".to_string(),
            offset: 5,
        };
        let d = delta_since(&t, Some(&mark));
        assert_eq!(d.user_prompts, 1);
        assert_eq!(d.assistant_messages, 1);
    }

    #[test]
    fn title_mark_takes_the_whole_transcript() {
        let t = joined(&[user("u1", "one"), assistant("a1", "r1")]);
        let d = delta_since(&t, Some(&Watermark::Title("shell".to_string())));
        assert_eq!(d.user_prompts, 1);
        assert_eq!(d.assistant_messages, 1);
    }

    #[test]
    fn one_user_prompt_is_significant() {
        let d = Delta {
            user_prompts: 1,
            ..Delta::default()
        };
        assert!(significant(&d));
    }

    #[test]
    fn six_assistant_messages_are_significant_five_are_not() {
        let five = Delta {
            assistant_messages: 5,
            ..Delta::default()
        };
        let six = Delta {
            assistant_messages: 6,
            ..Delta::default()
        };
        assert!(!significant(&five));
        assert!(significant(&six));
        assert!(!significant(&Delta::default()));
    }

    #[test]
    fn last_uuid_is_the_last_well_formed_line() {
        let t = joined(&[user("u1", "a"), assistant("a1", "b"), "{broken".to_string()]);
        assert_eq!(last_uuid(&t).as_deref(), Some("a1"));
        assert_eq!(last_uuid(""), None);
        assert_eq!(last_uuid("junk\n"), None);
    }

    #[test]
    fn tail_chars_cuts_on_char_boundaries() {
        assert_eq!(tail_chars("abcdef", 3), "def");
        assert_eq!(tail_chars("abc", 10), "abc");
        assert_eq!(tail_chars("abc", 0), "");
        assert_eq!(tail_chars("äöü日本", 2), "日本");
        assert_eq!(tail_chars("äöü日本", 4), "öü日本");
        assert_eq!(tail_chars("", 4), "");
    }

    // --- prompt ---

    #[test]
    fn prompt_lists_roles_nodes_links_title_and_text() {
        let roles: Vec<String> = DEFAULT_ROLES.iter().map(|r| r.to_string()).collect();
        let nodes = [
            NodeSummary {
                id: "adr-118",
                kind: "adr",
                title: "Unseal check",
            },
            NodeSummary {
                id: "P-004",
                kind: "project",
                title: "Vault",
            },
        ];
        let current = Tags {
            links: vec![TagLink {
                node: "adr-118".to_string(),
                role: "implementing".to_string(),
            }],
            activity: Some("wiring".to_string()),
            topic: None,
        };
        let p = prompt(
            &roles,
            &nodes,
            &current,
            "claude: unseal",
            "user: hi\nassistant: hello\n",
        );
        assert!(p.contains("planning, implementing, researching, related"));
        assert!(p.contains("adr-118\tadr\tUnseal check\n"));
        assert!(p.contains("P-004\tproject\tVault\n"));
        assert!(p.contains(r#"{"links":[{"node":"adr-118","role":"implementing"}],"activity":"wiring","topic":null}"#));
        assert!(p.contains("claude: unseal"));
        assert!(p.ends_with("user: hi\nassistant: hello\n"));
        assert!(p.contains("\"links\""));
        assert!(p.contains("\"activity\""));
        assert!(p.contains("\"topic\""));
        assert!(p.to_lowercase().contains("only"));
    }

    #[test]
    fn prompt_keeps_only_the_transcript_tail() {
        let roles = vec!["related".to_string()];
        let text: String = "x".repeat(MAX_TRANSCRIPT_CHARS + 100);
        let p = prompt(&roles, &[], &Tags::default(), "t", &text);
        let xs = p.chars().filter(|c| *c == 'x').count();
        assert!((MAX_TRANSCRIPT_CHARS..MAX_TRANSCRIPT_CHARS + 100).contains(&xs));
    }

    // --- response ---

    #[test]
    fn parse_response_accepts_a_bare_object() {
        let tags = parse_response(
            r#"{"links":[{"node":"adr-118","role":"implementing"}],"activity":"wiring","topic":null}"#,
        )
        .unwrap();
        assert_eq!(tags.links.len(), 1);
        assert_eq!(tags.links[0].node, "adr-118");
        assert_eq!(tags.activity.as_deref(), Some("wiring"));
        assert_eq!(tags.topic, None);
    }

    #[test]
    fn parse_response_tolerates_missing_keys_and_topic() {
        let tags = parse_response(r#"{"topic":"lunch"}"#).unwrap();
        assert!(tags.links.is_empty());
        assert_eq!(tags.topic.as_deref(), Some("lunch"));
        let tags = parse_response(r#"{"links":null,"activity":null}"#).unwrap();
        assert!(tags.links.is_empty());
    }

    #[test]
    fn parse_response_unwraps_the_claude_json_envelope() {
        let inner =
            r#"{"links":[{"node":"P-004","role":"planning"}],"activity":"roadmap","topic":null}"#;
        let envelope = serde_json::json!({
            "type": "result", "subtype": "success", "is_error": false,
            "result": inner, "session_id": "abc", "total_cost_usd": 0.001
        });
        let tags = parse_response(&envelope.to_string()).unwrap();
        assert_eq!(tags.links[0].node, "P-004");
        assert_eq!(tags.activity.as_deref(), Some("roadmap"));
    }

    #[test]
    fn parse_response_strips_fences_and_prose_inside_the_envelope() {
        let inner = "Here you go:\n```json\n{\"links\": [], \"activity\": \"idle\", \"topic\": \"weather {and} braces\"}\n```\nHope that helps.";
        let envelope = serde_json::json!({ "type": "result", "result": inner });
        let tags = parse_response(&envelope.to_string()).unwrap();
        assert!(tags.links.is_empty());
        assert_eq!(tags.activity.as_deref(), Some("idle"));
        assert_eq!(tags.topic.as_deref(), Some("weather {and} braces"));
    }

    #[test]
    fn parse_response_accepts_a_fenced_bare_object() {
        let tags = parse_response("```json\n{\"links\":[],\"topic\":\"x\"}\n```\n").unwrap();
        assert_eq!(tags.topic.as_deref(), Some("x"));
    }

    #[test]
    fn parse_response_rejects_garbage() {
        assert!(parse_response("").is_err());
        assert!(parse_response("not json at all").is_err());
        assert!(parse_response("[1,2]").is_err());
        assert!(parse_response(r#"{"links":"nope"}"#).is_err());
        let envelope = serde_json::json!({ "type": "result", "result": "no object here" });
        assert!(parse_response(&envelope.to_string()).is_err());
        let error_envelope = serde_json::json!({ "type": "result", "is_error": true, "result": 5 });
        assert!(parse_response(&error_envelope.to_string()).is_err());
    }

    // --- config ---

    #[test]
    fn default_config_runs_claude_haiku_with_json_output() {
        let c = Config::default();
        assert_eq!(
            c.tagger_command,
            [
                "claude",
                "-p",
                "--model",
                "haiku",
                "--output-format",
                "json"
            ]
        );
        assert_eq!(c.roles, DEFAULT_ROLES);
    }

    #[test]
    fn config_path_is_under_kabelsalat() {
        assert_eq!(
            config_path(Path::new("/home/u/.config")),
            PathBuf::from("/home/u/.config/kabelsalat/overview.json")
        );
    }

    #[test]
    fn parse_config_fills_missing_keys_with_defaults() {
        assert_eq!(parse_config("{}").unwrap(), Config::default());
        let c = parse_config(r#"{"roles":["a","b"],"unknown":1}"#).unwrap();
        assert_eq!(c.roles, ["a", "b"]);
        assert_eq!(c.tagger_command, Config::default().tagger_command);
        let c = parse_config(r#"{"tagger_command":["ollama","run","x"]}"#).unwrap();
        assert_eq!(c.tagger_command, ["ollama", "run", "x"]);
        assert_eq!(c.roles, DEFAULT_ROLES);
    }

    #[test]
    fn parse_config_rejects_bad_shapes() {
        assert!(parse_config(r#"{"tagger_command":[]}"#).is_err());
        assert!(parse_config(r#"{"tagger_command":"claude -p"}"#).is_err());
        assert!(parse_config(r#"{"tagger_command":["claude",1]}"#).is_err());
        assert!(parse_config(r#"{"roles":"planning"}"#).is_err());
        assert!(parse_config(r#"{"roles":[]}"#).is_err());
        assert!(parse_config("[]").is_err());
        assert!(parse_config("{").is_err());
    }

    #[test]
    fn load_config_falls_back_to_defaults() {
        let dir = temp_dir("load");
        assert_eq!(load_config(&dir.join("missing.json")), Config::default());
        let bad = dir.join("bad.json");
        std::fs::write(&bad, "{nope").unwrap();
        assert_eq!(load_config(&bad), Config::default());
        let good = dir.join("good.json");
        std::fs::write(&good, r#"{"roles":["only"]}"#).unwrap();
        assert_eq!(load_config(&good).roles, ["only"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- rate limit ---

    #[test]
    fn may_run_only_one_at_a_time_and_not_too_often() {
        assert!(may_run(None, false));
        assert!(!may_run(None, true));
        assert!(may_run(Some(MIN_RUN_GAP_SECS), false));
        assert!(may_run(Some(MIN_RUN_GAP_SECS + 1), false));
        assert!(!may_run(Some(MIN_RUN_GAP_SECS - 1), false));
        assert!(!may_run(Some(0), false));
        assert!(!may_run(Some(10_000), true));
    }

    #[test]
    fn title_input_uses_the_title_as_mark_and_text() {
        let (mark, text) = title_input("vim: notes.md");
        assert_eq!(mark, Watermark::Title("vim: notes.md".to_string()));
        assert!(text.contains("vim: notes.md"));
        assert_eq!(mark.render(), "title:vim: notes.md");
    }
}

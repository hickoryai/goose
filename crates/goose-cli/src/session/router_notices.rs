//! router-acp's own notices. router-acp speaks to the user inside the agent's reply text, as
//! a markdown blockquote: a `> router-acp · <headline>` line, then `> ` continuation lines with
//! the details. The transcript shows each notice as one `▸` line, like a tool call, with a short
//! summary of the headline; the tool-call list (↑) holds the full text.

/// A run of reply text: ordinary prose, or one router-acp notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Text(String),
    /// The notice's full text, without the blockquote markers.
    Notice(String),
}

const PREFIX: &str = "router-acp ·";

/// The notice text of a quoted line that starts a notice, if it does.
fn notice_start(line: &str) -> Option<&str> {
    let quoted = line.strip_prefix('>')?.trim_start();
    quoted.starts_with(PREFIX).then_some(quoted)
}

/// A quoted continuation line's text (`> detail` or a bare `>`).
fn continuation(line: &str) -> Option<&str> {
    let quoted = line.strip_prefix('>')?;
    Some(quoted.strip_prefix(' ').unwrap_or(quoted))
}

/// Splits reply text into prose and the router-acp notices quoted in it, in order. Blank lines
/// that only separated a notice from the prose around it go with the notice.
pub fn split(text: &str) -> Vec<Segment> {
    if !text.contains(PREFIX) {
        return vec![Segment::Text(text.to_string())];
    }
    let mut segments = Vec::new();
    let mut prose = String::new();
    let mut notice: Option<Vec<String>> = None;
    let mut after_notice = false;
    for line in text.split_inclusive('\n') {
        let bare = line.strip_suffix('\n').unwrap_or(line);
        let bare = bare.strip_suffix('\r').unwrap_or(bare);
        if let Some(start) = notice_start(bare) {
            if let Some(lines) = notice.take() {
                segments.push(Segment::Notice(lines.join("\n")));
            }
            push_prose(&mut segments, &mut prose);
            notice = Some(vec![start.to_string()]);
            continue;
        }
        if let Some(lines) = notice.as_mut() {
            if let Some(more) = continuation(bare) {
                lines.push(more.to_string());
                continue;
            }
            segments.push(Segment::Notice(lines.join("\n")));
            notice = None;
            after_notice = true;
        }
        if after_notice && bare.trim().is_empty() {
            continue;
        }
        after_notice = false;
        prose.push_str(line);
    }
    if let Some(lines) = notice.take() {
        segments.push(Segment::Notice(lines.join("\n")));
    }
    push_prose(&mut segments, &mut prose);
    segments
}

/// Prose before a notice keeps one line end at most, so no blank line sits above the notice.
fn push_prose(segments: &mut Vec<Segment>, prose: &mut String) {
    let text = std::mem::take(prose);
    let trimmed = text.trim_end_matches(['\n', '\r', ' ']);
    if trimmed.is_empty() {
        return;
    }
    let ended = text
        .get(trimmed.len()..)
        .is_some_and(|rest| rest.contains('\n'));
    segments.push(Segment::Text(if ended {
        format!("{trimmed}\n")
    } else {
        trimmed.to_string()
    }));
}

/// The one-line summary of a notice: `router-acp · ` and the short form of its headline.
pub fn summary(notice: &str) -> String {
    let headline = notice.lines().next().unwrap_or_default();
    let headline = headline
        .strip_prefix(PREFIX)
        .map(str::trim_start)
        .unwrap_or(headline);
    let short = SUMMARIZERS
        .iter()
        .find_map(|(matches, summarize)| matches(headline).then(|| summarize(headline)))
        .flatten()
        .unwrap_or_else(|| first_clause(headline));
    format!("{PREFIX} {}", plain(&short))
}

type Matcher = fn(&str) -> bool;
type Summarizer = fn(&str) -> Option<String>;

/// Each notice kind router-acp sends, by its headline, and how it is shortened. Anything else
/// falls back to its first clause.
const SUMMARIZERS: &[(Matcher, Summarizer)] = &[
    (|h| h.starts_with("delegate_task"), delegate_task),
    (|h| h.starts_with("switched "), switched),
    (|h| h.starts_with("failover: cordon"), cordon_failover),
    (|h| h.contains(" → ") && h.contains(" · task "), routed),
    (
        |h| h.starts_with("note: ") || h.starts_with("summarized by"),
        handoff_note,
    ),
    (|h| h.starts_with("planner: "), planner),
    (|h| h.starts_with("pre-class"), pre_class),
    (
        |h| h.starts_with("escalating to ") || h.starts_with("demoting to "),
        next_turn_move,
    ),
    (labelled_matches, labelled),
];

/// `delegate_task → <candidate> · task <class> · <strategy> → <candidate> · … · "<task>"`
/// becomes `delegate_task <class> · <strategy> → <model>`.
fn delegate_task(headline: &str) -> Option<String> {
    let parts: Vec<&str> = headline.split(" · ").collect();
    let class = parts
        .iter()
        .find_map(|part| part.strip_prefix("task "))
        .map(str::trim)?;
    let class_at = parts.iter().position(|part| part.starts_with("task "))?;
    let strategy = parts
        .get(class_at + 1)
        .filter(|part| !part.starts_with('"'))
        .map(|part| shorten_ids(part));
    let to = parts
        .first()
        .and_then(|first| first.split_once(" → "))
        .map(|(_, candidate)| shorten_ids(candidate));
    Some(match (strategy, to) {
        (Some(strategy), _) if strategy.contains(" → ") => {
            format!("delegate_task {class} · {strategy}")
        }
        (Some(strategy), Some(to)) => format!("delegate_task {class} · {strategy} → {to}"),
        (None, Some(to)) => format!("delegate_task {class} → {to}"),
        (Some(strategy), None) => format!("delegate_task {class} · {strategy}"),
        (None, None) => format!("delegate_task {class}"),
    })
}

/// `switched <old> → <new> — <reason>` becomes `switched <old model> → <new model> · <reason>`.
fn switched(headline: &str) -> Option<String> {
    let rest = headline.strip_prefix("switched ")?;
    let (route, reason) = match rest.split_once(" — ") {
        Some((route, reason)) => (route, Some(reason)),
        None => (rest, None),
    };
    let route = shorten_ids(route);
    Some(match reason.map(short_reason).filter(|r| !r.is_empty()) {
        Some(reason) => format!("switched {route} · {reason}"),
        None => format!("switched {route}"),
    })
}

/// `failover: cordon → <candidate> · task <class> (<reason>…)`.
fn cordon_failover(headline: &str) -> Option<String> {
    let parts: Vec<&str> = headline.split(" · ").collect();
    let to = shorten_ids(parts.first()?.split_once(" → ")?.1);
    let task = parts.get(1).and_then(|part| part.strip_prefix("task "));
    let (class, reason) = match task {
        Some(task) => match task.split_once(" (") {
            Some((class, reason)) => (Some(class), Some(reason.trim_end_matches(')'))),
            None => (Some(task), None),
        },
        None => (None, None),
    };
    let mut out = format!("failover → {to}");
    if let Some(class) = class {
        out.push_str(&format!(" · {class}"));
    }
    if let Some(reason) = reason.map(short_reason).filter(|r| !r.is_empty()) {
        out.push_str(&format!(" · {reason}"));
    }
    Some(out)
}

/// The routing line for a turn: `[failover: ]<strategy> → <candidate> · task <class>
/// (complexity <n>)` becomes `<strategy> → <model> · <class>`.
fn routed(headline: &str) -> Option<String> {
    let (route, task) = headline.split_once(" · task ")?;
    let class = task.split(" (").next().unwrap_or(task).trim();
    Some(format!("{} · {class}", shorten_ids(route)))
}

fn handoff_note(headline: &str) -> Option<String> {
    let note = headline.strip_prefix("note: ").unwrap_or(headline);
    Some(format!(
        "handoff {}",
        note.split(';').next().unwrap_or(note).trim()
    ))
}

/// `planner: <why> → <phase>[ …]`.
fn planner(headline: &str) -> Option<String> {
    Some(first_clause(&headline.replace(" phase", "")))
}

/// `pre-class · …` lines: the step, without its timings.
fn pre_class(headline: &str) -> Option<String> {
    let parts: Vec<&str> = headline
        .split(" · ")
        .filter(|part| !part.starts_with("evaluator=") && !part.ends_with("ms"))
        .collect();
    Some(parts.join(" · "))
}

/// `escalating to <candidate> for the next turn — <reason>`.
fn next_turn_move(headline: &str) -> Option<String> {
    let (verb, rest) = headline.split_once(" to ")?;
    let (target, reason) = rest.split_once(" — ").unwrap_or((rest, ""));
    let target = shorten_ids(target.trim_end_matches(" for the next turn"));
    let reason = short_reason(reason);
    Some(if reason.is_empty() {
        format!("{verb} to {target}")
    } else {
        format!("{verb} to {target} · {reason}")
    })
}

/// Typed notices: `<label>: <what happens>`, e.g. `Reserve reached: switching to GPT-6.1 Sol`.
fn labelled_matches(headline: &str) -> bool {
    headline
        .split_once(": ")
        .is_some_and(|(label, _)| label.split(' ').count() <= 4 && !label.contains('·'))
}

fn labelled(headline: &str) -> Option<String> {
    let (label, rest) = headline.split_once(": ")?;
    let rest = rest.split(" (").next().unwrap_or(rest);
    let rest = rest.split(';').next().unwrap_or(rest).trim();
    Some(format!("{label} · {}", shorten_ids(rest)))
}

/// A reason's first clause: up to a `;`, `(` or ` — `.
fn short_reason(reason: &str) -> String {
    let end = [";", " (", " — ", ", "]
        .iter()
        .filter_map(|stop| reason.find(stop))
        .min()
        .unwrap_or(reason.len());
    shorten_ids(reason.get(..end).unwrap_or(reason).trim())
}

const FALLBACK_CHARS: usize = 60;

/// An unknown notice: its first ` · ` / ` — ` clause, cut to a readable length.
fn first_clause(headline: &str) -> String {
    let end = [" · ", " — "]
        .iter()
        .filter_map(|stop| headline.find(stop))
        .min()
        .unwrap_or(headline.len());
    let clause = shorten_ids(headline.get(..end).unwrap_or(headline).trim());
    if clause.chars().count() > FALLBACK_CHARS {
        let cut: String = clause.chars().take(FALLBACK_CHARS - 1).collect();
        format!("{}…", cut.trim_end())
    } else {
        clause
    }
}

/// Every router candidate id in `text` (`agent@account/model[variant]`) as its short model
/// family: `claude@0746…/opus[1m]` → `opus`.
pub fn shorten_ids(text: &str) -> String {
    text.split(' ')
        .map(|word| {
            let core = word.trim_end_matches([',', ')', ';', ':', '.']);
            let tail = word.get(core.len()..).unwrap_or_default();
            match core
                .split_once('@')
                .and_then(|(_, rest)| rest.split_once('/'))
            {
                Some((_, model)) => format!("{}{tail}", model_family(model)),
                None => word.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A model id's short family: `opus[1m]` → `opus`, `gpt-6.1-sol` → `sol`,
/// `claude-opus-4-1` → `opus`, `grok-4` → `grok`.
pub fn model_family(model: &str) -> String {
    let model = model.split('[').next().unwrap_or(model);
    let model = model.rsplit('/').next().unwrap_or(model);
    let words: Vec<&str> = model
        .split(['-', '_'])
        .filter(|word| word.chars().any(|c| c.is_ascii_alphabetic()))
        .filter(|word| !matches!(*word, "latest" | "preview" | "claude" | "gpt" | "openai"))
        .collect();
    words
        .last()
        .map(|word| word.to_ascii_lowercase())
        .unwrap_or_else(|| model.to_string())
}

/// Markdown emphasis and code marks would print literally on a plain line; drop them.
fn plain(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(c, '*' | '`'))
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const DELEGATE: &str = r#"router-acp · delegate_task → claude@07460b494a7f4c0b9474e2957345695f/opus[1m] · task Feature · planner → claude@07460b494a7f4c0b9474e2957345695f/opus[1m] · phase=implementation (pool: implementation_candidates + boost +3.00) · utility 3.63 = 0.84×quality 2.97→0.55 (Feature) + 0.16×quota (headroom 60%, cost rank 4) + pref 3.07 · tradeoff 3→1.6 (complexity-scaled) · "[router-acp planner role: implement-work] Policy identity...""#;

    #[test]
    fn delegate_task_keeps_class_strategy_and_model_family() {
        assert_eq!(
            summary(DELEGATE),
            "router-acp · delegate_task Feature · planner → opus"
        );
    }

    #[test]
    fn each_notice_kind_has_a_short_form() {
        let cases = [
            (
                "router-acp · switched codex@9f2c/gpt-6.1-sol → claude@0746/opus[1m] — usage cordon; resets 2026-10-17",
                "router-acp · switched sol → opus · usage cordon",
            ),
            (
                "router-acp · Reserve reached: switching to GPT-6.1 Sol",
                "router-acp · Reserve reached · switching to GPT-6.1 Sol",
            ),
            (
                "router-acp · note: summarized by the previous model; the new model does not see the earlier transcript verbatim",
                "router-acp · handoff summarized by the previous model",
            ),
            (
                "router-acp · failover: cordon → grok@ab12/grok-4 · task Bugfix (usage limit, resets 2026-10-17)",
                "router-acp · failover → grok · Bugfix · usage limit",
            ),
            (
                "router-acp · planner → claude@0746/opus[1m] · task Feature (complexity 0.62)",
                "router-acp · planner → opus · Feature",
            ),
            (
                "router-acp · escalating to claude@0746/opus[1m] for the next turn — repeated tool failures",
                "router-acp · escalating to opus · repeated tool failures",
            ),
            (
                "router-acp · GPT-6.1 Sol outage: no other model can serve; retrying GPT-6.1 Sol in 4m (at 2026-10-16T12:00:00Z)",
                "router-acp · GPT-6.1 Sol outage · no other model can serve",
            ),
            (
                "router-acp · pre-class · evaluator=haiku · 412ms",
                "router-acp · pre-class",
            ),
            (
                "router-acp · planner: skill `implement-work` → implementation phase",
                "router-acp · planner: skill implement-work → implementation",
            ),
        ];
        for (notice, expected) in cases {
            assert_eq!(summary(notice), expected, "for {notice}");
        }
    }

    #[test]
    fn unknown_notices_fall_back_to_their_first_clause() {
        assert_eq!(
            summary("router-acp · loaded ticket ABC-12 into context (4120 chars) · extra"),
            "router-acp · loaded ticket ABC-12 into context (4120 chars)"
        );
        let long = format!("router-acp · {}", "word ".repeat(30));
        let short = summary(&long);
        assert!(short.ends_with('…'), "{short}");
        assert!(short.chars().count() <= "router-acp · ".len() + FALLBACK_CHARS);
    }

    #[test]
    fn model_families_drop_account_and_variant() {
        assert_eq!(model_family("opus[1m]"), "opus");
        assert_eq!(model_family("gpt-6.1-sol"), "sol");
        assert_eq!(model_family("claude-opus-4-1"), "opus");
        assert_eq!(model_family("grok-4"), "grok");
        assert_eq!(
            shorten_ids("planner → claude@07460b494a7f4c0b9474e2957345695f/opus[1m],"),
            "planner → opus,"
        );
    }

    #[test]
    fn notices_split_out_of_reply_text_in_order() {
        let text = format!(
            "Starting now.\n\n> {DELEGATE}\n> routing detail line\n>\n> second detail\n\nThe planner is on it.\n"
        );
        assert_eq!(
            split(&text),
            [
                Segment::Text("Starting now.\n".into()),
                Segment::Notice(format!("{DELEGATE}\nrouting detail line\n\nsecond detail")),
                Segment::Text("The planner is on it.\n".into()),
            ]
        );
        assert_eq!(
            split("> router-acp · one\n> router-acp · two\n"),
            [
                Segment::Notice("router-acp · one".into()),
                Segment::Notice("router-acp · two".into()),
            ]
        );
        assert_eq!(
            split("plain > text\n"),
            [Segment::Text("plain > text\n".into())]
        );
    }
}

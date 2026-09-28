//! `--detailed` decision traces: the teacher contract, its parser, and the
//! rendered row text (see `docs/decision-traces.md`).

use serde_json::Value;

/// The training system message on every detailed row without
/// `[generation].persona`. It is hashed into `generator_config_hash`, so its
/// text must never change.
pub const DEFAULT_SYSTEM_MESSAGE: &str = "You are a careful IT operations assistant. Use supplied evidence, distinguish facts from hypotheses, prefer safe bounded actions, and include verification and recovery.";

const DEFAULT_PERSONA: &str = "a careful IT operations assistant";
const SYSTEM_GUIDANCE: &str = "Use supplied evidence, distinguish facts from hypotheses, prefer safe bounded actions, and include verification and recovery.";

/// The training system message: [`DEFAULT_SYSTEM_MESSAGE`], with its role
/// phrase replaced by `[generation].persona` when one is set.
pub fn system_message(persona: Option<&str>) -> String {
    format!(
        "You are {}. {SYSTEM_GUIDANCE}",
        persona.unwrap_or(DEFAULT_PERSONA)
    )
}

/// Appended to the outbound system message only; never stored in a row.
/// It is part of `generator_config_hash`, so editing it needs a new `--out`.
/// The rules are explained in `docs/decision-traces.md`.
pub const TEACHER_INSTRUCTION: &str = concat!(
    "Your reply trains an IT operations assistant. An on-call engineer reads the steps as an audit and acts on the final answer. They never see these instructions, so do not mention them.\n",
    "\n",
    "Coverage. Be complete, not brief: every step adds a new fact, check, inference, or decision; never restate the task. Scale depth to the task: a few steps for a narrow question, many for a multi-cause incident or a production change.\n",
    "- Answer every explicit request in the task and state material unknowns.\n",
    "- Tie incident facts to supplied observations; label general domain knowledge as a hypothesis or conditional recommendation.\n",
    "- For an incident, separate observed signals from possible causes and give a check that distinguishes each material cause.\n",
    "- For a requested change, give staged steps with the precheck, approval, success check, stop condition, and rollback for each stage.\n",
    "- For each proposed check, name the expected signal and explain how it changes the next decision.\n",
    "- Prefer read-only checks. Bound risky actions and state approval needs.\n",
    "\n",
    "Grounding. Observations come only from the task; nothing ran or changed unless the task says so. Never invent command output, logs, metrics, versions, device names, addresses, or topology. If a detail is missing, say so or make the advice conditional. Grounding limits observations, not expertise: apply your full knowledge of the protocols, products, defaults, and commands involved, say what it implies for this case, and label it as general knowledge. Draw every inference the supplied evidence supports, including what it already rules out.\n",
    "\n",
    "Steps. Each step is one paragraph with no line breaks, written as a publishable audit, not private chain-of-thought. Evidence cites a supplied observation. A hypothesis names a possible cause and what would confirm or rule it out. An action says what to run or request, why, and what each likely result would mean; never its result. Give the exact read-only command or query when the platform is known; if unsure of the exact syntax, name the output to look for instead of guessing. A verification confirms a named hypothesis or change. A conclusion states the decision, confidence, and open questions.\n",
    "\n",
    "Final answer. The complete answer in the role and format the task asks for, usable without the steps. Do not introduce yourself or name the assistant. Use Markdown where it helps; headings start at ###.\n",
    "\n",
    "Before replying, check that the final answer covers each explicit request in the task, then check the draft against Coverage and Grounding and fix gaps.\n",
    "\n",
    "Output. One JSON object and no other text, with exactly two keys: reasoning_steps, then final_answer. reasoning_steps is an array of at most 24 objects, each with exactly two keys, kind and content. kind is one of evidence, hypothesis, action, verification, conclusion; use each at least once and repeat as needed. content and final_answer are non-empty JSON strings. End the reply with the closing brace of the object.",
);

pub const MAX_STEPS: usize = 24;

/// Step kinds with their rendered labels.
const KINDS: [(&str, &str); 5] = [
    ("evidence", "Evidence"),
    ("hypothesis", "Hypothesis"),
    ("action", "Action"),
    ("verification", "Verification"),
    ("conclusion", "Conclusion"),
];

const TRACE_HEADING: &str = "## Decision trace";
const ANSWER_HEADING: &str = "## Final answer";

pub struct Step {
    /// Lowercase kind, one of the five.
    pub kind: &'static str,
    pub content: String,
}

pub struct Trace {
    pub steps: Vec<Step>,
    pub final_answer: String,
}

/// The teacher's reply as a trace, or `None` when it breaks the contract.
/// The object is the whole string, or the first object followed only by
/// whitespace or a closing fence. A preamble before the object is allowed.
pub fn parse(content: &str) -> Option<Trace> {
    let text = content.trim();
    find(text).or_else(|| {
        // A reply complete except for the object's closing brace (seen from
        // some teachers without JSON mode) is repaired once. Every
        // other rule still applies.
        let unfenced = text.strip_suffix("```").unwrap_or(text).trim_end();
        find(&format!("{unfenced}}}"))
    })
}

fn find(text: &str) -> Option<Trace> {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        if let Some(trace) = from_value(&value) {
            return Some(trace);
        }
    }
    for (start, _) in text.match_indices('{') {
        let mut stream = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>();
        let Some(Ok(value)) = stream.next() else {
            continue;
        };
        let rest = text[start + stream.byte_offset()..].trim();
        if !rest.is_empty() && rest != "```" {
            continue;
        }
        if let Some(trace) = from_value(&value) {
            return Some(trace);
        }
    }
    None
}

fn from_value(value: &Value) -> Option<Trace> {
    let object = value.as_object()?;
    if object.len() != 2 {
        return None;
    }
    let raw_steps = object.get("reasoning_steps")?.as_array()?;
    let final_answer = strip(object.get("final_answer")?.as_str()?);
    if final_answer.is_empty() || raw_steps.is_empty() || raw_steps.len() > MAX_STEPS {
        return None;
    }
    let mut steps = Vec::with_capacity(raw_steps.len());
    for step in raw_steps {
        let step = step.as_object()?;
        if step.len() != 2 {
            return None;
        }
        let kind = step.get("kind")?.as_str()?;
        let (kind, _) = KINDS.iter().find(|(known, _)| *known == kind)?;
        let content = strip(step.get("content")?.as_str()?);
        // A heading line inside a step would make the rendered text ambiguous.
        if content.is_empty()
            || content
                .lines()
                .any(|line| matches!(line.trim(), TRACE_HEADING | ANSWER_HEADING))
        {
            return None;
        }
        steps.push(Step {
            kind,
            content: content.to_string(),
        });
    }
    if !KINDS
        .iter()
        .all(|(kind, _)| steps.iter().any(|step| step.kind == *kind))
    {
        return None;
    }
    Some(Trace {
        steps,
        final_answer: final_answer.to_string(),
    })
}

/// Python's `str.strip()`: Rust `trim` plus the separators U+001C..U+001F,
/// which Python also counts as whitespace, so a Python tool that checks the
/// same trace trims it the same way.
fn strip(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// The row text: `## Decision trace`, one numbered `Label: content` line per
/// step, a blank line, `## Final answer`, then the answer.
pub fn render(trace: &Trace) -> String {
    let mut lines = vec![TRACE_HEADING.to_string()];
    for (index, step) in trace.steps.iter().enumerate() {
        let label = KINDS
            .iter()
            .find(|(kind, _)| *kind == step.kind)
            .map_or(step.kind, |(_, label)| label);
        lines.push(format!("{}. {label}: {}", index + 1, step.content));
    }
    lines.push(String::new());
    lines.push(ANSWER_HEADING.to_string());
    lines.push(trace.final_answer.clone());
    lines.join("\n")
}

/// The text after the first `\n\n## Final answer\n` of a rendered trace.
pub fn final_answer(rendered: &str) -> Option<&str> {
    rendered
        .split_once("\n\n## Final answer\n")
        .map(|(_, answer)| answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [&str; 5] = [
        "evidence",
        "hypothesis",
        "action",
        "verification",
        "conclusion",
    ];

    const FIVE: &str = r#"{"reasoning_steps":[
      {"kind":"evidence","content":"e"},
      {"kind":"hypothesis","content":"h"},
      {"kind":"action","content":"a"},
      {"kind":"verification","content":"v"},
      {"kind":"conclusion","content":"c"}
    ],"final_answer":"do this"}"#;

    fn steps(kinds: &[&str]) -> String {
        let steps: Vec<String> = kinds
            .iter()
            .map(|kind| format!(r#"{{"kind":"{kind}","content":"x"}}"#))
            .collect();
        format!(
            r#"{{"reasoning_steps":[{}],"final_answer":"y"}}"#,
            steps.join(",")
        )
    }

    #[test]
    fn renders_numbered_steps_then_the_final_answer() {
        let trace = parse(FIVE).unwrap();
        let text = render(&trace);
        assert_eq!(
            text,
            "## Decision trace\n1. Evidence: e\n2. Hypothesis: h\n3. Action: a\n4. Verification: v\n5. Conclusion: c\n\n## Final answer\ndo this"
        );
        assert_eq!(final_answer(&text), Some("do this"));
    }

    #[test]
    fn accepts_repeats_fences_preambles_and_trims() {
        let mut kinds = ALL.to_vec();
        kinds.insert(2, "hypothesis");
        assert_eq!(parse(&steps(&kinds)).unwrap().steps.len(), 6);
        assert!(parse(&format!("```json\n{FIVE}\n```")).is_some());
        assert!(parse(&format!("Here is the JSON:\n```json\n{FIVE}\n```")).is_some());
        let padded = FIVE
            .replace("\"e\"", "\"  e \\n\"")
            .replace("\"do this\"", "\" do this\\n\"");
        let trace = parse(&padded).unwrap();
        assert_eq!(trace.steps[0].content, "e");
        assert_eq!(trace.final_answer, "do this");
        // Python's str.strip() also removes U+001C..U+001F.
        let separators = FIVE.replace("\"e\"", "\"\\u001fe\\u001c\"");
        assert_eq!(parse(&separators).unwrap().steps[0].content, "e");
        let blank = FIVE.replace("\"e\"", "\"\\u001f\"");
        assert!(parse(&blank).is_none());
        let cap: Vec<&str> = ALL.iter().cycle().take(MAX_STEPS).copied().collect();
        assert!(parse(&steps(&cap)).is_some());
    }

    #[test]
    fn a_reply_missing_only_its_closing_brace_is_repaired() {
        let open = FIVE.strip_suffix('}').unwrap();
        assert!(parse(open).is_some());
        assert!(parse(&format!("```json\n{open}\n```")).is_some());
        // Anything more broken than one missing brace still fails.
        assert!(parse(open.strip_suffix("\"do this\"").unwrap()).is_none());
        assert!(parse(steps(&ALL[..4]).trim_end_matches('}')).is_none());
    }

    #[test]
    fn a_multi_line_step_keeps_its_newline() {
        let raw = FIVE.replace("\"a\"", "\"run:\\n- show bgp summary\"");
        assert!(render(&parse(&raw).unwrap())
            .contains("3. Action: run:\n- show bgp summary\n4. Verification: v"));
    }

    #[test]
    fn rejects_everything_outside_the_contract() {
        let over: Vec<&str> = ALL.iter().cycle().take(MAX_STEPS + 1).copied().collect();
        let rejected = [
            steps(&ALL[..4]),
            steps(&over),
            FIVE.replace("\"final_answer\"", "\"tool_calls\":[],\"final_answer\""),
            FIVE.replace("\"content\":\"e\"", "\"content\":\"e\",\"id\":1"),
            FIVE.replace("\"evidence\"", "\"Evidence\""),
            FIVE.replace("\"content\":\"e\"", "\"content\":\"  \""),
            FIVE.replace("\"do this\"", "\" \""),
            FIVE.replace("\"a\"", "\"a\\n## Final answer\\nb\""),
            format!("[{FIVE}]"),
            format!("{FIVE}\nLet me know if you need more."),
            "**Most likely hypothesis:** graceful restart did not finish.".to_string(),
            "## Decision trace\n1. Evidence: e\n2. Hypothesis: h\n3. Action: a\n4. Verification: v\n5. Conclusion: c\n\n## Final answer\nx".to_string(),
        ];
        for raw in rejected {
            assert!(parse(&raw).is_none(), "{raw}");
        }
    }

    #[test]
    fn default_system_message_names_an_it_operations_assistant() {
        assert_eq!(system_message(None), DEFAULT_SYSTEM_MESSAGE);
        assert_eq!(
            system_message(Some("Sia by Scogo.AI, which delivers Autonomous Agentic IT Operations")),
            "You are Sia by Scogo.AI, which delivers Autonomous Agentic IT Operations. Use supplied evidence, distinguish facts from hypotheses, prefer safe bounded actions, and include verification and recovery."
        );
    }

    #[test]
    fn teacher_instruction_matches_the_contract() {
        assert!(TEACHER_INSTRUCTION.contains("One JSON object and no other text"));
        assert!(!TEACHER_INSTRUCTION.contains("concise"));
        assert!(TEACHER_INSTRUCTION.contains(&format!("at most {MAX_STEPS} objects")));
        assert!(!TEACHER_INSTRUCTION.contains("tool_calls"));
        assert!(!TEACHER_INSTRUCTION.ends_with('\n'));
    }
}

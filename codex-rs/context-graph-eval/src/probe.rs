//! Probes: questions about a recorded session whose answers need specific earlier blocks.
//!
//! A strong model reads a session and writes probes with reference answers and the blocks that
//! support them. Evaluation asks each probe under different contexts and grades the answers.

use std::collections::HashSet;
use std::io::BufRead as _;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context as _;
use codex_context_graph::BlockId;
use codex_context_graph::history::History;
use codex_context_graph::prompt::AnswerError;
use codex_context_graph::prompt::ModelPrompt;
use codex_context_graph::prompt::parse_answer;
use codex_context_graph::render::kind_name;
use codex_context_graph::text::render_block;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_json::json;

const GENERATION_INSTRUCTIONS: &str = include_str!("../prompts/probe_generation.md");

/// Characters of one item, and of one block, that the generator reads.
const GENERATION_ITEM_CHARS: usize = 3_000;
const GENERATION_BLOCK_CHARS: usize = 6_000;

pub const CATEGORIES: [&str; 5] = ["lexical", "dependency", "multi_hop", "superseded", "local"];

/// A question about a session, asked after `turn`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probe {
    pub id: String,
    pub rollout: PathBuf,
    pub graph_id: String,
    pub turn: usize,
    pub category: String,
    pub question: String,
    pub answer: String,
    pub key_facts: Vec<String>,
    /// Blocks whose content the answer needs.
    pub support: Vec<BlockId>,
    /// Blocks that lead to an outdated answer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outdated: Vec<BlockId>,
    pub generator: String,
}

/// A rendered generation request.
pub struct GenerationPrompt {
    pub prompt: ModelPrompt,
    /// History position of the block each label `B1`, `B2`, … names.
    pub blocks: Vec<usize>,
}

/// Renders the request for `count` probes about `history`.
pub fn build_generation_prompt(history: &History<'_>, count: usize) -> GenerationPrompt {
    let mut input = String::from("## Session\n");
    let mut blocks = Vec::new();
    let mut turn = None;
    for index in (0..history.len()).filter(|index| history.recallable(*index)) {
        if turn != Some(history.turn(index)) {
            turn = Some(history.turn(index));
            input.push_str(&format!("\n### Turn {}\n", history.turn(index)));
        }
        blocks.push(index);
        input.push_str(&format!(
            "\n#### B{} [{}]\n",
            blocks.len(),
            kind_name(history.card(index).kind)
        ));
        input.push_str(&render_block(
            history.block(index).items,
            GENERATION_ITEM_CHARS,
            GENERATION_BLOCK_CHARS,
        ));
    }
    input.push_str(&format!(
        "\n## Task\nWrite {count} questions. The last turn is turn {}.\n",
        history.last_turn()
    ));
    GenerationPrompt {
        prompt: ModelPrompt {
            instructions: GENERATION_INSTRUCTIONS.to_string(),
            input,
            schema_name: "session_probes",
            schema: generation_schema(),
        },
        blocks,
    }
}

fn generation_schema() -> Value {
    let strings = json!({ "type": "array", "items": { "type": "string" } });
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["probes"],
        "properties": {
            "probes": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["category", "question", "answer", "key_facts", "support", "outdated"],
                    "properties": {
                        "category": { "type": "string", "enum": CATEGORIES },
                        "question": { "type": "string" },
                        "answer": { "type": "string" },
                        "key_facts": strings,
                        "support": strings,
                        "outdated": strings
                    }
                }
            }
        }
    })
}

#[derive(Deserialize)]
struct GeneratedProbes {
    probes: Vec<GeneratedProbe>,
}

#[derive(Deserialize)]
struct GeneratedProbe {
    category: String,
    question: String,
    answer: String,
    #[serde(default)]
    key_facts: Vec<String>,
    #[serde(default)]
    support: Vec<String>,
    #[serde(default)]
    outdated: Vec<String>,
}

/// Probes of a generation answer that name a known category and at least one known support
/// block. Unknown labels are dropped.
pub fn parse_generated_probes(
    prompt: &GenerationPrompt,
    history: &History<'_>,
    text: &str,
) -> Result<Vec<GeneratedProbeFields>, AnswerError> {
    let answer: GeneratedProbes = parse_answer(text)?;
    let resolve = |labels: &[String]| -> Vec<BlockId> {
        let mut seen = HashSet::new();
        labels
            .iter()
            .filter_map(|label| {
                let number: usize = label.trim().trim_start_matches(['B', 'b']).parse().ok()?;
                let index = *prompt.blocks.get(number.checked_sub(1)?)?;
                seen.insert(index)
                    .then(|| history.card(index).block_id.clone())
            })
            .collect()
    };
    Ok(answer
        .probes
        .into_iter()
        .filter(|probe| CATEGORIES.contains(&probe.category.as_str()))
        .filter_map(|probe| {
            let support = resolve(&probe.support);
            (!support.is_empty() && !probe.question.trim().is_empty()).then(|| {
                GeneratedProbeFields {
                    category: probe.category,
                    question: probe.question.trim().to_string(),
                    answer: probe.answer.trim().to_string(),
                    key_facts: probe.key_facts,
                    outdated: resolve(&probe.outdated),
                    support,
                }
            })
        })
        .collect())
}

/// The fields of a probe that generation writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedProbeFields {
    pub category: String,
    pub question: String,
    pub answer: String,
    pub key_facts: Vec<String>,
    pub support: Vec<BlockId>,
    pub outdated: Vec<BlockId>,
}

/// Reads a JSON-lines file, or nothing when it does not exist.
pub fn read_jsonl<T: DeserializeOwned>(path: &Path) -> anyhow::Result<Vec<T>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("failed to open {}", path.display())),
    };
    std::io::BufReader::new(file)
        .lines()
        .enumerate()
        .filter(|(_, line)| !matches!(line, Ok(line) if line.trim().is_empty()))
        .map(|(number, line)| {
            serde_json::from_str(&line?)
                .with_context(|| format!("{}:{}", path.display(), number + 1))
        })
        .collect()
}

/// Appends `value` to a JSON-lines file.
pub fn append_jsonl<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    writeln!(file, "{}", serde_json::to_string(value)?)?;
    Ok(())
}

#[cfg(test)]
#[path = "probe_tests.rs"]
mod tests;

//! Lexical search over cards: BM25 plus a bonus for files and symbols the query names. Recall
//! uses it to cut the candidates of very long histories down to what a screening prompt can
//! hold, and evaluation uses it as the simplest selector.

use std::collections::HashMap;

use crate::card::BlockCard;

const K1: f32 = 1.2;
const B: f32 = 0.75;
/// Score added when the query names a file the block read, wrote or mentioned.
const FILE_BONUS: f32 = 4.0;
/// Score added when the query names a symbol the block mentions.
const SYMBOL_BONUS: f32 = 2.0;
/// Shortest file name or symbol that earns a bonus, so that `a` or `io` do not match everything.
const MIN_ENTITY_CHARS: usize = 4;

/// Splits `text` into search terms: lowercase words of ASCII letters and digits, the parts of
/// snake_case and camelCase identifiers, and overlapping pairs of characters in other scripts,
/// such as Chinese, which does not separate words with spaces.
pub fn terms(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut word = String::new();
    let mut run: Vec<char> = Vec::new();
    for ch in text.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            flush_run(&mut run, &mut terms);
            word.push(ch);
        } else if ch.is_alphanumeric() {
            flush_word(&mut word, &mut terms);
            run.push(ch);
        } else {
            flush_word(&mut word, &mut terms);
            flush_run(&mut run, &mut terms);
        }
    }
    terms
}

fn flush_word(word: &mut String, terms: &mut Vec<String>) {
    if word.is_empty() {
        return;
    }
    let parts = identifier_parts(word);
    terms.push(word.to_ascii_lowercase());
    if parts.len() > 1 {
        terms.extend(parts.into_iter().map(str::to_ascii_lowercase));
    }
    word.clear();
}

fn flush_run(run: &mut Vec<char>, terms: &mut Vec<String>) {
    match run.len() {
        0 => {}
        1 => terms.push(run.iter().collect()),
        _ => terms.extend(run.windows(2).map(|pair| pair.iter().collect::<String>())),
    }
    run.clear();
}

/// The parts of a snake_case or camelCase identifier.
fn identifier_parts(word: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    for piece in word.split('_').filter(|piece| !piece.is_empty()) {
        let mut start = 0;
        let bytes = piece.as_bytes();
        for index in 1..bytes.len() {
            if bytes[index].is_ascii_uppercase() && bytes[index - 1].is_ascii_lowercase() {
                parts.push(&piece[start..index]);
                start = index;
            }
        }
        parts.push(&piece[start..]);
    }
    parts
}

/// The searchable text of a card.
pub fn card_text(card: &BlockCard) -> String {
    let entities = &card.entities;
    let mut text = vec![card.title.as_str(), card.summary.as_str()];
    text.extend(card.claims.iter().map(|claim| claim.text.as_str()));
    text.extend(entities.files_read.iter().map(String::as_str));
    text.extend(entities.files_written.iter().map(String::as_str));
    text.extend(entities.files_mentioned.iter().map(String::as_str));
    text.extend(entities.symbols.iter().map(String::as_str));
    text.extend(entities.commands.iter().map(String::as_str));
    text.extend(entities.errors.iter().map(String::as_str));
    text.extend(card.tools.iter().map(String::as_str));
    text.join("\n")
}

/// A BM25 index over a fixed set of documents.
#[derive(Debug, Clone)]
pub struct Bm25 {
    documents: Vec<HashMap<String, u32>>,
    lengths: Vec<f32>,
    average_length: f32,
    document_frequency: HashMap<String, u32>,
}

impl Bm25 {
    pub fn new<S: AsRef<str>>(documents: impl IntoIterator<Item = S>) -> Self {
        let mut counts = Vec::new();
        let mut lengths = Vec::new();
        let mut document_frequency: HashMap<String, u32> = HashMap::new();
        for document in documents {
            let mut terms_in_document: HashMap<String, u32> = HashMap::new();
            let document_terms = terms(document.as_ref());
            lengths.push(document_terms.len() as f32);
            for term in document_terms {
                *terms_in_document.entry(term).or_default() += 1;
            }
            for term in terms_in_document.keys() {
                *document_frequency.entry(term.clone()).or_default() += 1;
            }
            counts.push(terms_in_document);
        }
        let average_length = if lengths.is_empty() {
            0.0
        } else {
            lengths.iter().sum::<f32>() / lengths.len() as f32
        };
        Self {
            documents: counts,
            lengths,
            average_length,
            document_frequency,
        }
    }

    /// The BM25 score of every document for `query`, in document order.
    pub fn scores(&self, query: &str) -> Vec<f32> {
        let mut query_terms = terms(query);
        query_terms.sort();
        query_terms.dedup();
        let total = self.documents.len() as f32;
        self.documents
            .iter()
            .zip(&self.lengths)
            .map(|(counts, length)| {
                query_terms
                    .iter()
                    .filter_map(|term| {
                        let frequency = *counts.get(term)? as f32;
                        let containing =
                            self.document_frequency.get(term).copied().unwrap_or(0) as f32;
                        let idf = ((total - containing + 0.5) / (containing + 0.5) + 1.0).ln();
                        let norm = 1.0 - B + B * length / self.average_length.max(1.0);
                        Some(idf * frequency * (K1 + 1.0) / (frequency + K1 * norm))
                    })
                    .sum()
            })
            .collect()
    }
}

/// Bonus for the files and symbols of `card` that `query` names.
pub fn entity_bonus(card: &BlockCard, query: &str) -> f32 {
    let entities = &card.entities;
    let names_file = entities
        .files_read
        .iter()
        .chain(&entities.files_written)
        .chain(&entities.files_mentioned)
        .any(|path| {
            let name = path.rsplit('/').next().unwrap_or(path);
            name.chars().count() >= MIN_ENTITY_CHARS && query.contains(name)
        });
    let names_symbol = entities.symbols.iter().any(|symbol| {
        symbol.chars().count() >= MIN_ENTITY_CHARS && query.contains(symbol.as_str())
    });
    let mut bonus = 0.0;
    if names_file {
        bonus += FILE_BONUS;
    }
    if names_symbol {
        bonus += SYMBOL_BONUS;
    }
    bonus
}

/// Indices of `cards` ranked by relevance to `query`, best first; cards that share no term with
/// the query and name none of its files or symbols are left out.
pub fn rank_cards(cards: &[&BlockCard], query: &str) -> Vec<(usize, f32)> {
    let index = Bm25::new(cards.iter().map(|card| card_text(card)));
    let mut ranked: Vec<(usize, f32)> = index
        .scores(query)
        .into_iter()
        .zip(cards)
        .map(|(score, card)| score + entity_bonus(card, query))
        .enumerate()
        .filter(|(_, score)| *score > 0.0)
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;

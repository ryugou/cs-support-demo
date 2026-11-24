use std::cmp::{Ordering, Reverse};
use std::ops::Range;

use unicode_normalization::char::{canonical_combining_class, decompose_compatible};

use super::catalog::{NameCatalog, NameTarget};
use crate::resolve::normalize_key;

const MIN_EXACT_NAME_CHARS: usize = 2;
const MIN_TYPO_NAME_CHARS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MentionMatch {
    Exact,
    Typo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NameMention {
    pub(crate) surface: String,
    pub(crate) key: String,
    pub(crate) matched: MentionMatch,
    pub(crate) targets: Vec<NameTarget>,
    span: Range<usize>,
}

impl NameMention {
    pub(crate) fn single_target(&self) -> Option<&NameTarget> {
        match self.targets.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }
}

struct NormalizedText {
    chars: Vec<char>,
    sources: Vec<Range<usize>>,
    separated: Vec<bool>,
}

struct Occurrence {
    entry: usize,
    span: Range<usize>,
    matched: MentionMatch,
    keeps_class: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edit {
    Substitution(usize),
    Insertion(usize),
    Deletion,
    Transposition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    AsciiAlphanumeric,
    Hiragana,
    Katakana,
    Ideograph,
    Other,
}

pub(crate) fn name_chars(text: &str) -> Vec<char> {
    NormalizedText::new(text).chars
}

pub(crate) fn name_key(text: &str) -> String {
    name_chars(text).into_iter().collect()
}

pub(crate) fn find_mentions(text: &str, catalog: &NameCatalog) -> Vec<NameMention> {
    let normalized = NormalizedText::new(text);
    let occurrences = find_occurrences(&normalized, catalog);
    let exact = longest_exact_occurrences(&occurrences);
    let typos: Vec<&Occurrence> = occurrences
        .iter()
        .filter(|occurrence| occurrence.matched == MentionMatch::Typo)
        .filter(|occurrence| {
            !exact
                .iter()
                .any(|found| overlaps(&found.span, &occurrence.span))
        })
        .collect();
    let mut mentions = exact_mentions(text, &normalized, catalog, &exact);
    mentions.extend(typo_mentions(text, &normalized, catalog, typos));
    mentions.sort_by_key(|mention| (mention.span.start, mention.span.end));
    mentions
}

impl NormalizedText {
    fn new(text: &str) -> Self {
        let mut normalized = Self {
            chars: Vec::new(),
            sources: Vec::new(),
            separated: Vec::new(),
        };
        let mut separated = true;
        for cluster in clusters(text) {
            let key = normalize_key(&text[cluster.clone()]);
            if key.is_empty() {
                separated = true;
                continue;
            }
            for (offset, ch) in key.chars().enumerate() {
                normalized.chars.push(ch);
                normalized.sources.push(cluster.clone());
                normalized.separated.push(offset == 0 && separated);
            }
            separated = false;
        }
        normalized
    }

    fn starts(&self, len: usize) -> impl Iterator<Item = usize> {
        self.chars
            .len()
            .checked_sub(len)
            .map(|last| 0..=last)
            .into_iter()
            .flatten()
    }

    fn is_whole_word_span(&self, span: &Range<usize>) -> bool {
        !self.splits_ascii_word_at(span.start) && !self.splits_ascii_word_at(span.end)
    }

    fn splits_ascii_word_at(&self, at: usize) -> bool {
        at > 0
            && at < self.chars.len()
            && !self.separated[at]
            && self.chars[at - 1].is_ascii_alphanumeric()
            && self.chars[at].is_ascii_alphanumeric()
    }

    fn original<'a>(&self, text: &'a str, span: &Range<usize>) -> &'a str {
        &text[self.sources[span.start].start..self.sources[span.end - 1].end]
    }
}

fn clusters(text: &str) -> Vec<Range<usize>> {
    let mut clusters: Vec<Range<usize>> = Vec::new();
    for (start, ch) in text.char_indices() {
        let end = start + ch.len_utf8();
        match clusters.last_mut() {
            Some(last) if continues_cluster(ch) => last.end = end,
            _ => clusters.push(start..end),
        }
    }
    clusters
}

fn continues_cluster(ch: char) -> bool {
    let mut leading = None;
    decompose_compatible(ch, |part| {
        if leading.is_none() {
            leading = Some(part);
        }
    });
    leading.is_some_and(|part| canonical_combining_class(part) != 0)
}

fn find_occurrences(text: &NormalizedText, catalog: &NameCatalog) -> Vec<Occurrence> {
    let mut found = Vec::new();
    for (entry, catalog_entry) in catalog.entries().iter().enumerate() {
        let name = catalog_entry.key.as_slice();
        if name.len() >= MIN_EXACT_NAME_CHARS {
            for start in text.starts(name.len()) {
                let span = start..start + name.len();
                if text.chars[span.clone()] == *name && text.is_whole_word_span(&span) {
                    found.push(Occurrence {
                        entry,
                        span,
                        matched: MentionMatch::Exact,
                        keeps_class: true,
                    });
                }
            }
        }
        if name.len() >= MIN_TYPO_NAME_CHARS {
            for len in name.len() - 1..=name.len() + 1 {
                for start in text.starts(len) {
                    let span = start..start + len;
                    if !text.is_whole_word_span(&span) {
                        continue;
                    }
                    let written = &text.chars[span.clone()];
                    if let Some(edit) = single_typo_edit(written, name) {
                        found.push(Occurrence {
                            entry,
                            span,
                            matched: MentionMatch::Typo,
                            keeps_class: keeps_char_class(edit, written, name),
                        });
                    }
                }
            }
        }
    }
    found
}

fn single_typo_edit(written: &[char], name: &[char]) -> Option<Edit> {
    let common = written
        .iter()
        .zip(name)
        .take_while(|(written, name)| written == name)
        .count();
    match written.len().cmp(&name.len()) {
        Ordering::Equal if common == written.len() => None,
        Ordering::Equal if written[common + 1..] == name[common + 1..] => {
            Some(Edit::Substitution(common))
        }
        Ordering::Equal => is_adjacent_swap(written, name, common).then_some(Edit::Transposition),
        Ordering::Greater => (written.len() == name.len() + 1
            && written[common + 1..] == name[common..])
            .then_some(Edit::Insertion(common)),
        Ordering::Less => (written.len() + 1 == name.len()
            && written[common..] == name[common + 1..])
            .then_some(Edit::Deletion),
    }
}

fn keeps_char_class(edit: Edit, written: &[char], name: &[char]) -> bool {
    match edit {
        Edit::Substitution(at) => char_class(written[at]) == char_class(name[at]),
        Edit::Insertion(at) => {
            let inserted = char_class(written[at]);
            [at.checked_sub(1), Some(at)]
                .into_iter()
                .flatten()
                .filter_map(|index| name.get(index))
                .any(|neighbor| char_class(*neighbor) == inserted)
        }
        Edit::Deletion | Edit::Transposition => true,
    }
}

fn char_class(ch: char) -> CharClass {
    match ch {
        _ if ch.is_ascii_alphanumeric() => CharClass::AsciiAlphanumeric,
        '\u{3041}'..='\u{309F}' => CharClass::Hiragana,
        '\u{30A0}'..='\u{30FF}' | '\u{31F0}'..='\u{31FF}' => CharClass::Katakana,
        '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}' => {
            CharClass::Ideograph
        }
        '\u{20000}'..='\u{3FFFF}' => CharClass::Ideograph,
        _ => CharClass::Other,
    }
}

fn is_adjacent_swap(written: &[char], name: &[char], at: usize) -> bool {
    at + 1 < written.len()
        && written[at] == name[at + 1]
        && written[at + 1] == name[at]
        && written[at + 2..] == name[at + 2..]
}

fn longest_exact_occurrences(occurrences: &[Occurrence]) -> Vec<&Occurrence> {
    let exact: Vec<&Occurrence> = occurrences
        .iter()
        .filter(|occurrence| occurrence.matched == MentionMatch::Exact)
        .collect();
    exact
        .iter()
        .copied()
        .filter(|occurrence| {
            !exact
                .iter()
                .any(|other| contains_longer(&other.span, &occurrence.span))
        })
        .collect()
}

fn contains_longer(outer: &Range<usize>, inner: &Range<usize>) -> bool {
    outer.start <= inner.start && inner.end <= outer.end && outer.len() > inner.len()
}

fn overlaps(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

fn exact_mentions(
    text: &str,
    normalized: &NormalizedText,
    catalog: &NameCatalog,
    exact: &[&Occurrence],
) -> Vec<NameMention> {
    let mut mentions: Vec<NameMention> = Vec::new();
    for occurrence in exact {
        let target = &catalog.entries()[occurrence.entry].target;
        match mentions
            .iter_mut()
            .find(|mention| mention.span == occurrence.span)
        {
            Some(mention) => add_target(mention, target),
            None => mentions.push(mention_at(
                text,
                normalized,
                occurrence.span.clone(),
                MentionMatch::Exact,
                target,
            )),
        }
    }
    mentions
}

fn typo_mentions(
    text: &str,
    normalized: &NormalizedText,
    catalog: &NameCatalog,
    mut typos: Vec<&Occurrence>,
) -> Vec<NameMention> {
    typos.sort_by_key(|occurrence| (occurrence.span.start, occurrence.span.end));
    let mut groups: Vec<(usize, Vec<&Occurrence>)> = Vec::new();
    for occurrence in typos {
        match groups.last_mut() {
            Some((group_end, members)) if occurrence.span.start < *group_end => {
                *group_end = (*group_end).max(occurrence.span.end);
                members.push(occurrence);
            }
            _ => groups.push((occurrence.span.end, vec![occurrence])),
        }
    }
    groups
        .into_iter()
        .filter_map(|(_, members)| {
            let readings = plausible_readings(members);
            let span = written_span(&readings)?;
            let mut mention = NameMention {
                surface: normalized.original(text, &span).to_string(),
                key: normalized.chars[span.clone()].iter().collect(),
                matched: MentionMatch::Typo,
                targets: Vec::new(),
                span,
            };
            for reading in readings {
                add_target(&mut mention, &catalog.entries()[reading.entry].target);
            }
            Some(mention)
        })
        .collect()
}

fn plausible_readings(members: Vec<&Occurrence>) -> Vec<&Occurrence> {
    if members.iter().any(|member| member.keeps_class) {
        members
            .into_iter()
            .filter(|member| member.keeps_class)
            .collect()
    } else {
        members
    }
}

fn written_span(readings: &[&Occurrence]) -> Option<Range<usize>> {
    let keeps_class = readings.iter().all(|reading| reading.keeps_class);
    let spans = readings.iter().map(|reading| reading.span.clone());
    if keeps_class {
        spans.min_by_key(|span| (Reverse(span.len()), span.start))
    } else {
        spans.min_by_key(|span| (span.len(), span.start))
    }
}

fn mention_at(
    text: &str,
    normalized: &NormalizedText,
    span: Range<usize>,
    matched: MentionMatch,
    target: &NameTarget,
) -> NameMention {
    NameMention {
        surface: normalized.original(text, &span).to_string(),
        key: normalized.chars[span.clone()].iter().collect(),
        matched,
        targets: vec![target.clone()],
        span,
    }
}

fn add_target(mention: &mut NameMention, target: &NameTarget) {
    let known = mention
        .targets
        .iter()
        .any(|existing| existing.kind == target.kind && existing.id == target.id);
    if !known {
        mention.targets.push(target.clone());
    }
}

#[cfg(test)]
mod tests;

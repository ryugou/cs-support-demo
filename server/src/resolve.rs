use unicode_normalization::UnicodeNormalization;

pub fn normalize_key(input: &str) -> String {
    input
        .nfkc()
        .flat_map(char::to_lowercase)
        .filter(|ch| ch.is_alphanumeric())
        .collect()
}

/// needle を正規化してから、正規化済み haystack に部分一致するか（空 needle は不一致）。
pub fn norm_contains(haystack_norm: &str, needle_raw: &str) -> bool {
    let needle = normalize_key(needle_raw);
    !needle.is_empty() && haystack_norm.contains(&needle)
}

pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0; b.len() + 1];

    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (curr[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[b.len()]
}

pub fn fuzzy_score(query: &str, candidate: &str) -> f32 {
    let q = normalize_key(query);
    let c = normalize_key(candidate);
    if q.is_empty() || c.is_empty() {
        return 0.0;
    }
    if c.contains(&q) || q.contains(&c) {
        return 1.0;
    }
    let dist = levenshtein(&q, &c);
    let max_len = q.chars().count().max(c.chars().count()) as f32;
    (1.0 - (dist as f32 / max_len)).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_width_case_and_symbols() {
        assert_eq!(normalize_key("ＨＢ-100 mini"), "hb100mini");
    }

    #[test]
    fn scores_partial_match() {
        assert_eq!(fuzzy_score("HB100", "HB-100"), 1.0);
    }
}

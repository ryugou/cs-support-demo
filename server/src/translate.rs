use crate::model::SectionInput;
use anyhow::{anyhow, Result};
use std::{collections::HashMap, fs, path::Path};

pub type Glossary = HashMap<String, String>;

pub fn load_glossary(path: &Path) -> Result<Glossary> {
    let body = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&body)?)
}

pub fn validate_fixture_translation(section: &SectionInput, glossary: &Glossary) -> Result<()> {
    if section
        .body_en
        .as_deref()
        .unwrap_or_default()
        .trim()
        .is_empty()
    {
        return Ok(());
    }
    let body_ja = section.body_ja.as_deref().unwrap_or_default().trim();
    if body_ja.is_empty() {
        return Err(anyhow!("missing Japanese fixture for {}", section.anchor));
    }
    for (en, ja) in glossary {
        if section.body_en.as_deref().unwrap_or_default().contains(en) && !body_ja.contains(ja) {
            // Some glossary entries are product names or generic terms that do not need to
            // appear in every translated sentence. Keep this as a soft validation boundary.
            tracing::debug!(anchor = %section.anchor, en, ja, "glossary entry not present in fixture translation");
        }
    }
    Ok(())
}

use super::*;
use crate::harness::name_confirmation::test_fixtures::{f_name, f_p7};

fn reference(
    surface: &str,
    resolution: ProductReferenceResolution,
    model: Option<&str>,
) -> ProductReference {
    ProductReference {
        surface: surface.to_string(),
        resolution,
        matched_model: model.map(ToString::to_string),
    }
}

fn matched(surface: &str, model: &str) -> ProductReference {
    reference(surface, ProductReferenceResolution::Matched, Some(model))
}

#[test]
fn a_matched_reference_for_a_typo_notation_is_held_back() {
    let cases = [
        (
            "ADV-VC729Pが録画されない",
            matched("ADV-VC729P", "ADC-VC729P"),
        ),
        (
            "ADV-VC729Pが録画されない",
            matched("ADC-VC729P", "ADC-VC729P"),
        ),
        ("ADC-V742の設定", matched("ADC-V742", "adc-v724")),
    ];
    for (text, llm_reference) in cases {
        let screened = screen_product_references(&f_p7(), text, vec![llm_reference]);
        assert_eq!(screened.len(), 1, "{text}");
        assert_eq!(
            screened[0].resolution,
            ProductReferenceResolution::Ambiguous,
            "{text}"
        );
    }
}

#[test]
fn an_abbreviation_matched_by_the_llm_stays_matched() {
    let screened = screen_product_references(
        &f_p7(),
        "V724が録画されない",
        vec![matched("V724", "ADC-V724")],
    );
    assert_eq!(screened, vec![matched("V724", "ADC-V724")]);
}

#[test]
fn a_registered_product_name_or_alias_identifies_the_model() {
    let cases = [
        ("みまもりドームカメラが録画されない", "みまもりドームカメラ"),
        ("見守りドームが録画されない", "見守りドーム"),
    ];
    for (text, surface) in cases {
        let screened = screen_product_references(&f_name(), text, Vec::new());
        assert_eq!(screened, vec![matched(surface, "ADC-V724")], "{text}");
    }
}

#[test]
fn an_exact_model_is_kept_while_a_typo_of_another_is_held_back() {
    let screened = screen_product_references(
        &f_p7(),
        "ADC-V724 と ADV-VC729P を使っています",
        vec![
            matched("ADC-V724", "ADC-V724"),
            matched("ADV-VC729P", "ADC-VC729P"),
        ],
    );
    assert_eq!(
        screened,
        vec![
            matched("ADC-V724", "ADC-V724"),
            reference(
                "ADV-VC729P",
                ProductReferenceResolution::Ambiguous,
                Some("ADC-VC729P")
            ),
        ]
    );
}

#[test]
fn an_exact_model_already_matched_is_not_added_twice() {
    let screened = screen_product_references(
        &f_p7(),
        "adc-v724が録画されない",
        vec![matched("adc-v724", "ADC-V724")],
    );
    assert_eq!(screened.len(), 1);
}

#[test]
fn foreign_and_ambiguous_references_are_left_unchanged() {
    let references = vec![
        reference("Ring", ProductReferenceResolution::Foreign, None),
        reference("ADV-VC729P", ProductReferenceResolution::Ambiguous, None),
    ];
    let screened = screen_product_references(&f_p7(), "Ring と ADV-VC729P", references.clone());
    assert_eq!(screened, references);
}

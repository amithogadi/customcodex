use super::*;
use pretty_assertions::assert_eq;

#[test]
fn empty_catalog_has_no_fragment() {
    let rendered = render_catalog(
        &SkillCatalog::default(),
        /*include_skills_usage_instructions*/ false,
        SkillCatalogRenderPolicy::ExtensionCompatible,
        SkillMetadataBudget::Characters(8_000),
    );

    assert!(rendered.fragment.is_none());
    assert_eq!(rendered.warning_message, None);
}

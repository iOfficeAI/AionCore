const PRODUCT_BRAND: &str = "CSBU WorkMate";

/// Replace legacy product names in user- and agent-facing prose while
/// preserving compatibility identifiers such as repository URLs and paths.
pub fn normalize_product_brand_text(value: &str) -> String {
    let normalized = value
        .replace("Powered by @aionui", &format!("Powered by {PRODUCT_BRAND}"))
        .replace("Aion CLI", PRODUCT_BRAND)
        .replace("Aion Assistant", PRODUCT_BRAND)
        .replace("Aion UI", PRODUCT_BRAND);
    let normalized = replace_prose_token(&normalized, "AionUI");
    replace_prose_token(&normalized, "AionUi")
}

fn replace_prose_token(value: &str, token: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;

    for (start, _) in value.match_indices(token) {
        let end = start + token.len();
        let previous = value[..start].chars().next_back();
        let next = value[end..].chars().next();
        let embedded_in_technical_identifier =
            previous.is_some_and(is_technical_prefix) || next.is_some_and(is_technical_suffix);

        output.push_str(&value[cursor..start]);
        if embedded_in_technical_identifier {
            output.push_str(token);
        } else {
            output.push_str(PRODUCT_BRAND);
        }
        cursor = end;
    }

    output.push_str(&value[cursor..]);
    output
}

fn is_technical_prefix(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '/' | '\\' | '@' | '_' | '-' | '%')
}

fn is_technical_suffix(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '/' | '\\' | '_')
}

#[cfg(test)]
mod tests {
    use super::normalize_product_brand_text;

    #[test]
    fn replaces_legacy_names_in_prose() {
        let value = "Aion CLI, Aion Assistant, Aion UI, AionUI, AionUi and Powered by @aionui";
        assert_eq!(
            normalize_product_brand_text(value),
            "CSBU WorkMate, CSBU WorkMate, CSBU WorkMate, CSBU WorkMate, CSBU WorkMate and Powered by CSBU WorkMate"
        );
    }

    #[test]
    fn replaces_brand_next_to_non_ascii_prose() {
        assert_eq!(normalize_product_brand_text("使用AionUi管家"), "使用CSBU WorkMate管家");
    }

    #[test]
    fn preserves_compatibility_urls_paths_and_identifiers() {
        let value = "https://github.com/iOfficeAI/AionUi /Applications/AionUi.app AionUi/scripts @AionUI/plugin";
        assert_eq!(normalize_product_brand_text(value), value);
    }
}

//! Serde helpers for tolerating LLM output where a `Vec<String>` field
//! is sometimes sent as a single string (e.g. `"*.rs"`) instead of an
//! array (e.g. `["*.rs"]`).

/// Deserializes a JSON value into `Vec<String>`, accepting either
/// a single string (`"*.rs"`) or an array of strings (`["*.rs", "src/**/*.toml"]`).
///
/// This is used on tool-args fields that LLMs sometimes "flatten" from
/// a single-element array into a bare string.
pub fn deserialize_string_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct StringListVisitor;

    impl<'v> serde::de::Visitor<'v> for StringListVisitor {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a string or an array of strings")
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(vec![v.to_string()])
        }

        fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(vec![v])
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'v>,
        {
            let mut items = Vec::new();
            while let Some(item) = serde::de::SeqAccess::next_element::<String>(&mut seq)? {
                items.push(item);
            }
            Ok(items)
        }
    }

    deserializer.deserialize_any(StringListVisitor)
}

/// Deserializes a JSON value into `Option<Vec<String>>`, accepting
/// `null`, a single string, or an array of strings.
pub fn deserialize_optional_string_list<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct OptStringListVisitor;

    impl<'v> serde::de::Visitor<'v> for OptStringListVisitor {
        type Value = Option<Vec<String>>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("null, a string, or an array of strings")
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(Some(vec![v.to_string()]))
        }

        fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(Some(vec![v]))
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'v>,
        {
            let mut items = Vec::new();
            while let Some(item) = serde::de::SeqAccess::next_element::<String>(&mut seq)? {
                items.push(item);
            }
            Ok(Some(items))
        }
    }

    deserializer.deserialize_any(OptStringListVisitor)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Integration: verify the deserializers work on real struct fields via
    // `#[serde(deserialize_with)]`.
    #[derive(serde::Deserialize)]
    struct TestArgs {
        #[serde(default, deserialize_with = "deserialize_string_list")]
        include: Vec<String>,
        #[serde(default, deserialize_with = "deserialize_optional_string_list")]
        extends: Option<Vec<String>>,
    }

    #[test]
    fn struct_fields_accept_single_string() {
        let args: TestArgs = serde_json::from_value(serde_json::json!({
            "include": "*.rs",
            "extends": "base"
        }))
        .unwrap();
        assert_eq!(args.include, vec!["*.rs".to_string()]);
        assert_eq!(args.extends, Some(vec!["base".to_string()]));
    }

    #[test]
    fn struct_fields_accept_array() {
        let args: TestArgs = serde_json::from_value(serde_json::json!({
            "include": ["*.rs", "*.toml"],
            "extends": ["a", "b"]
        }))
        .unwrap();
        assert_eq!(args.include, vec!["*.rs".to_string(), "*.toml".to_string()]);
        assert_eq!(args.extends, Some(vec!["a".to_string(), "b".to_string()]));
    }

    #[test]
    fn struct_fields_missing_defaults() {
        let args: TestArgs = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(args.include.is_empty());
        assert!(args.extends.is_none());
    }

    #[test]
    fn struct_fields_rejects_number() {
        let result: Result<TestArgs, _> =
            serde_json::from_value(serde_json::json!({ "include": 42 }));
        assert!(result.is_err());
    }
}

use serde::Serialize;

/// Serialize a value to pretty-printed JSON.
pub fn to_json<T: Serialize>(data: &T) -> String {
    serde_json::to_string_pretty(data).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

/// Serialize a value to compact JSON.
pub fn to_json_compact<T: Serialize>(data: &T) -> String {
    serde_json::to_string(data).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_to_json_pretty() {
        let mut map = HashMap::new();
        map.insert("key", "value");
        let json = to_json(&map);
        assert!(json.contains("\"key\""));
        assert!(json.contains("\"value\""));
        assert!(json.contains('\n'));
    }

    #[test]
    fn test_to_json_compact() {
        let mut map = HashMap::new();
        map.insert("key", "value");
        let json = to_json_compact(&map);
        assert!(json.contains("\"key\""));
        assert!(!json.contains('\n'));
    }
}

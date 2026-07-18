//! Secret redaction helpers to reduce accidental API key / token leakage
//! into collected Markdown, LLM prompts, and error messages.

use std::sync::LazyLock;

use regex::Regex;

const REDACTED: &str = "[REDACTED]";

/// Patterns that commonly appear when users paste credentials into chats
/// or when APIs echo secrets back in error bodies.
static SECRET_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // More specific provider prefixes first.
        r"(?i)\bsk-ant-[a-z0-9_\-]{16,}\b",
        r"(?i)\bsk-proj-[a-z0-9_\-]{16,}\b",
        r"(?i)\bsk-[a-z0-9_\-]{16,}\b",
        r"\b(ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}\b",
        r"\bgithub_pat_[A-Za-z0-9_]{20,}\b",
        r"\bAIza[0-9A-Za-z_\-]{20,}\b",
        r"\bxox[baprs]-[A-Za-z0-9\-]{10,}\b",
        r"\bAKIA[0-9A-Z]{16}\b",
        r"(?i)\bBearer\s+[A-Za-z0-9\-._~+/]+=*\b",
        r#"(?i)(?:api[_-]?key|secret[_-]?key|access[_-]?token|auth[_-]?token)\s*[=:]\s*['"]?[A-Za-z0-9_\-./+]{16,}['"]?"#,
        r"-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----[\s\S]*?-----END (?:RSA |EC |OPENSSH )?PRIVATE KEY-----",
    ]
    .into_iter()
    .map(|p| Regex::new(p).expect("valid secret redaction regex"))
    .collect()
});

/// Replace likely secrets in `input` with `[REDACTED]`.
pub fn redact_secrets(input: &str) -> String {
    let mut out = input.to_string();
    for re in SECRET_PATTERNS.iter() {
        out = re.replace_all(&out, REDACTED).into_owned();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::redact_secrets;

    #[test]
    fn redacts_openai_style_keys() {
        let s = "key is sk-abcdefghijklmnopqrstuvwxyz012345 please keep";
        let out = redact_secrets(s);
        assert!(!out.contains("sk-abcdefghijklmnopqrstuvwxyz012345"));
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn redacts_anthropic_and_github_tokens() {
        let s = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz and ghp_abcdefghijklmnopqrstuvwxyzABCD";
        let out = redact_secrets(s);
        assert!(!out.contains("sk-ant-"));
        assert!(!out.contains("ghp_"));
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn redacts_assignment_style_secrets() {
        let s = r#"export OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz012345"#;
        let out = redact_secrets(s);
        assert!(!out.contains("sk-abcdefghijklmnopqrstuvwxyz012345"));
    }

    #[test]
    fn leaves_short_placeholders_alone() {
        // README / docs use sk-... ; unit tests use short fakes like sk-x.
        assert_eq!(redact_secrets("export KEY=sk-..."), "export KEY=sk-...");
        assert_eq!(redact_secrets("sk-x"), "sk-x");
        assert_eq!(redact_secrets("sk-test-123"), "sk-test-123");
    }

    #[test]
    fn redacts_private_key_blocks() {
        let pem = "-----BEGIN PRIVATE KEY-----\nMIIEvgIBADANBg==\n-----END PRIVATE KEY-----";
        let out = redact_secrets(&format!("here {pem} there"));
        assert!(!out.contains("MIIEvgIBADANBg"));
        assert!(out.contains("[REDACTED]"));
    }
}

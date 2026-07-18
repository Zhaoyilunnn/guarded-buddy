//! Weekly report template: built-in Chinese default template + lenient `{{placeholder}}` substitution from custom files.

/// Default weekly report template. Placeholders: `{{start_date}}` `{{end_date}}` `{{stats}}` `{{daily_notes}}`.
pub const DEFAULT_TEMPLATE: &str = r#"你是一名资深软件工程师的助理。以下是我在 {{start_date}} 至 {{end_date}} 一周内与多个 AI 编程助手（Codex / Cursor / Claude Code / Gemini）的对话记录统计与每日记录。请据此生成一份高质量的中文周报。

## 本周统计
{{stats}}

## 每日对话记录
{{daily_notes}}

## 输出要求
请严格按照以下结构输出 Markdown，不要输出任何额外解释：

# 周报（{{start_date}} ~ {{end_date}}）

## 一、本周工作概述
用 3–6 句话概括本周完成的主要工作、涉及的项目与取得的成果。

## 二、详细开发记录
按项目或主题分点列出具体工作：做了什么、使用了哪些技术、解决了什么问题。保留关键技术名词、文件名与命令。合并同类工作，按重要程度排序。

## 三、关键技术决策
列出本周做出的技术选型或架构决策及其原因；如无，写"无"。

## 四、遗留问题与下周计划
列出本周未解决的问题、遗留的 TODO，以及据此推断的下周计划。推断内容请标注"（推断）"。

注意：仅基于以上对话记录撰写，不要编造记录中不存在的内容；语言简洁专业。
"#;

/// Template rendering context.
pub struct TemplateContext {
    pub start_date: String,
    pub end_date: String,
    pub stats: String,
    pub daily_notes: String,
}

/// Lenient placeholder engine: replaces only the four known placeholders; unknown `{{key}}` values are left as-is.
pub struct TemplateEngine {
    pub(crate) template: String,
}

impl TemplateEngine {
    pub fn default_template() -> Self {
        Self {
            template: DEFAULT_TEMPLATE.to_string(),
        }
    }

    pub fn from_file(path: &std::path::Path) -> Result<Self, TemplateError> {
        let template = std::fs::read_to_string(path).map_err(|source| TemplateError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self { template })
    }

    pub fn render(&self, ctx: &TemplateContext) -> String {
        self.template
            .replace("{{start_date}}", &ctx.start_date)
            .replace("{{end_date}}", &ctx.end_date)
            .replace("{{stats}}", &ctx.stats)
            .replace("{{daily_notes}}", &ctx.daily_notes)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TemplateError {
    #[error("cannot read template file {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn ctx() -> TemplateContext {
        TemplateContext {
            start_date: "2026-07-12".to_string(),
            end_date: "2026-07-18".to_string(),
            stats: "- 会话 10 个".to_string(),
            daily_notes: "# 2026-07-15\n...".to_string(),
        }
    }

    #[test]
    fn default_template_contains_all_placeholders_and_sections() {
        let tpl = TemplateEngine::default_template();
        for ph in ["{{start_date}}", "{{end_date}}", "{{stats}}", "{{daily_notes}}"] {
            assert!(tpl.template.contains(ph), "missing placeholder {ph}");
        }
        for section in [
            "本周工作概述",
            "详细开发记录",
            "关键技术决策",
            "遗留问题与下周计划",
        ] {
            assert!(tpl.template.contains(section), "missing section {section}");
        }
    }

    #[test]
    fn render_substitutes_all_known_placeholders() {
        let engine = TemplateEngine {
            template: "从 {{start_date}} 到 {{end_date}}\n{{stats}}\n{{daily_notes}}".to_string(),
        };
        let out = engine.render(&ctx());
        assert_eq!(
            out,
            "从 2026-07-12 到 2026-07-18\n- 会话 10 个\n# 2026-07-15\n..."
        );
    }

    #[test]
    fn render_leaves_unknown_placeholders_untouched() {
        let engine = TemplateEngine {
            template: "{{unknown}} and {{start_date}}".to_string(),
        };
        assert_eq!(engine.render(&ctx()), "{{unknown}} and 2026-07-12");
    }

    #[test]
    fn render_replaces_multiple_occurrences() {
        let engine = TemplateEngine {
            template: "{{start_date}} ~ {{start_date}}".to_string(),
        };
        assert_eq!(engine.render(&ctx()), "2026-07-12 ~ 2026-07-12");
    }

    #[test]
    fn from_file_reads_template() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "自定义 {{{{start_date}}}}").unwrap();
        let engine = TemplateEngine::from_file(f.path()).unwrap();
        assert_eq!(engine.render(&ctx()), "自定义 2026-07-12");
    }

    #[test]
    fn from_file_missing_returns_err() {
        let path = std::path::Path::new("/nonexistent/template.md");
        assert!(TemplateEngine::from_file(path).is_err());
    }
}

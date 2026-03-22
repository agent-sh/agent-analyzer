//! Language distribution detection by file extension.

use std::collections::HashMap;
use std::path::Path;

use analyzer_core::types::LanguageInfo;
use analyzer_core::walk;

struct LangExt {
    ext: &'static str,
    lang: &'static str,
}

static LANGUAGE_EXTENSIONS: &[LangExt] = &[
    LangExt {
        ext: "rs",
        lang: "Rust",
    },
    LangExt {
        ext: "ts",
        lang: "TypeScript",
    },
    LangExt {
        ext: "tsx",
        lang: "TypeScript",
    },
    LangExt {
        ext: "js",
        lang: "JavaScript",
    },
    LangExt {
        ext: "jsx",
        lang: "JavaScript",
    },
    LangExt {
        ext: "mjs",
        lang: "JavaScript",
    },
    LangExt {
        ext: "cjs",
        lang: "JavaScript",
    },
    LangExt {
        ext: "py",
        lang: "Python",
    },
    LangExt {
        ext: "go",
        lang: "Go",
    },
    LangExt {
        ext: "java",
        lang: "Java",
    },
    LangExt {
        ext: "rb",
        lang: "Ruby",
    },
    LangExt {
        ext: "php",
        lang: "PHP",
    },
    LangExt {
        ext: "cs",
        lang: "C#",
    },
    LangExt {
        ext: "cpp",
        lang: "C++",
    },
    LangExt {
        ext: "cc",
        lang: "C++",
    },
    LangExt {
        ext: "c",
        lang: "C",
    },
    LangExt {
        ext: "h",
        lang: "C",
    },
    LangExt {
        ext: "swift",
        lang: "Swift",
    },
    LangExt {
        ext: "kt",
        lang: "Kotlin",
    },
    LangExt {
        ext: "scala",
        lang: "Scala",
    },
    LangExt {
        ext: "sh",
        lang: "Shell",
    },
    LangExt {
        ext: "bash",
        lang: "Shell",
    },
    LangExt {
        ext: "zsh",
        lang: "Shell",
    },
    LangExt {
        ext: "css",
        lang: "CSS",
    },
    LangExt {
        ext: "scss",
        lang: "SCSS",
    },
    LangExt {
        ext: "html",
        lang: "HTML",
    },
    LangExt {
        ext: "vue",
        lang: "Vue",
    },
    LangExt {
        ext: "svelte",
        lang: "Svelte",
    },
    LangExt {
        ext: "sql",
        lang: "SQL",
    },
    LangExt {
        ext: "lua",
        lang: "Lua",
    },
    LangExt {
        ext: "zig",
        lang: "Zig",
    },
    LangExt {
        ext: "ex",
        lang: "Elixir",
    },
    LangExt {
        ext: "exs",
        lang: "Elixir",
    },
    LangExt {
        ext: "erl",
        lang: "Erlang",
    },
    LangExt {
        ext: "hs",
        lang: "Haskell",
    },
    LangExt {
        ext: "clj",
        lang: "Clojure",
    },
    LangExt {
        ext: "dart",
        lang: "Dart",
    },
];

/// Build a lookup map from extension to language name.
fn ext_to_lang() -> HashMap<&'static str, &'static str> {
    LANGUAGE_EXTENSIONS
        .iter()
        .map(|e| (e.ext, e.lang))
        .collect()
}

/// Detect language distribution by counting source files.
pub fn detect_languages(repo_path: &Path) -> Vec<LanguageInfo> {
    let ext_map = ext_to_lang();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut total = 0usize;

    walk::walk_files(repo_path, |path| {
        let rel = path
            .strip_prefix(repo_path)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if walk::is_noise(&rel) {
            return;
        }
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if let Some(lang) = ext_map.get(ext) {
                *counts.entry(lang).or_default() += 1;
                total += 1;
            }
        }
    })
    .ok();

    if total == 0 {
        return vec![];
    }

    let mut langs: Vec<LanguageInfo> = counts
        .into_iter()
        .map(|(lang, count)| LanguageInfo {
            language: lang.to_string(),
            file_count: count,
            percentage: (count as f64 / total as f64) * 100.0,
        })
        .collect();

    langs.sort_by(|a, b| b.file_count.cmp(&a.file_count));
    langs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_languages() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub fn lib() {}").unwrap();
        std::fs::write(dir.path().join("app.ts"), "export {}").unwrap();

        let langs = detect_languages(dir.path());
        assert!(langs.len() >= 2);
        assert_eq!(langs[0].language, "Rust");
        assert_eq!(langs[0].file_count, 2);
    }

    #[test]
    fn test_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let langs = detect_languages(dir.path());
        assert!(langs.is_empty());
    }
}

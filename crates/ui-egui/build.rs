//! Bake the contributor credits into the binary as static Rust tables. Nothing is read at run time.
//!
//! - contributors/contributors.json: commit stats per GitHub username, with a mark for whose
//!   people.toml entry is verified.
//! - contributors/people.toml: the names people chose for themselves. A name is used only when the
//!   JSON says the entry was verified (`people_entry`).
//!
//! A missing or malformed file gives empty tables (or no names) and a warning.

use std::fmt::Write as _;

use serde_json::Value;

fn main() {
    if let Err(error) = catalog_inputs() {
        println!("cargo::error={error}");
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contributors");
    let json = read(&dir.join("contributors.json"), "the About window shows no contributors", |t| serde_json::from_str::<Value>(t).map_err(|e| e.to_string()))
        .unwrap_or(Value::Null);
    let people =
        read(&dir.join("people.toml"), "contributors are shown by username only", |t| t.parse::<toml::Table>().map_err(|e| e.to_string())).unwrap_or_default();
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default()).join("credits.rs");
    if let Err(e) = std::fs::write(&out, generate(&json, &people)) {
        println!("cargo::warning=writing {}: {e}", out.display());
    }
}

fn read<T>(path: &std::path::Path, fallback: &str, parse: impl Fn(&str) -> Result<T, String>) -> Option<T> {
    println!("cargo::rerun-if-changed={}", path.display());
    let r = std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(|t| parse(&t));
    if let Err(e) = &r {
        println!("cargo::warning={}: {e}; {fallback}", path.display());
    }
    r.ok()
}

/// A non-empty string field of someone's people.toml entry, as a Rust `Option<&str>` literal.
fn name(v: Option<&str>) -> String {
    match v.map(str::trim).filter(|s| !s.is_empty()) {
        Some(x) => format!("Some({x:?})"),
        None => "None".into(),
    }
}

fn s(v: &Value, k: &str) -> String {
    format!("{:?}", v.get(k).and_then(Value::as_str).unwrap_or(""))
}

fn n(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

fn rows(json: &Value, k: &str) -> Vec<Value> {
    json.get(k).and_then(Value::as_array).cloned().unwrap_or_default()
}

fn generate(json: &Value, people: &toml::Table) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "pub static TOTAL_COMMITS: u64 = {};", n(json, "commits"));
    let _ = writeln!(o, "pub static CONTRIBUTORS: &[Contributor] = &[");
    for c in rows(json, "contributors") {
        let login = c.get("login").and_then(Value::as_str).unwrap_or("");
        if login.is_empty() {
            continue;
        }
        // Names only from a verified, self-chosen people.toml entry.
        let entry = people.get("people").and_then(|p| p.get(login)).filter(|_| c.get("people_entry").and_then(Value::as_bool) == Some(true));
        let field = |k: &str| entry.and_then(|e| e.get(k)).and_then(toml::Value::as_str);
        let synced = entry.and_then(|e| e.get("sync_github_name")).and_then(toml::Value::as_bool) == Some(true);
        let display =
            field("display_name").filter(|s| !s.trim().is_empty()).or_else(|| if synced { c.get("github_name").and_then(Value::as_str) } else { None });
        let _ = writeln!(
            o,
            "    Contributor {{ login: {login:?}, display_name: {}, real_name: {}, prs: {}, commits: {}, lines_added: {}, lines_deleted: {}, binary_added: {}, binary_deleted: {}, first_commit: {}, last_commit: {} }},",
            name(display),
            name(field("real_name")),
            n(&c, "prs"),
            n(&c, "commits"),
            n(&c, "lines_added"),
            n(&c, "lines_deleted"),
            n(&c, "binary_added"),
            n(&c, "binary_deleted"),
            s(&c, "first_commit"),
            s(&c, "last_commit"),
        );
    }
    let _ = writeln!(o, "];");
    let _ = writeln!(o, "pub static MODELS: &[Model] = &[");
    for m in rows(json, "models") {
        let _ = writeln!(
            o,
            "    Model {{ company: {}, model: {}, version: {}, commits: {}, lines_added: {}, lines_deleted: {} }},",
            s(&m, "company"),
            s(&m, "model"),
            s(&m, "version"),
            n(&m, "commits"),
            n(&m, "lines_added"),
            n(&m, "lines_deleted"),
        );
    }
    let _ = writeln!(o, "];");
    o
}

fn catalog_inputs() -> Result<(), String> {
    use std::{fs, path::PathBuf};
    println!("cargo::rerun-if-env-changed=CRAFT_I18N_DIR");
    println!("cargo::rerun-if-env-changed=CRAFT_I18N_REQUIRED");
    let required = std::env::var("CRAFT_I18N_REQUIRED").is_ok_and(|value| value.trim() == "1");
    let selected = std::env::var_os("CRAFT_I18N_DIR").filter(|value| !value.is_empty()).map(PathBuf::from);
    if required && selected.is_none() {
        return Err("CRAFT_I18N_REQUIRED=1 needs CRAFT_I18N_DIR with FilmCraft catalogs".into());
    }
    if let Some(root) = &selected {
        println!("cargo::rerun-if-changed={}", root.join("locales").display());
    }
    let out = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is missing")?).join("i18n");
    if out.exists() {
        fs::remove_dir_all(&out).map_err(|error| format!("clearing generated catalogs {}: {error}", out.display()))?;
    }
    fs::create_dir_all(&out).map_err(|error| format!("creating generated catalogs {}: {error}", out.display()))?;
    for code in ["bs", "cs", "de", "es", "fr", "id", "it", "ja", "ko", "pl", "pt-br", "ru", "sv", "tr", "uk", "zh-cn"] {
        let canonical = if code == "zh-cn" { "zh-hans" } else { code };
        let external = selected.as_ref().map(|root| craft_i18n::build_catalog::read(root, "filmcraft", canonical)).transpose();
        let text = match external {
            Ok(Some(Some(rows))) => {
                let escape = |text: &str| text.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n");
                let mut text = String::new();
                for row in rows {
                    let _ = writeln!(text, "{}\t{}\t{}", escape(&row.context), escape(&row.source), escape(&row.translation));
                }
                text
            }
            unavailable => {
                let reason = match unavailable {
                    Err(error) => error,
                    _ => format!("FilmCraft {canonical} catalog is missing"),
                };
                if required {
                    return Err(format!("CRAFT_I18N_REQUIRED=1: {reason}"));
                }
                if selected.is_some() {
                    println!("cargo::warning={reason}; using the built-in {code} catalog");
                }
                let path = PathBuf::from("src/i18n").join(format!("{code}.tsv"));
                println!("cargo::rerun-if-changed={}", path.display());
                fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?
            }
        };
        fs::write(out.join(format!("{code}.tsv")), text).map_err(|error| format!("writing the {code} catalog: {error}"))?;
    }
    Ok(())
}

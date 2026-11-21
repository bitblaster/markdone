//! Spell checking: Hunspell dictionaries read by `spellbook`, several languages at once
//! (a word is right if any of them accepts it) plus a personal dictionary.
//!
//! Dictionaries come from the system on Linux (`/usr/share/hunspell`, as installed by the
//! distribution's `hunspell-*` packages); a language the system lacks — and every
//! language on Windows and macOS — is downloaded from LibreOffice's dictionary
//! repository the first time it is enabled, into the user data folder.
//!
//! The active speller is process-wide: the line renderer asks it which words to
//! underline, and [`generation`] changes whenever the dictionaries or the personal words
//! change, so cached line renders are rebuilt.

use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use unicode_segmentation::UnicodeSegmentation;

/// A language offered in the Preferences.
pub struct Language {
    /// The Hunspell code, e.g. `it_IT` (also the file name of a system dictionary).
    pub code: &'static str,
    /// Its name, as shown.
    pub name: &'static str,
    /// Where LibreOffice's repository keeps it: `<dir>/<file>.aff` and `.dic`.
    dir: &'static str,
    file: &'static str,
}

/// The languages that can be enabled.
pub const LANGUAGES: &[Language] = &[
    Language {
        code: "en_US",
        name: "English (United States)",
        dir: "en",
        file: "en_US",
    },
    Language {
        code: "en_GB",
        name: "English (United Kingdom)",
        dir: "en",
        file: "en_GB",
    },
    Language {
        code: "it_IT",
        name: "Italiano",
        dir: "it_IT",
        file: "it_IT",
    },
    Language {
        code: "fr_FR",
        name: "Français",
        dir: "fr_FR/dictionaries",
        file: "fr",
    },
    Language {
        code: "de_DE",
        name: "Deutsch",
        dir: "de",
        file: "de_DE_frami",
    },
    Language {
        code: "es_ES",
        name: "Español",
        dir: "es",
        file: "es_ES",
    },
    Language {
        code: "pt_PT",
        name: "Português (Portugal)",
        dir: "pt_PT",
        file: "pt_PT",
    },
    Language {
        code: "pt_BR",
        name: "Português (Brasil)",
        dir: "pt_BR",
        file: "pt_BR",
    },
    Language {
        code: "nl_NL",
        name: "Nederlands",
        dir: "nl_NL",
        file: "nl_NL",
    },
];

/// The repository revision the dictionaries are downloaded from (pinned, so a download
/// is reproducible).
const REPOSITORY: &str = "https://raw.githubusercontent.com/LibreOffice/dictionaries/c0865a85eb67dc377f2cf8e8fe2c4fa9e53f0dcb";

/// The language with this code.
pub fn language(code: &str) -> Option<&'static Language> {
    LANGUAGES.iter().find(|l| l.code == code)
}

/// The URLs of a language's `.aff` and `.dic` files.
pub fn download_urls(lang: &Language) -> [String; 2] {
    ["aff", "dic"].map(|ext| format!("{REPOSITORY}/{}/{}.{ext}", lang.dir, lang.file))
}

/// The folder downloaded dictionaries are kept in.
pub fn download_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("markdone").join("dictionaries"))
}

/// The `.aff`/`.dic` paths of a downloaded language (they may not exist yet).
pub fn downloaded_paths(code: &str) -> Option<[PathBuf; 2]> {
    let dir = download_dir()?;
    Some(["aff", "dic"].map(|ext| dir.join(format!("{code}.{ext}"))))
}

/// The system's dictionary for `code`, when installed (Linux only).
pub fn system_paths(code: &str) -> Option<[PathBuf; 2]> {
    if !cfg!(all(unix, not(target_os = "macos"))) {
        return None;
    }
    [
        "/usr/share/hunspell",
        "/usr/share/myspell/dicts",
        "/usr/share/myspell",
    ]
    .iter()
    .map(|dir| ["aff", "dic"].map(|ext| Path::new(dir).join(format!("{code}.{ext}"))))
    .find(|[aff, dic]| aff.is_file() && dic.is_file())
}

/// Where a language's dictionary is, if anywhere: the system's first, then a download.
pub fn dictionary_paths(code: &str) -> Option<[PathBuf; 2]> {
    system_paths(code)
        .or_else(|| downloaded_paths(code).filter(|[aff, dic]| aff.is_file() && dic.is_file()))
}

/// Where a language's dictionary comes from, for the Preferences.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    /// Installed with the system.
    System,
    /// Downloaded earlier.
    Downloaded,
    /// Not here: enabling it downloads it.
    Missing,
}

/// A dictionary download in progress or failed (Preferences shows it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownloadState {
    Downloading,
    Failed(String),
}

/// Download `lang`'s dictionary into [`download_dir`] (written through temporary files,
/// so a failed download leaves nothing half-written).
pub async fn download(lang: &Language) -> Result<(), String> {
    let paths = downloaded_paths(lang.code).ok_or("no folder for the dictionaries")?;
    let client = reqwest::Client::new();
    let mut files = Vec::new();
    for url in download_urls(lang) {
        let bytes = client
            .get(&url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| e.to_string())?
            .bytes()
            .await
            .map_err(|e| e.to_string())?;
        files.push(bytes);
    }
    if let Some(dir) = paths[0].parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    for (path, bytes) in paths.iter().zip(&files) {
        let tmp = path.with_extension("part");
        std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn availability(code: &str) -> Availability {
    if system_paths(code).is_some() {
        Availability::System
    } else if dictionary_paths(code).is_some() {
        Availability::Downloaded
    } else {
        Availability::Missing
    }
}

/// The personal dictionary: the words added with "Add to Dictionary", one per line.
pub fn personal_dictionary_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("markdone").join("dictionary.txt"))
}

/// The dictionaries in use.
pub struct Speller {
    dictionaries: Vec<spellbook::Dictionary>,
}

static SPELLER: RwLock<Option<Arc<Speller>>> = RwLock::new(None);
static PERSONAL: RwLock<Option<HashSet<String>>> = RwLock::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Changes whenever the result of [`misspelled`] may change for the same text.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// Load the dictionaries of `codes` (those that are available; the codes of the others
/// are returned) into a speller. Slow-ish (tens of milliseconds per language): call it
/// off the UI thread.
pub fn load(codes: &[String]) -> (Speller, Vec<String>) {
    let mut dictionaries = Vec::new();
    let mut missing = Vec::new();
    for code in codes {
        let loaded = dictionary_paths(code).and_then(|[aff, dic]| {
            let aff = read_dictionary_file(&aff)?;
            let dic = read_dictionary_file(&dic)?;
            spellbook::Dictionary::new(&aff, &dic)
                .map_err(|e| eprintln!("[markdone] dictionary {code}: {e}"))
                .ok()
        });
        match loaded {
            Some(d) => dictionaries.push(d),
            None => missing.push(code.clone()),
        }
    }
    (Speller { dictionaries }, missing)
}

/// A dictionary file as text. Most are UTF-8; older ones (German, some system
/// dictionaries) are ISO-8859-1, which maps each byte to the code point of its value.
fn read_dictionary_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(decode_dictionary(bytes))
}

fn decode_dictionary(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => e.into_bytes().into_iter().map(char::from).collect(),
    }
}

/// Make `speller` the active one (`None`: spell checking off).
pub fn install(speller: Option<Speller>) {
    if PERSONAL.read().map(|p| p.is_none()).unwrap_or(true) {
        load_personal();
    }
    if let Ok(mut s) = SPELLER.write() {
        *s = speller.filter(|s| !s.dictionaries.is_empty()).map(Arc::new);
    }
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Whether spell checking is on (with at least one dictionary).
pub fn is_active() -> bool {
    SPELLER.read().map(|s| s.is_some()).unwrap_or(false)
}

fn active() -> Option<Arc<Speller>> {
    SPELLER.read().ok().and_then(|s| s.clone())
}

fn load_personal() {
    let words = personal_dictionary_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|w| !w.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if let Ok(mut p) = PERSONAL.write() {
        *p = Some(words);
    }
}

/// Add `word` to the personal dictionary (and its file).
pub fn add_to_personal(word: &str) -> std::io::Result<()> {
    let word = normalize(word);
    if let Some(path) = personal_dictionary_path() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(file, "{word}")?;
    }
    if let Ok(mut p) = PERSONAL.write() {
        p.get_or_insert_with(HashSet::new).insert(word);
    }
    GENERATION.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Typographic apostrophes as the dictionaries spell them.
fn normalize(word: &str) -> String {
    word.replace('’', "'")
}

/// Whether `word` is right: some dictionary accepts it, or it is a personal word.
fn is_correct(speller: &Speller, word: &str) -> bool {
    let word = normalize(word);
    if let Ok(p) = PERSONAL.read()
        && let Some(p) = p.as_ref()
        && (p.contains(&word) || p.contains(&word.to_lowercase()))
    {
        return true;
    }
    speller.dictionaries.iter().any(|d| d.check(&word))
}

/// Suggestions for a misspelled `word`, best first, from every dictionary in use.
pub fn suggest(word: &str) -> Vec<String> {
    let Some(speller) = active() else {
        return Vec::new();
    };
    let word = normalize(word);
    let mut out: Vec<String> = Vec::new();
    for d in &speller.dictionaries {
        let mut found = Vec::new();
        d.suggest(&word, &mut found);
        for s in found {
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out.truncate(6);
    out
}

/// The words of `line` to check, as byte ranges in it: prose only. Words overlapping a
/// `skip` range (code, links' targets, math…) are left out, and so are tokens that are
/// not prose (URLs, paths, e-mail addresses, identifiers: anything with `/`, `@`, `_`,
/// digits and the like), single letters and words in capitals (acronyms).
pub fn checkable_words(line: &str, skip: &[Range<usize>]) -> Vec<Range<usize>> {
    let mut words = Vec::new();
    let mut token_start = None;
    let bytes = line.as_bytes();
    for i in 0..=bytes.len() {
        let space = i == bytes.len() || bytes[i].is_ascii_whitespace();
        match (token_start, space) {
            (None, false) => token_start = Some(i),
            (Some(start), true) => {
                token_start = None;
                let token = &line[start..i];
                if token.contains([
                    '/', '\\', '@', '_', '<', '>', '`', '=', '{', '}', '|', '$', '#', '~', '^',
                ]) || token.contains("www.")
                {
                    continue;
                }
                for (offset, word) in token.unicode_word_indices() {
                    let range = start + offset..start + offset + word.len();
                    if word.chars().count() < 2
                        || word.chars().any(|c| c.is_numeric())
                        || (word.chars().all(|c| !c.is_lowercase()) && word.chars().count() > 1)
                        || skip
                            .iter()
                            .any(|s| s.start < range.end && range.start < s.end)
                    {
                        continue;
                    }
                    words.push(range);
                }
            }
            _ => {}
        }
    }
    words
}

/// The misspelled words of `line` (byte ranges in it), or nothing when spell checking
/// is off. `skip` and the rules are those of [`checkable_words`]; `caret`, a byte offset
/// in the line, spares the word being typed there (the caret right after it).
pub fn misspelled(line: &str, skip: &[Range<usize>], caret: Option<usize>) -> Vec<Range<usize>> {
    let Some(speller) = active() else {
        return Vec::new();
    };
    checkable_words(line, skip)
        .into_iter()
        .filter(|r| caret != Some(r.end))
        .filter(|r| !is_correct(&speller, &line[r.clone()]))
        .collect()
}

/// The word at byte `off` of `line` that spell checking marks, if any (for the context
/// menu): its range in the line.
pub fn misspelled_at(line: &str, skip: &[Range<usize>], off: usize) -> Option<Range<usize>> {
    misspelled(line, skip, None)
        .into_iter()
        .find(|r| r.start <= off && off <= r.end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words<'a>(line: &'a str, skip: &[Range<usize>]) -> Vec<&'a str> {
        checkable_words(line, skip)
            .into_iter()
            .map(|r| &line[r])
            .collect()
    }

    #[test]
    fn checks_prose_only() {
        assert_eq!(
            words("Un albero, l'altro e dell’acqua.", &[]),
            vec!["Un", "albero", "l'altro", "dell’acqua"]
        );
        // URLs, paths, mails, identifiers, numbers, acronyms and single letters are skipped.
        assert_eq!(
            words(
                "see https://x.org/a or ~/dir/f a@b.it snake_case v2 NASA I ok",
                &[]
            ),
            vec!["see", "or", "ok"]
        );
        // Words overlapping a skipped range (code, a link's target…) are left out.
        let line = "use `foo bar` here";
        let code = [Range { start: 4, end: 13 }];
        assert_eq!(words(line, &code), vec!["use", "here"]);
    }

    #[test]
    fn latin1_dictionaries_are_decoded() {
        assert_eq!(decode_dictionary("Straße".as_bytes().to_vec()), "Straße");
        assert_eq!(
            decode_dictionary(vec![b'S', b't', b'r', b'a', 0xDF, b'e']),
            "Straße"
        );
    }

    #[test]
    fn catalog_urls_point_at_aff_and_dic() {
        let [aff, dic] = download_urls(language("it_IT").unwrap());
        assert!(aff.ends_with("/it_IT/it_IT.aff"));
        assert!(dic.ends_with("/it_IT/it_IT.dic"));
        let [aff, _] = download_urls(language("fr_FR").unwrap());
        assert!(aff.ends_with("/fr_FR/dictionaries/fr.aff"));
    }
}

//! Keeps the index honest: every repository in `extensions.json` must have a
//! latest release den can install, checked with den's own manifest rules.
//!
//! ```text
//! den-index check [--base <old extensions.json>]   # for pull requests
//! den-index build                                   # writes index.json
//! ```
//!
//! `check` fails on a problem with an entry the pull request adds; entries
//! already on main (`--base`) only warn, so a broken release elsewhere never
//! blocks an unrelated pull request. `build` leaves broken entries out of
//! `index.json` with a warning.

use std::collections::HashSet;
use std::process::ExitCode;

use den_extension::{API_VERSION, MANIFEST, Manifest};
use serde::{Deserialize, Serialize};

const LIST: &str = "extensions.json";
const INDEX: &str = "index.json";

/// `extensions.json`, as authors edit it.
#[derive(Debug, Deserialize)]
struct List {
    extensions: Vec<Item>,
}

#[derive(Debug, Deserialize)]
struct Item {
    repo: String,
}

/// An entry of `index.json`, as den reads it (`Listing` in den).
#[derive(Debug, PartialEq, Serialize)]
struct Listing {
    repo: String,
    id: String,
    name: String,
    version: String,
    api: u32,
    description: String,
    icon_url: String,
    readme_url: String,
}

#[derive(Serialize)]
struct Index {
    /// Unix seconds.
    generated: u64,
    extensions: Vec<Listing>,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("check") => check(args.iter().position(|a| a == "--base").and_then(|i| args.get(i + 1))),
        Some("build") => build(),
        _ => Err("usage: den-index check [--base <old extensions.json>] | den-index build".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn read_list(path: &str) -> Result<List, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))
}

fn check(base: Option<&String>) -> Result<(), String> {
    let list = read_list(LIST)?;
    let old: HashSet<String> = match base {
        // A missing base (the first pull request) means everything is new.
        Some(path) => read_list(path).map(|l| l.extensions.into_iter().map(|i| i.repo.to_lowercase()).collect()).unwrap_or_default(),
        None => HashSet::new(),
    };
    let (listings, problems) = resolve(&list, &Fetch::Web);
    let mut failed = false;
    for (repo, problem) in &problems {
        let new = !old.contains(&repo.to_lowercase());
        eprintln!("{}: {repo}: {problem}", if new { "error" } else { "warning" });
        failed |= new;
    }
    for listing in &listings {
        println!("ok: {} {} ({})", listing.id, listing.version, listing.repo);
    }
    if failed { Err("fix the entries above".into()) } else { Ok(()) }
}

fn build() -> Result<(), String> {
    let list = read_list(LIST)?;
    let (mut listings, problems) = resolve(&list, &Fetch::Web);
    for (repo, problem) in &problems {
        eprintln!("warning: left out {repo}: {problem}");
    }
    listings.sort_by_key(|l| l.name.to_lowercase());
    // Unchanged listings keep the file (and its date) as it is: no daily commit.
    let old = std::fs::read_to_string(INDEX).ok().and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    if old.is_some_and(|old| old["extensions"] == serde_json::to_value(&listings).unwrap_or_default()) {
        println!("{INDEX} is up to date");
        return Ok(());
    }
    let generated = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let text = serde_json::to_string_pretty(&Index { generated, extensions: listings }).map_err(|e| e.to_string())?;
    std::fs::write(INDEX, text + "\n").map_err(|e| format!("cannot write {INDEX}: {e}"))?;
    println!("wrote {INDEX}");
    Ok(())
}

/// How the checks reach GitHub; a fixture in the tests.
enum Fetch {
    Web,
    #[cfg(test)]
    Fixture(std::collections::HashMap<String, Option<String>>),
}

impl Fetch {
    /// The body at `url`; `None` when it isn't there.
    fn text(&self, url: &str) -> Result<Option<String>, String> {
        match self {
            Fetch::Web => match agent().get(url).call() {
                Ok(response) => response.into_string().map(Some).map_err(|e| e.to_string()),
                Err(ureq::Error::Status(404, _)) => Ok(None),
                Err(e) => Err(format!("cannot fetch {url}: {e}")),
            },
            #[cfg(test)]
            Fetch::Fixture(files) => Ok(files.get(url).cloned().flatten()),
        }
    }

    /// Whether something is at `url` (without downloading a release's zip).
    fn exists(&self, url: &str) -> Result<bool, String> {
        match self {
            Fetch::Web => match agent().head(url).call() {
                Ok(_) => Ok(true),
                Err(ureq::Error::Status(404, _)) => Ok(false),
                Err(e) => Err(format!("cannot reach {url}: {e}")),
            },
            #[cfg(test)]
            Fetch::Fixture(files) => Ok(files.get(url).is_some_and(Option::is_some)),
        }
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(30)).redirects(8).build()
}

/// The listings, and what is wrong with the entries that have none.
fn resolve(list: &List, fetch: &Fetch) -> (Vec<Listing>, Vec<(String, String)>) {
    let mut listings: Vec<Listing> = Vec::new();
    let mut problems = Vec::new();
    let mut repos = HashSet::new();
    for item in &list.extensions {
        if !repos.insert(item.repo.to_lowercase()) {
            problems.push((item.repo.clone(), "listed twice".into()));
            continue;
        }
        match listing(&item.repo, fetch) {
            Ok(listing) if listings.iter().any(|l| l.id == listing.id) => {
                problems.push((item.repo.clone(), format!("the id \"{}\" is already listed by another repository", listing.id)));
            }
            Ok(listing) => listings.push(listing),
            Err(problem) => problems.push((item.repo.clone(), problem)),
        }
    }
    (listings, problems)
}

/// `repo`'s latest release, checked as den would install it.
fn listing(repo: &str, fetch: &Fetch) -> Result<Listing, String> {
    if !valid_repo(repo) {
        return Err("write it as owner/repo".into());
    }
    let release = |file: &str| format!("https://github.com/{repo}/releases/latest/download/{file}");
    let text = fetch.text(&release(MANIFEST))?.ok_or("its latest release has no extension.json (publish with release.yml from den's examples)")?;
    let manifest = Manifest::parse(&text)?;
    check_manifest(repo, &manifest)?;
    if !fetch.exists(&release(&manifest.asset()))? {
        return Err(format!("its latest release has no {}", manifest.asset()));
    }
    let raw = |file: &str| format!("https://raw.githubusercontent.com/{repo}/HEAD/{file}");
    let found = |url: String| -> Result<String, String> { Ok(if fetch.exists(&url)? { url } else { String::new() }) };
    let icon_url = if manifest.icon.is_empty() { String::new() } else { found(raw(&manifest.icon))? };
    if !manifest.icon.is_empty() && icon_url.is_empty() {
        return Err(format!("its icon {} isn't in the repository", manifest.icon));
    }
    Ok(Listing {
        repo: repo.to_string(),
        id: manifest.id,
        name: manifest.name,
        version: manifest.version,
        api: manifest.api,
        description: manifest.description,
        icon_url,
        readme_url: found(raw("README.md"))?,
    })
}

/// The rules beyond `Manifest::parse` for a listed extension.
fn check_manifest(repo: &str, manifest: &Manifest) -> Result<(), String> {
    if manifest.api > API_VERSION {
        return Err(format!("built for extension API {}, newer than den's {API_VERSION}", manifest.api));
    }
    if manifest.sha256.is_empty() {
        return Err("its released extension.json has no sha256 (release.yml adds it)".into());
    }
    if !manifest.repository.is_empty() && !manifest.repository.eq_ignore_ascii_case(repo) {
        return Err(format!("its extension.json names the repository {}", manifest.repository));
    }
    if manifest.description.trim().is_empty() {
        return Err("its extension.json has no description".into());
    }
    Ok(())
}

fn valid_repo(repo: &str) -> bool {
    let ok = |s: &str| !s.is_empty() && !s.starts_with('.') && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    matches!(repo.split('/').collect::<Vec<_>>()[..], [owner, name] if ok(owner) && ok(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const SHA: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn manifest(id: &str, extra: &str) -> String {
        format!(r#"{{"id":"{id}","name":"{id}","version":"1.0.0","api":1,"description":"d","sha256":"{SHA}"{extra}}}"#)
    }

    /// A release of `repo` with `manifest`, its zip and README.
    fn release(files: &mut HashMap<String, Option<String>>, repo: &str, id: &str, manifest: String) {
        let base = format!("https://github.com/{repo}/releases/latest/download");
        files.insert(format!("{base}/extension.json"), Some(manifest));
        files.insert(format!("{base}/{id}-windows-x86_64.zip"), Some(String::new()));
        files.insert(format!("https://raw.githubusercontent.com/{repo}/HEAD/README.md"), Some(String::new()));
    }

    fn list(repos: &[&str]) -> List {
        List { extensions: repos.iter().map(|r| Item { repo: r.to_string() }).collect() }
    }

    #[test]
    fn lists_good_releases_and_says_whats_wrong_with_the_others() {
        let mut files = HashMap::new();
        release(&mut files, "me/good", "good", manifest("good", r#","icon":"icon.svg""#));
        files.insert("https://raw.githubusercontent.com/me/good/HEAD/icon.svg".into(), Some(String::new()));
        release(&mut files, "you/same-id", "good", manifest("good", ""));
        release(&mut files, "me/no-sha", "no-sha", manifest("no-sha", "").replace(SHA, ""));
        release(&mut files, "me/future", "future", manifest("future", "").replace(r#""api":1"#, r#""api":99"#));
        release(&mut files, "me/other-repo", "other-repo", manifest("other-repo", r#","repository":"someone/else""#));
        release(&mut files, "me/no-zip", "no-zip", manifest("no-zip", ""));
        files.remove("https://github.com/me/no-zip/releases/latest/download/no-zip-windows-x86_64.zip");
        release(&mut files, "me/no-icon", "no-icon", manifest("no-icon", r#","icon":"gone.png""#));
        let repos = ["me/good", "you/same-id", "me/no-sha", "me/future", "me/other-repo", "me/no-zip", "me/no-icon", "me/unreleased", "nope", "ME/GOOD"];
        let (listings, problems) = resolve(&list(&repos), &Fetch::Fixture(files));

        assert_eq!(listings.len(), 1);
        let good = &listings[0];
        assert_eq!((good.id.as_str(), good.icon_url.as_str()), ("good", "https://raw.githubusercontent.com/me/good/HEAD/icon.svg"));
        assert_eq!(good.readme_url, "https://raw.githubusercontent.com/me/good/HEAD/README.md");

        let problem = |repo: &str| problems.iter().find(|(r, _)| r == repo).map(|(_, p)| p.as_str()).unwrap_or("no problem");
        assert!(problem("you/same-id").contains("already listed"));
        assert!(problem("me/no-sha").contains("sha256"));
        assert!(problem("me/future").contains("newer than den"));
        assert!(problem("me/other-repo").contains("someone/else"));
        assert!(problem("me/no-zip").contains("no-zip-windows-x86_64.zip"));
        assert!(problem("me/no-icon").contains("gone.png"));
        assert!(problem("me/unreleased").contains("no extension.json"));
        assert!(problem("nope").contains("owner/repo"));
        assert!(problem("ME/GOOD").contains("twice"));
    }

    #[test]
    fn validates_repository_names() {
        assert!(valid_repo("patrickiel/task-buttons"));
        for bad in ["", "a", "a/b/c", "a/.b", "a b/c", "../x"] {
            assert!(!valid_repo(bad), "{bad}");
        }
    }
}

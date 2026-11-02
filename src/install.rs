use std::path::{Path, PathBuf};

use crate::Error;
use crate::language::{Language, Manifest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

fn manifest(dir: &Path) -> Result<Manifest, Error> {
    let path = dir.join("edo-package.toml");
    let text = std::fs::read_to_string(&path).map_err(|e| Error::new("io", format!("{}: {e}", path.display())))?;
    toml::from_str(&text).map_err(|e| Error::new("manifest", format!("{}: {e}", path.display())))
}

/// Packages of a registry checkout as (name, directory), sorted by name.
pub fn packages_in(root: &Path) -> Result<Vec<(String, PathBuf)>, Error> {
    let m = manifest(root)?;
    let registry = match (m.registry, m.package) {
        (Some(_), Some(_)) => return Err(Error::new("manifest", "[registry] and [package] cannot be in one file")),
        (None, Some(p)) => return Ok(vec![(p.name, root.to_path_buf())]),
        (None, None) => return Ok(vec![]),
        (Some(r), None) => r,
    };
    let mut globs = globset::GlobSetBuilder::new();
    for p in &registry.packages {
        let glob = globset::GlobBuilder::new(p).literal_separator(true).build();
        globs.add(glob.map_err(|e| Error::new("manifest", e.to_string()))?);
    }
    let globs = globs.build().map_err(|e| Error::new("manifest", e.to_string()))?;
    let mut out = vec![];
    for entry in ignore::WalkBuilder::new(root).hidden(false).build().filter_map(Result::ok) {
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        let rel = rel.to_string_lossy().replace('\\', "/");
        if entry.path().join("edo-package.toml").is_file()
            && globs.is_match(&rel)
            && let Some(p) = manifest(entry.path())?.package
        {
            out.push((p.name, entry.path().to_path_buf()));
        }
    }
    out.sort();
    Ok(out)
}

/// A registry checked out to a local directory.
pub struct Fetched {
    pub dir: PathBuf,
    /// The commit that was checked out.
    pub commit: String,
    /// The download's temporary directory; dropping it deletes the checkout.
    pub tmp: Option<tempfile::TempDir>,
}

/// `<config>/edo`, which holds `edo-cache.toml` and the packages under `packages/<registry>/<name>/`.
pub fn home_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("edo")
}

/// `<home>/packages/<registry>/<name>`. Both come from outside (a registry's manifest, the cache, the user), so
/// each must be plain directory names: `..` or an absolute path would point the install's delete elsewhere.
fn package_dir(home: &Path, registry: &str, name: &str) -> Result<PathBuf, Error> {
    let plain = |p: &str| Path::new(p).components().all(|c| matches!(c, std::path::Component::Normal(_)));
    if !(plain(registry) && plain(name) && Path::new(name).components().count() == 1) {
        return Err(Error::new("invalid_name", format!("{registry}:{name} is not a plain registry and package name")));
    }
    Ok(home.join("packages").join(registry).join(name))
}

/// `<home>/edo-cache.toml`: the known registries and the installed packages.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Cache {
    #[serde(default)]
    pub registries: Vec<KnownRegistry>,
    #[serde(default)]
    pub packages: Vec<InstalledPackage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnownRegistry {
    #[serde(rename = "type")]
    pub kind: RegistryKind,
    /// `owner/repo`
    pub repo: String,
    /// The commit `packages` was read from; absent until the registry is first fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default)]
    pub packages: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RegistryKind {
    Github,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstalledPackage {
    /// The registry id, such as `github.com/owner/repo`.
    pub registry: String,
    pub name: String,
    pub commit: String,
}

impl InstalledPackage {
    /// `<registry>@<commit>`, as reports show it.
    fn source(&self) -> String {
        format!("{}@{}", self.registry, self.commit)
    }
}

impl KnownRegistry {
    /// The id used in specs, reports and install paths.
    pub fn id(&self) -> String {
        match self.kind {
            RegistryKind::Github => format!("github.com/{}", self.repo),
        }
    }

    fn from_id(id: &str) -> Result<Self, Error> {
        let repo = id.strip_prefix("github.com/").ok_or_else(|| {
            Error::new("unsupported_registry", format!("{id}: only github.com registries are supported"))
        })?;
        Ok(KnownRegistry { kind: RegistryKind::Github, repo: repo.into(), commit: None, packages: vec![] })
    }
}

const CACHE_FILE: &str = "edo-cache.toml";

impl Cache {
    /// Without a cache file, only the default registry is known.
    pub fn load(home: &Path) -> Result<Cache, Error> {
        let path = home.join(CACHE_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| Error::new("cache", format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(Cache { registries: vec![KnownRegistry::from_id(DEFAULT_REGISTRY)?], packages: vec![] })
            }
            Err(e) => Err(Error::new("io", format!("{}: {e}", path.display()))),
        }
    }

    // shortcut: read-modify-write without a lock, so concurrent edo lang runs can lose an entry; lock if that happens.
    fn save(&self, home: &Path) -> Result<(), Error> {
        let io = |e: std::io::Error| Error::new("io", format!("{}: {e}", home.display()));
        std::fs::create_dir_all(home).map_err(io)?;
        let text = toml::to_string(self).map_err(|e| Error::new("cache", e.to_string()))?;
        std::fs::write(home.join(CACHE_FILE), text).map_err(io)
    }
}

/// Fetches a registry and records it as known, with the packages it holds.
pub fn update_registry(
    home: &Path,
    id: &str,
    fetch: &dyn Fn(&str) -> Result<Fetched, Error>,
) -> Result<Fetched, Error> {
    let mut known = KnownRegistry::from_id(id)?;
    let fetched = fetch(id)?;
    known.commit = Some(fetched.commit.clone());
    known.packages = packages_in(&fetched.dir)?.into_iter().map(|(name, _)| name).collect();
    let mut cache = Cache::load(home)?;
    match cache.registries.iter_mut().find(|r| r.id() == id) {
        Some(r) => *r = known,
        None => cache.registries.push(known),
    }
    cache.save(home)?;
    Ok(fetched)
}

fn error_report(spec: &str, e: Error) -> Value {
    json!({"ok": false, "spec": spec, "error": e.code, "message": e.message})
}

/// Makes a registry given as `<repo>` known.
pub fn add_registry(home: &Path, spec: &str, fetch: &dyn Fn(&str) -> Result<Fetched, Error>) -> Value {
    let id = parse_spec(spec).registry.unwrap_or_default();
    match update_registry(home, &id, fetch).and_then(|_| Cache::load(home)) {
        Ok(cache) => {
            let packages = cache.registries.iter().find(|r| r.id() == id).map(|r| r.packages.clone());
            json!({"ok": true, "registry": id, "packages": packages})
        }
        Err(e) => error_report(spec, e),
    }
}

/// Forgets a registry given as `<repo>`; packages installed from it stay.
pub fn remove_registry(home: &Path, spec: &str) -> Value {
    let id = parse_spec(spec).registry.unwrap_or_default();
    let removed = Cache::load(home).and_then(|mut cache| {
        let before = cache.registries.len();
        cache.registries.retain(|r| r.id() != id);
        if cache.registries.len() == before {
            return Err(Error::new("not_found", format!("{id} is not a known registry")));
        }
        cache.save(home)
    });
    match removed {
        Ok(()) => json!({"ok": true, "registry": id}),
        Err(e) => error_report(spec, e),
    }
}

/// Downloads a GitHub registry's default branch as a tarball and unpacks it into a temporary directory.
/// shortcut: only github.com registries can be fetched; add other hosts' archive URLs when someone needs them.
pub fn fetch_github(registry: &str) -> Result<Fetched, Error> {
    let dir = tempfile::tempdir().map_err(|e| Error::new("io", e.to_string()))?;
    let tmp = dir.path();
    let repo = registry.strip_prefix("github.com/").ok_or_else(|| {
        Error::new("unsupported_registry", format!("{registry}: only github.com registries are supported"))
    })?;
    let net = |e: ureq::Error| Error::new("network", format!("{registry}: {e}"));
    let response = ureq::get(&format!("https://api.github.com/repos/{repo}/tarball"))
        .header("User-Agent", "edo")
        .call()
        .map_err(net)?;
    let gz = flate2::read::GzDecoder::new(response.into_body().into_reader());
    tar::Archive::new(gz).unpack(tmp).map_err(|e| Error::new("io", format!("{registry}: {e}")))?;
    // The tarball holds one directory named `<owner>-<repo>-<commit>`.
    let top = std::fs::read_dir(tmp)
        .map_err(|e| Error::new("io", e.to_string()))?
        .filter_map(Result::ok)
        .find(|e| e.path().is_dir())
        .ok_or_else(|| Error::new("io", format!("{registry}: empty archive")))?;
    let commit = top.file_name().to_string_lossy().rsplit('-').next().unwrap_or_default().to_string();
    Ok(Fetched { dir: top.path(), commit, tmp: Some(dir) })
}

/// Installs one package into `home` (`<config>/edo`) and reports the outcome.
pub fn install(home: &Path, spec: &str, fetch: &dyn Fn(&str) -> Result<Fetched, Error>) -> Value {
    try_install(home, spec, fetch).unwrap_or_else(|e| error_report(spec, e))
}

fn try_install(home: &Path, spec: &str, fetch: &dyn Fn(&str) -> Result<Fetched, Error>) -> Result<Value, Error> {
    let spec = parse_spec(spec);
    let registry = spec.registry.as_deref().unwrap_or(DEFAULT_REGISTRY);
    let fetched = update_registry(home, registry, fetch)?;
    let packages = packages_in(&fetched.dir)?;
    let (name, dir) = match &spec.name {
        Some(n) => packages.into_iter().find(|(p, _)| p == n),
        // Without a name, only a registry whose root is itself a package is unambiguous.
        None => match packages.as_slice() {
            [(_, dir)] if *dir == fetched.dir => packages.into_iter().next(),
            _ => {
                let names: Vec<&str> = packages.iter().map(|(p, _)| p.as_str()).collect();
                let message = format!("{registry} has several packages; name one of: {}", names.join(", "));
                return Err(Error::new("name_required", message));
            }
        },
    }
    .ok_or_else(|| Error::new("not_found", format!("no such package in {registry}")))?;
    let target = package_dir(home, registry, &name)?;
    let extensions = manifest(&dir)?.package.map(|p| p.extensions).unwrap_or_default();
    for other in installed(home)?.iter().filter(|p| p.dir != target) {
        if let Some(ext) = other.extensions.iter().find(|e| extensions.contains(e)) {
            let message = format!(".{ext} is already handled by {}:{}", other.registry, other.name);
            return Err(Error::new("extension_conflict", message));
        }
    }
    let io = |e: std::io::Error| Error::new("io", format!("{}: {e}", target.display()));
    if target.exists() {
        std::fs::remove_dir_all(&target).map_err(io)?;
    }
    copy_dir(&dir, &target).map_err(io)?;
    let mut cache = Cache::load(home)?;
    let previous = cache.packages.iter().position(|p| p.registry == registry && p.name == name);
    let previous = previous.map(|i| cache.packages.remove(i));
    let entry = InstalledPackage { registry: registry.into(), name, commit: fetched.commit.clone() };
    let mut report = json!({"ok": true, "name": entry.name, "registry": registry, "source": entry.source()});
    if let Some(previous) = previous {
        report["replaced_source"] = json!(previous.source());
    }
    cache.packages.push(entry);
    cache.save(home)?;
    Ok(report)
}

pub fn install_all(home: &Path, specs: &[String], fetch: &dyn Fn(&str) -> Result<Fetched, Error>) -> Vec<Value> {
    specs.iter().map(|s| install(home, s, fetch)).collect()
}

/// Removes an installed package given as `<name>` or `<repo>:<name>`.
pub fn remove(home: &Path, spec: &str) -> Value {
    let parsed = parse_spec(spec);
    let name = parsed.name.as_deref().unwrap_or("");
    let found: Vec<Installed> = match installed(home) {
        Ok(all) => all
            .into_iter()
            .filter(|p| p.name == name && parsed.registry.as_ref().is_none_or(|r| *r == p.registry))
            .collect(),
        Err(e) => return error_report(spec, e),
    };
    match found.as_slice() {
        [] => json!({"ok": false, "spec": spec, "error": "not_found"}),
        [p] => match forget(home, p) {
            Ok(()) => json!({"ok": true, "name": p.name, "registry": p.registry}),
            Err(e) => error_report(spec, e),
        },
        many => {
            let candidates: Vec<String> = many.iter().map(|p| format!("{}:{}", p.registry, p.name)).collect();
            json!({"ok": false, "spec": spec, "error": "ambiguous", "matches": candidates})
        }
    }
}

/// Drops a package from the cache and deletes its directory.
fn forget(home: &Path, p: &Installed) -> Result<(), Error> {
    let mut cache = Cache::load(home)?;
    cache.packages.retain(|c| !(c.registry == p.registry && c.name == p.name));
    cache.save(home)?;
    match std::fs::remove_dir_all(&p.dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(Error::new("io", e.to_string())),
        _ => Ok(()),
    }
}

/// Builtin languages plus the installed ones, which win on shared extensions.
/// A package that fails to load is skipped so one broken package does not disable edo.
pub fn languages(home: &Path) -> crate::language::Languages {
    let loaded = installed(home).unwrap_or_default().iter().filter_map(|p| Language::load_dir(&p.dir).ok()).collect();
    crate::language::Languages::with_installed(loaded)
}

/// Builtin and installed packages, with their language server's status when they have one.
pub fn list(home: &Path) -> Result<Value, Error> {
    let with_lsp = |mut entry: Value, lsp: Option<&crate::lsp::Lsp>| {
        if let Some(lsp) = lsp {
            entry["lsp"] = lsp.status();
        }
        entry
    };
    let builtin: Vec<Value> = crate::language::Languages::builtin()
        .iter()
        .map(|l| with_lsp(json!({"name": l.name, "extensions": l.extensions}), l.lsp.as_ref()))
        .collect();
    let installed: Vec<Value> = installed(home)?
        .into_iter()
        .map(|p| {
            let entry = json!({"registry": p.registry, "name": p.name, "extensions": p.extensions, "source": p.source});
            with_lsp(entry, p.lsp.as_ref())
        })
        .collect();
    Ok(json!({"builtin": builtin, "installed": installed}))
}

/// A package installed in the store.
#[derive(Debug)]
pub struct Installed {
    pub registry: String,
    pub name: String,
    pub extensions: Vec<String>,
    pub lsp: Option<crate::lsp::Lsp>,
    pub dir: PathBuf,
    /// `<registry>@<commit>`
    pub source: String,
}

/// The packages recorded in `edo-cache.toml`, sorted by registry and name.
pub fn installed(home: &Path) -> Result<Vec<Installed>, Error> {
    let mut out = vec![];
    for p in Cache::load(home)?.packages {
        let dir = package_dir(home, &p.registry, &p.name)?;
        // The cache is the record of what is installed; a package whose manifest is gone or broken still is one,
        // so it stays removable, and it claims no extensions.
        let package = manifest(&dir).ok().and_then(|m| m.package);
        let (extensions, lsp) = package.map_or((vec![], None), |m| (m.extensions, m.lsp));
        out.push(Installed { source: p.source(), registry: p.registry, name: p.name, extensions, lsp, dir });
    }
    out.sort_by(|a, b| (&a.registry, &a.name).cmp(&(&b.registry, &b.name)));
    Ok(out)
}

/// Copies the package's files; nested package directories (a root package's siblings) are not part of it.
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            std::fs::copy(entry.path(), to.join(entry.file_name()))?;
        }
    }
    Ok(())
}

/// The registry that `edo lang install <name>` looks in.
pub const DEFAULT_REGISTRY: &str = "github.com/takagiy/edo";

/// A package request: the registry id (such as `github.com/owner/repo`) if given, and the package name if given.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    /// `None` when only a name was given: install looks in the default registry, remove in any.
    pub registry: Option<String>,
    pub name: Option<String>,
}

pub fn parse_spec(spec: &str) -> Spec {
    if !spec.contains('/') {
        return Spec { registry: None, name: Some(spec.into()) };
    }
    // The name follows the last `:`, unless that `:` belongs to the URL scheme.
    let (repo, name) = match spec.rsplit_once(':') {
        Some((repo, name)) if !name.contains('/') => (repo, Some(name.to_string())),
        _ => (spec, None),
    };
    let repo = repo.split_once("://").map_or(repo, |(_, rest)| rest).trim_end_matches('/').trim_end_matches(".git");
    let host = repo.split('/').next().unwrap_or("");
    let registry = if host.contains('.') { repo.to_string() } else { format!("github.com/{repo}") };
    Spec { registry: Some(registry), name }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(registry: &str, name: Option<&str>) -> Spec {
        Spec { registry: Some(registry.into()), name: name.map(String::from) }
    }

    #[test]
    fn bare_name_looks_in_default_registry() {
        // Given: デフォルトのレジストリにあるパッケージ
        let repo = langs_repo();
        let home = tempfile::tempdir().unwrap();
        // When: 名前だけを指定してインストールする
        let report = install(home.path(), "myrust", &local(vec![(DEFAULT_REGISTRY, &repo)]));
        // Then: デフォルトのレジストリから探して入る
        assert_eq!(report["ok"], true, "{report}");
        assert!(home.path().join("packages").join(DEFAULT_REGISTRY).join("myrust/edo-package.toml").is_file());
    }

    #[test]
    fn repo_and_name_look_in_that_registry() {
        // Given / When: <repo>:<name> を解析する / Then: そのレジストリの、その名前のパッケージになる
        assert_eq!(parse_spec("github.com/foo/langs:zig"), spec("github.com/foo/langs", Some("zig")));
    }

    #[test]
    fn owner_slash_repo_is_github_repository() {
        // Given / When: owner/repo を解析する / Then: github.com のリポジトリになり、名前はない
        assert_eq!(parse_spec("foo/langs"), spec("github.com/foo/langs", None));
        assert_eq!(parse_spec("foo/langs:zig"), spec("github.com/foo/langs", Some("zig")));
    }

    #[test]
    fn url_registry_splits_name_at_last_colon() {
        // Given / When: URL のレジストリを解析する / Then: 最後の : で名前と分かれ、スキームは除かれる
        assert_eq!(parse_spec("https://gitlab.com/x/y:sql"), spec("gitlab.com/x/y", Some("sql")));
        assert_eq!(parse_spec("https://gitlab.com/x/y"), spec("gitlab.com/x/y", None));
    }

    #[test]
    fn registry_packages_glob_lists_packages() {
        // Given: [registry] packages = ["languages/*"] を持つ edo 本体のリポジトリ
        let root = Path::new(".");
        // When: パッケージを列挙する
        let names: Vec<String> = packages_in(root).unwrap().into_iter().map(|(name, _)| name).collect();
        // Then: languages/ の下のパッケージが並ぶ
        assert_eq!(names, ["go", "java", "javascript", "kotlin", "markdown", "python", "rust", "tsx", "typescript"]);
    }

    fn registry(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    fn package(name: &str, ext: &str) -> String {
        format!("[package]\nname = \"{name}\"\nextensions = [\"{ext}\"]\n[package.syntax]\nmodule = \"builtin:rust\"\n")
    }

    #[test]
    fn root_package_registry_is_its_only_package() {
        // Given: ルートが [package] のリポジトリ
        let repo = registry(&[("edo-package.toml", &package("myrust", "myrs")), ("edo.scm", "")]);
        // When: パッケージを列挙する
        let found = packages_in(repo.path()).unwrap();
        // Then: ルート自身がただ 1 つのパッケージになる
        assert_eq!(found, [("myrust".to_string(), repo.path().to_path_buf())]);
    }

    #[test]
    fn registry_and_package_in_same_manifest_is_error() {
        // Given: [registry] と [package] を両方持つマニフェスト
        let text = format!("{}\n[registry]\npackages = [\"*\"]\n", package("x", "x"));
        let repo = registry(&[("edo-package.toml", &text)]);
        // When: パッケージを列挙する
        let err = packages_in(repo.path()).unwrap_err();
        // Then: エラーになる
        assert_eq!(err.code, "manifest");
    }

    /// A fetcher that serves registries from local directories.
    fn local(repos: Vec<(&'static str, &tempfile::TempDir)>) -> impl Fn(&str) -> Result<Fetched, Error> {
        let repos: Vec<(String, PathBuf)> =
            repos.into_iter().map(|(id, d)| (id.to_string(), d.path().to_path_buf())).collect();
        move |id: &str| {
            let (_, dir) = repos.iter().find(|(r, _)| r == id).ok_or_else(|| Error::new("not_found", id))?;
            Ok(Fetched { dir: dir.clone(), commit: "abc".into(), tmp: None })
        }
    }

    fn langs_repo() -> tempfile::TempDir {
        registry(&[
            ("edo-package.toml", "[registry]\npackages = [\"langs/*\"]\n"),
            ("langs/myrust/edo-package.toml", &package("myrust", "myrs")),
            ("langs/myrust/edo.scm", ""),
            ("langs/other/edo-package.toml", &package("other", "oth")),
            ("langs/other/edo.scm", ""),
        ])
    }

    #[test]
    fn package_is_installed_under_registry_and_name() {
        // Given: パッケージを持つレジストリと空のインストール先
        let repo = langs_repo();
        let home = tempfile::tempdir().unwrap();
        // When: <repo>:<name> を指定してインストールする
        let report = install(home.path(), "foo/langs:myrust", &local(vec![("github.com/foo/langs", &repo)]));
        // Then: <インストール先>/<registry>/<name>/ に入る
        assert_eq!(report["ok"], true, "{report}");
        assert!(home.path().join("packages/github.com/foo/langs/myrust/edo-package.toml").is_file());
    }

    #[test]
    fn package_named_outside_packages_dir_is_refused_and_deletes_nothing() {
        // Given: ルートのパッケージ名がインストール先の外を指すレジストリと、インストール先にあるファイル
        let repo = registry(&[("edo-package.toml", &package("../../../..", "evil")), ("edo.scm", "")]);
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("keep"), "").unwrap();
        // When: 名前なしでインストールする
        let report = install(home.path(), "foo/evil", &local(vec![("github.com/foo/evil", &repo)]));
        // Then: エラーになり、インストール先のファイルは消えない
        assert_eq!(report["error"], "invalid_name", "{report}");
        assert!(home.path().join("keep").is_file());
    }

    #[test]
    fn installed_package_is_recorded_in_cache_with_its_commit() {
        // Given: パッケージを持つレジストリ
        let repo = langs_repo();
        let home = tempfile::tempdir().unwrap();
        // When: インストールする
        install(home.path(), "foo/langs:myrust", &local(vec![("github.com/foo/langs", &repo)]));
        // Then: edo-cache.toml に入手元のコミットと一緒に記録される
        let cache = Cache::load(home.path()).unwrap();
        let entry =
            InstalledPackage { registry: "github.com/foo/langs".into(), name: "myrust".into(), commit: "abc".into() };
        assert_eq!(cache.packages, [entry]);
    }

    #[test]
    fn without_cache_only_default_registry_is_known() {
        // Given: edo-cache.toml のないホーム
        let home = tempfile::tempdir().unwrap();
        // When: 読み込む
        let cache = Cache::load(home.path()).unwrap();
        // Then: デフォルトのレジストリだけが既知になる
        let ids: Vec<String> = cache.registries.iter().map(KnownRegistry::id).collect();
        assert_eq!(ids, [DEFAULT_REGISTRY]);
    }

    #[test]
    fn registry_used_for_install_becomes_known_with_its_packages() {
        // Given: 2 つのパッケージを持つレジストリ
        let repo = langs_repo();
        let home = tempfile::tempdir().unwrap();
        // When: そのうち 1 つをインストールする
        install(home.path(), "foo/langs:myrust", &local(vec![("github.com/foo/langs", &repo)]));
        // Then: レジストリが既知になり、パッケージの一覧とコミットが記録される
        let cache = Cache::load(home.path()).unwrap();
        let known = cache.registries.iter().find(|r| r.id() == "github.com/foo/langs").unwrap();
        assert_eq!(known.packages, ["myrust", "other"]);
        assert_eq!(known.commit.as_deref(), Some("abc"));
    }

    #[test]
    fn reinstalling_same_registry_package_overwrites_and_reports_previous_source() {
        // Given: 一度インストールしたパッケージ
        let repo = langs_repo();
        let home = tempfile::tempdir().unwrap();
        install(home.path(), "foo/langs:myrust", &local(vec![("github.com/foo/langs", &repo)]));
        // When: 新しいコミットから同じパッケージを入れる
        let newer = |_: &str| Ok(Fetched { dir: repo.path().to_path_buf(), commit: "def".into(), tmp: None });
        let report = install(home.path(), "foo/langs:myrust", &newer);
        // Then: 上書きされ、前の入手元が報告される
        assert_eq!(report["replaced_source"], "github.com/foo/langs@abc");
    }

    #[test]
    fn same_name_packages_from_different_registries_coexist() {
        // Given: 同じ名前で拡張子の違うパッケージを持つ 2 つのレジストリ
        let a = registry(&[("edo-package.toml", &package("sql", "pgsql")), ("edo.scm", "")]);
        let b = registry(&[("edo-package.toml", &package("sql", "mysql")), ("edo.scm", "")]);
        let home = tempfile::tempdir().unwrap();
        let fetch = local(vec![("github.com/a/sql", &a), ("github.com/b/sql", &b)]);
        // When: 両方をインストールする
        install(home.path(), "a/sql", &fetch);
        let report = install(home.path(), "b/sql", &fetch);
        // Then: どちらも残る
        assert_eq!(report["ok"], true, "{report}");
        assert!(home.path().join("packages/github.com/a/sql/sql/edo-package.toml").is_file());
        assert!(home.path().join("packages/github.com/b/sql/sql/edo-package.toml").is_file());
    }

    #[test]
    fn extension_conflict_with_installed_package_is_error() {
        // Given: 同じ拡張子を宣言するパッケージを持つ 2 つのレジストリ
        let a = registry(&[("edo-package.toml", &package("kt1", "kt")), ("edo.scm", "")]);
        let b = registry(&[("edo-package.toml", &package("kt2", "kt")), ("edo.scm", "")]);
        let home = tempfile::tempdir().unwrap();
        let fetch = local(vec![("github.com/a/kt", &a), ("github.com/b/kt", &b)]);
        install(home.path(), "a/kt", &fetch);
        // When: 2 つ目をインストールする
        let report = install(home.path(), "b/kt", &fetch);
        // Then: 拡張子の衝突でエラーになり、入らない
        assert_eq!(report["error"], "extension_conflict");
        assert!(!home.path().join("packages/github.com/b/kt").exists());
    }

    #[test]
    fn several_specs_give_one_result_each() {
        // Given: 2 つのパッケージを持つレジストリ
        let repo = langs_repo();
        let home = tempfile::tempdir().unwrap();
        // When: 2 つを同時に指定してインストールする
        let specs = ["foo/langs:myrust".to_string(), "foo/langs:other".to_string()];
        let reports = install_all(home.path(), &specs, &local(vec![("github.com/foo/langs", &repo)]));
        // Then: パッケージごとに結果が返る
        let names: Vec<_> = reports.iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["myrust", "other"]);
    }

    #[test]
    fn list_shows_builtin_and_installed_packages() {
        // Given: 1 つインストールしたインストール先
        let repo = langs_repo();
        let home = tempfile::tempdir().unwrap();
        install(home.path(), "foo/langs:myrust", &local(vec![("github.com/foo/langs", &repo)]));
        // When: 一覧を出す
        let listed = list(home.path()).unwrap();
        // Then: 組み込みとインストール済みのパッケージが並ぶ
        assert_eq!(listed["builtin"][0]["name"], "rust");
        assert_eq!(
            listed["installed"],
            json!([{"registry": "github.com/foo/langs", "name": "myrust", "extensions": ["myrs"], "source": "github.com/foo/langs@abc"}])
        );
    }

    #[test]
    fn list_shows_language_server_command_and_whether_it_is_found() {
        // Given: 何もインストールしていないインストール先
        let home = tempfile::tempdir().unwrap();
        // When: 一覧を出す
        let listed = list(home.path()).unwrap();
        // Then: LSP の設定がある言語には、そのコマンドと、見つかるかどうかが出る
        let rust = &listed["builtin"][0]["lsp"];
        assert_eq!(rust["command"], json!(["rust-analyzer"]));
        assert!(rust["found"].is_boolean(), "{rust}");
        let markdown = listed["builtin"].as_array().unwrap().iter().find(|l| l["name"] == "markdown").unwrap();
        assert!(markdown.get("lsp").is_none(), "{markdown}");
    }

    #[test]
    fn removing_package_deletes_its_directory() {
        // Given: インストールしたパッケージ
        let repo = langs_repo();
        let home = tempfile::tempdir().unwrap();
        install(home.path(), "foo/langs:myrust", &local(vec![("github.com/foo/langs", &repo)]));
        // When: 名前を指定して削除する
        let report = remove(home.path(), "myrust");
        // Then: パッケージのディレクトリが消える
        assert_eq!(report["ok"], true, "{report}");
        assert!(!home.path().join("packages/github.com/foo/langs/myrust").exists());
    }

    #[test]
    fn removed_package_is_dropped_from_cache() {
        // Given: インストールしたパッケージ
        let repo = langs_repo();
        let home = tempfile::tempdir().unwrap();
        install(home.path(), "foo/langs:myrust", &local(vec![("github.com/foo/langs", &repo)]));
        // When: 削除する
        remove(home.path(), "myrust");
        // Then: edo-cache.toml から消える
        assert!(Cache::load(home.path()).unwrap().packages.is_empty());
    }

    #[test]
    fn bare_name_matching_several_packages_is_ambiguous() {
        // Given: 違うレジストリから入れた同名のパッケージ
        let a = registry(&[("edo-package.toml", &package("sql", "pgsql")), ("edo.scm", "")]);
        let b = registry(&[("edo-package.toml", &package("sql", "mysql")), ("edo.scm", "")]);
        let home = tempfile::tempdir().unwrap();
        let fetch = local(vec![("github.com/a/sql", &a), ("github.com/b/sql", &b)]);
        install(home.path(), "a/sql", &fetch);
        install(home.path(), "b/sql", &fetch);
        // When: 名前だけで削除する
        let report = remove(home.path(), "sql");
        // Then: ambiguous になり、どちらも消えない
        assert_eq!(report["error"], "ambiguous");
        assert!(home.path().join("packages/github.com/a/sql/sql").exists());
    }

    #[test]
    fn installed_packages_load_as_languages() {
        // Given: .myrs を宣言するパッケージをインストールしたインストール先
        let repo = langs_repo();
        let home = tempfile::tempdir().unwrap();
        install(home.path(), "foo/langs:myrust", &local(vec![("github.com/foo/langs", &repo)]));
        // When: 言語を読み込む
        let langs = languages(home.path());
        // Then: .myrs のファイルにそのパッケージが選ばれる
        assert_eq!(langs.for_path(Path::new("a.myrs")).map(|l| l.name.as_str()), Some("myrust"));
    }

    /// A home with myrust and other installed, and other's manifest gone.
    fn home_with_broken_package() -> tempfile::TempDir {
        let repo = langs_repo();
        let home = tempfile::tempdir().unwrap();
        let fetch = local(vec![("github.com/foo/langs", &repo)]);
        install_all(home.path(), &["foo/langs:myrust".into(), "foo/langs:other".into()], &fetch);
        std::fs::remove_file(home.path().join("packages/github.com/foo/langs/other/edo-package.toml")).unwrap();
        home
    }

    #[test]
    fn package_without_manifest_does_not_stop_others_loading() {
        // Given: 2 つのパッケージのうち、片方の manifest が消えたインストール先
        let home = home_with_broken_package();
        // When: 言語を読み込む
        let langs = languages(home.path());
        // Then: もう片方は読み込まれる
        assert_eq!(langs.for_path(Path::new("a.myrs")).map(|l| l.name.as_str()), Some("myrust"));
    }

    #[test]
    fn package_without_manifest_can_be_removed() {
        // Given: manifest が消えたパッケージのあるインストール先
        let home = home_with_broken_package();
        // When: そのパッケージを削除する
        let report = remove(home.path(), "other");
        // Then: 削除できる
        assert_eq!(report["ok"], true, "{report}");
        assert!(!home.path().join("packages/github.com/foo/langs/other").exists());
    }

    #[test]
    fn directory_not_recorded_in_cache_is_not_loaded() {
        // Given: edo-cache.toml に記録せず、手で置いたパッケージのディレクトリ
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("packages/github.com/foo/langs/myrust");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("edo-package.toml"), package("myrust", "myrs")).unwrap();
        std::fs::write(dir.join("edo.scm"), "").unwrap();
        // When: 言語を読み込む
        let langs = languages(home.path());
        // Then: そのパッケージは選ばれない
        assert!(langs.for_path(Path::new("a.myrs")).is_none());
    }

    #[test]
    fn registry_without_name_is_error() {
        // Given: 2 つのパッケージを持つ [registry] のレジストリ
        let repo = langs_repo();
        let home = tempfile::tempdir().unwrap();
        // When: 名前なしでインストールする
        let report = install(home.path(), "foo/langs", &local(vec![("github.com/foo/langs", &repo)]));
        // Then: エラーになり、何も入らない
        assert_eq!(report["error"], "name_required", "{report}");
        assert!(installed(home.path()).unwrap().is_empty());
    }

    #[test]
    fn fetched_temporary_directory_is_removed_after_install() {
        // Given: レジストリを一時ディレクトリに取得する fetcher
        let home = tempfile::tempdir().unwrap();
        let fetched_at = std::cell::RefCell::new(PathBuf::new());
        let fetch = |_: &str| {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("edo-package.toml"), package("myrust", "myrs")).unwrap();
            std::fs::write(tmp.path().join("edo.scm"), "").unwrap();
            *fetched_at.borrow_mut() = tmp.path().to_path_buf();
            Ok(Fetched { dir: tmp.path().to_path_buf(), commit: "abc".into(), tmp: Some(tmp) })
        };
        // When: インストールする
        let report = install(home.path(), "foo/myrust", &fetch);
        // Then: インストールは成功し、一時ディレクトリは消える
        assert_eq!(report["ok"], true, "{report}");
        assert!(!fetched_at.borrow().exists());
    }

    #[test]
    fn added_registry_becomes_known_with_its_packages() {
        // Given: 2 つのパッケージを持つレジストリ
        let repo = langs_repo();
        let home = tempfile::tempdir().unwrap();
        // When: レジストリを追加する
        let report = add_registry(home.path(), "foo/langs", &local(vec![("github.com/foo/langs", &repo)]));
        // Then: 既知になり、パッケージの一覧が記録される
        assert_eq!(report["ok"], true, "{report}");
        let cache = Cache::load(home.path()).unwrap();
        let known = cache.registries.iter().find(|r| r.id() == "github.com/foo/langs").unwrap();
        assert_eq!(known.packages, ["myrust", "other"]);
    }

    #[test]
    fn removed_registry_is_forgotten_but_its_packages_stay() {
        // Given: パッケージを 1 つ入れたレジストリ
        let repo = langs_repo();
        let home = tempfile::tempdir().unwrap();
        install(home.path(), "foo/langs:myrust", &local(vec![("github.com/foo/langs", &repo)]));
        // When: レジストリを外す
        let report = remove_registry(home.path(), "foo/langs");
        // Then: 既知でなくなり、パッケージは残る
        assert_eq!(report["ok"], true, "{report}");
        let cache = Cache::load(home.path()).unwrap();
        assert!(cache.registries.iter().all(|r| r.id() != "github.com/foo/langs"));
        assert_eq!(cache.packages.len(), 1);
    }

    #[test]
    fn removing_unknown_registry_is_not_found() {
        // Given: 何も追加していないホーム
        let home = tempfile::tempdir().unwrap();
        // When: 既知でないレジストリを外す
        let report = remove_registry(home.path(), "foo/langs");
        // Then: not_found になる
        assert_eq!(report["error"], "not_found");
    }
}

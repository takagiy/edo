use std::path::{Path, PathBuf};

use crate::Error;
use crate::language::{Language, Manifest};
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
        if entry.path().join("edo-package.toml").is_file() && globs.is_match(&rel)
            && let Some(p) = manifest(entry.path())?.package {
                out.push((p.name, entry.path().to_path_buf()));
            }
    }
    out.sort();
    Ok(out)
}

/// Holds where an installed package came from.
const SOURCE_FILE: &str = ".edo-source";

/// A registry checked out to a local directory.
pub struct Fetched {
    pub dir: PathBuf,
    /// Where it came from, such as `github.com/owner/repo@<commit>`.
    pub source: String,
    /// The download's temporary directory; dropping it deletes the checkout.
    pub tmp: Option<tempfile::TempDir>,
}

/// `<config>/edo/packages`, where packages are installed.
pub fn store_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("edo").join("packages")
}

/// Downloads a GitHub registry's default branch as a tarball and unpacks it into a temporary directory.
/// shortcut: only github.com registries can be fetched; add other hosts' archive URLs when someone needs them.
pub fn fetch_github(registry: &str) -> Result<Fetched, Error> {
    let dir = tempfile::tempdir().map_err(|e| Error::new("io", e.to_string()))?;
    let tmp = dir.path();
    let repo = registry
        .strip_prefix("github.com/")
        .ok_or_else(|| Error::new("unsupported_registry", format!("{registry}: only github.com registries are supported")))?;
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
    Ok(Fetched { dir: top.path(), source: format!("{registry}@{commit}"), tmp: Some(dir) })
}

/// Installs one package into `store` (`<config>/edo/packages`) and reports the outcome.
pub fn install(store: &Path, spec: &str, fetch: &dyn Fn(&str) -> Result<Fetched, Error>) -> Value {
    try_install(store, spec, fetch).unwrap_or_else(|e| json!({"ok": false, "spec": spec, "error": e.code, "message": e.message}))
}

fn try_install(store: &Path, spec: &str, fetch: &dyn Fn(&str) -> Result<Fetched, Error>) -> Result<Value, Error> {
    let spec = parse_spec(spec);
    let registry = spec.registry.as_deref().unwrap_or(DEFAULT_REGISTRY);
    let fetched = fetch(registry)?;
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
    let target = store.join(registry).join(&name);
    let extensions = manifest(&dir)?.package.map(|p| p.extensions).unwrap_or_default();
    for other in installed(store)?.iter().filter(|p| p.dir != target) {
        if let Some(ext) = other.extensions.iter().find(|e| extensions.contains(e)) {
            let message = format!(".{ext} is already handled by {}:{}", other.registry, other.name);
            return Err(Error::new("extension_conflict", message));
        }
    }
    let source_file = target.join(SOURCE_FILE);
    let previous = std::fs::read_to_string(&source_file).ok();
    let io = |e: std::io::Error| Error::new("io", format!("{}: {e}", target.display()));
    if target.exists() {
        std::fs::remove_dir_all(&target).map_err(io)?;
    }
    copy_dir(&dir, &target).map_err(io)?;
    std::fs::write(&source_file, &fetched.source).map_err(io)?;
    let mut report = json!({"ok": true, "name": name, "registry": registry, "source": fetched.source});
    if let Some(previous) = previous {
        report["replaced_source"] = json!(previous);
    }
    Ok(report)
}

pub fn install_all(store: &Path, specs: &[String], fetch: &dyn Fn(&str) -> Result<Fetched, Error>) -> Vec<Value> {
    specs.iter().map(|s| install(store, s, fetch)).collect()
}

/// Removes an installed package given as `<name>` or `<repo>:<name>`.
pub fn remove(store: &Path, spec: &str) -> Value {
    let parsed = parse_spec(spec);
    let name = parsed.name.as_deref().unwrap_or("");
    let found: Vec<Installed> = match installed(store) {
        Ok(all) => all.into_iter().filter(|p| p.name == name && parsed.registry.as_ref().is_none_or(|r| *r == p.registry)).collect(),
        Err(e) => return json!({"ok": false, "spec": spec, "error": e.code, "message": e.message}),
    };
    match found.as_slice() {
        [] => json!({"ok": false, "spec": spec, "error": "not_found"}),
        [p] => match std::fs::remove_dir_all(&p.dir) {
            Ok(()) => json!({"ok": true, "name": p.name, "registry": p.registry}),
            Err(e) => json!({"ok": false, "spec": spec, "error": "io", "message": e.to_string()}),
        },
        many => {
            let candidates: Vec<String> = many.iter().map(|p| format!("{}:{}", p.registry, p.name)).collect();
            json!({"ok": false, "spec": spec, "error": "ambiguous", "matches": candidates})
        }
    }
}

/// Builtin languages plus the installed ones, which win on shared extensions.
/// A package that fails to load is skipped so one broken package does not disable edo.
pub fn languages(store: &Path) -> crate::language::Languages {
    let loaded = installed(store).unwrap_or_default().iter().filter_map(|p| Language::load_dir(&p.dir).ok()).collect();
    crate::language::Languages::with_installed(loaded)
}

/// Builtin and installed packages.
pub fn list(store: &Path) -> Result<Value, Error> {
    let builtin: Vec<Value> = crate::language::Languages::builtin()
        .iter()
        .map(|l| json!({"name": l.name, "extensions": l.extensions}))
        .collect();
    let installed: Vec<Value> = installed(store)?
        .into_iter()
        .map(|p| {
            let source = std::fs::read_to_string(p.dir.join(SOURCE_FILE)).unwrap_or_default();
            json!({"registry": p.registry, "name": p.name, "extensions": p.extensions, "source": source})
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
    pub dir: PathBuf,
}

/// Every package in the store (`<store>/<registry>/<name>/edo-package.toml`).
pub fn installed(store: &Path) -> Result<Vec<Installed>, Error> {
    let mut out = vec![];
    if !store.exists() {
        return Ok(out);
    }
    for entry in ignore::WalkBuilder::new(store).standard_filters(false).build().filter_map(Result::ok) {
        if entry.file_name() != "edo-package.toml" {
            continue;
        }
        let dir = entry.path().parent().unwrap_or(store).to_path_buf();
        let Some(p) = manifest(&dir)?.package else { continue };
        let rel = dir.parent().and_then(|d| d.strip_prefix(store).ok()).unwrap_or(Path::new(""));
        let registry = rel.to_string_lossy().replace('\\', "/");
        out.push(Installed { registry, name: p.name, extensions: p.extensions, dir });
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
        let store = tempfile::tempdir().unwrap();
        // When: 名前だけを指定してインストールする
        let report = install(store.path(), "myrust", &local(vec![(DEFAULT_REGISTRY, &repo)]));
        // Then: デフォルトのレジストリから探して入る
        assert_eq!(report["ok"], true, "{report}");
        assert!(store.path().join(DEFAULT_REGISTRY).join("myrust/edo-package.toml").is_file());
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
        format!("[package]\nname = \"{name}\"\nextensions = [\"{ext}\"]\ngrammar = \"builtin:rust\"\n")
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
        let repos: Vec<(String, PathBuf)> = repos.into_iter().map(|(id, d)| (id.to_string(), d.path().to_path_buf())).collect();
        move |id: &str| {
            let (_, dir) = repos.iter().find(|(r, _)| r == id).ok_or_else(|| Error::new("not_found", id))?;
            Ok(Fetched { dir: dir.clone(), source: format!("{id}@abc"), tmp: None })
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
        let store = tempfile::tempdir().unwrap();
        // When: <repo>:<name> を指定してインストールする
        let report = install(store.path(), "foo/langs:myrust", &local(vec![("github.com/foo/langs", &repo)]));
        // Then: <インストール先>/<registry>/<name>/ に入る
        assert_eq!(report["ok"], true, "{report}");
        assert!(store.path().join("github.com/foo/langs/myrust/edo-package.toml").is_file());
    }

    #[test]
    fn installed_package_records_its_source() {
        // Given: パッケージを持つレジストリ
        let repo = langs_repo();
        let store = tempfile::tempdir().unwrap();
        // When: インストールする
        install(store.path(), "foo/langs:myrust", &local(vec![("github.com/foo/langs", &repo)]));
        // Then: 入手元が記録される
        let source = std::fs::read_to_string(store.path().join("github.com/foo/langs/myrust/.edo-source")).unwrap();
        assert_eq!(source, "github.com/foo/langs@abc");
    }

    #[test]
    fn reinstalling_same_registry_package_overwrites_and_reports_previous_source() {
        // Given: 一度インストールしたパッケージ
        let repo = langs_repo();
        let store = tempfile::tempdir().unwrap();
        install(store.path(), "foo/langs:myrust", &local(vec![("github.com/foo/langs", &repo)]));
        // When: 新しいコミットから同じパッケージを入れる
        let newer = |id: &str| Ok(Fetched { dir: repo.path().to_path_buf(), source: format!("{id}@def"), tmp: None });
        let report = install(store.path(), "foo/langs:myrust", &newer);
        // Then: 上書きされ、前の入手元が報告される
        assert_eq!(report["replaced_source"], "github.com/foo/langs@abc");
    }

    #[test]
    fn same_name_packages_from_different_registries_coexist() {
        // Given: 同じ名前で拡張子の違うパッケージを持つ 2 つのレジストリ
        let a = registry(&[("edo-package.toml", &package("sql", "pgsql")), ("edo.scm", "")]);
        let b = registry(&[("edo-package.toml", &package("sql", "mysql")), ("edo.scm", "")]);
        let store = tempfile::tempdir().unwrap();
        let fetch = local(vec![("github.com/a/sql", &a), ("github.com/b/sql", &b)]);
        // When: 両方をインストールする
        install(store.path(), "a/sql", &fetch);
        let report = install(store.path(), "b/sql", &fetch);
        // Then: どちらも残る
        assert_eq!(report["ok"], true, "{report}");
        assert!(store.path().join("github.com/a/sql/sql/edo-package.toml").is_file());
        assert!(store.path().join("github.com/b/sql/sql/edo-package.toml").is_file());
    }

    #[test]
    fn extension_conflict_with_installed_package_is_error() {
        // Given: 同じ拡張子を宣言するパッケージを持つ 2 つのレジストリ
        let a = registry(&[("edo-package.toml", &package("kt1", "kt")), ("edo.scm", "")]);
        let b = registry(&[("edo-package.toml", &package("kt2", "kt")), ("edo.scm", "")]);
        let store = tempfile::tempdir().unwrap();
        let fetch = local(vec![("github.com/a/kt", &a), ("github.com/b/kt", &b)]);
        install(store.path(), "a/kt", &fetch);
        // When: 2 つ目をインストールする
        let report = install(store.path(), "b/kt", &fetch);
        // Then: 拡張子の衝突でエラーになり、入らない
        assert_eq!(report["error"], "extension_conflict");
        assert!(!store.path().join("github.com/b/kt").exists());
    }

    #[test]
    fn several_specs_give_one_result_each() {
        // Given: 2 つのパッケージを持つレジストリ
        let repo = langs_repo();
        let store = tempfile::tempdir().unwrap();
        // When: 2 つを同時に指定してインストールする
        let specs = ["foo/langs:myrust".to_string(), "foo/langs:other".to_string()];
        let reports = install_all(store.path(), &specs, &local(vec![("github.com/foo/langs", &repo)]));
        // Then: パッケージごとに結果が返る
        let names: Vec<_> = reports.iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["myrust", "other"]);
    }

    #[test]
    fn list_shows_builtin_and_installed_packages() {
        // Given: 1 つインストールしたインストール先
        let repo = langs_repo();
        let store = tempfile::tempdir().unwrap();
        install(store.path(), "foo/langs:myrust", &local(vec![("github.com/foo/langs", &repo)]));
        // When: 一覧を出す
        let listed = list(store.path()).unwrap();
        // Then: 組み込みとインストール済みのパッケージが並ぶ
        assert_eq!(listed["builtin"][0]["name"], "rust");
        assert_eq!(
            listed["installed"],
            json!([{"registry": "github.com/foo/langs", "name": "myrust", "extensions": ["myrs"], "source": "github.com/foo/langs@abc"}])
        );
    }

    #[test]
    fn removing_package_deletes_its_directory() {
        // Given: インストールしたパッケージ
        let repo = langs_repo();
        let store = tempfile::tempdir().unwrap();
        install(store.path(), "foo/langs:myrust", &local(vec![("github.com/foo/langs", &repo)]));
        // When: 名前を指定して削除する
        let report = remove(store.path(), "myrust");
        // Then: パッケージのディレクトリが消える
        assert_eq!(report["ok"], true, "{report}");
        assert!(!store.path().join("github.com/foo/langs/myrust").exists());
    }

    #[test]
    fn bare_name_matching_several_packages_is_ambiguous() {
        // Given: 違うレジストリから入れた同名のパッケージ
        let a = registry(&[("edo-package.toml", &package("sql", "pgsql")), ("edo.scm", "")]);
        let b = registry(&[("edo-package.toml", &package("sql", "mysql")), ("edo.scm", "")]);
        let store = tempfile::tempdir().unwrap();
        let fetch = local(vec![("github.com/a/sql", &a), ("github.com/b/sql", &b)]);
        install(store.path(), "a/sql", &fetch);
        install(store.path(), "b/sql", &fetch);
        // When: 名前だけで削除する
        let report = remove(store.path(), "sql");
        // Then: ambiguous になり、どちらも消えない
        assert_eq!(report["error"], "ambiguous");
        assert!(store.path().join("github.com/a/sql/sql").exists());
    }

    #[test]
    fn installed_packages_load_as_languages() {
        // Given: .myrs を宣言するパッケージをインストールしたインストール先
        let repo = langs_repo();
        let store = tempfile::tempdir().unwrap();
        install(store.path(), "foo/langs:myrust", &local(vec![("github.com/foo/langs", &repo)]));
        // When: 言語を読み込む
        let langs = languages(store.path());
        // Then: .myrs のファイルにそのパッケージが選ばれる
        assert_eq!(langs.for_path(Path::new("a.myrs")).map(|l| l.name.as_str()), Some("myrust"));
    }

    #[test]
    fn registry_without_name_is_error() {
        // Given: 2 つのパッケージを持つ [registry] のレジストリ
        let repo = langs_repo();
        let store = tempfile::tempdir().unwrap();
        // When: 名前なしでインストールする
        let report = install(store.path(), "foo/langs", &local(vec![("github.com/foo/langs", &repo)]));
        // Then: エラーになり、何も入らない
        assert_eq!(report["error"], "name_required", "{report}");
        assert!(installed(store.path()).unwrap().is_empty());
    }

    #[test]
    fn fetched_temporary_directory_is_removed_after_install() {
        // Given: レジストリを一時ディレクトリに取得する fetcher
        let store = tempfile::tempdir().unwrap();
        let fetched_at = std::cell::RefCell::new(PathBuf::new());
        let fetch = |id: &str| {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("edo-package.toml"), package("myrust", "myrs")).unwrap();
            std::fs::write(tmp.path().join("edo.scm"), "").unwrap();
            *fetched_at.borrow_mut() = tmp.path().to_path_buf();
            Ok(Fetched { dir: tmp.path().to_path_buf(), source: format!("{id}@abc"), tmp: Some(tmp) })
        };
        // When: インストールする
        let report = install(store.path(), "foo/myrust", &fetch);
        // Then: インストールは成功し、一時ディレクトリは消える
        assert_eq!(report["ok"], true, "{report}");
        assert!(!fetched_at.borrow().exists());
    }
}

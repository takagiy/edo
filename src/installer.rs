//! The interactive installer that `edo lang install` opens without arguments.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{cursor, execute, queue};
use serde_json::Value;

use crate::Error;
use crate::install::{self, Cache, Fetched, KnownRegistry};

type Fetch<'a> = &'a dyn Fn(&str) -> Result<Fetched, Error>;

const HELP: &str = "↑↓: move | tab: select | U: update registry | enter: submit";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Package {
    registry: String,
    name: String,
}

impl Package {
    fn spec(&self) -> String {
        format!("{}:{}", self.registry, self.name)
    }
}

#[derive(Debug, PartialEq)]
enum Row {
    Header(String),
    Note(&'static str),
    Package(Package),
    Blank,
}

pub enum Input {
    Up,
    Down,
    Toggle,
    Submit,
    Update,
    Cancel,
}

#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    Update,
    /// Specs (`<registry>:<name>`) to install and to remove.
    Submit {
        install: Vec<String>,
        remove: Vec<String>,
    },
    Cancel,
}

/// The installer's screen: packages grouped by registry, with a cursor and a selection.
pub struct Picker {
    rows: Vec<Row>,
    installed: BTreeSet<Package>,
    selected: BTreeSet<Package>,
    /// Index into `rows`; always on a package row when there is one.
    cursor: usize,
    /// Shown above the key help, such as fetch errors.
    pub status: String,
}

impl Picker {
    pub fn new(cache: &Cache) -> Picker {
        let installed: BTreeSet<Package> =
            cache.packages.iter().map(|p| Package { registry: p.registry.clone(), name: p.name.clone() }).collect();
        let mut picker =
            Picker { rows: vec![], selected: installed.clone(), installed, cursor: 0, status: String::new() };
        picker.reload(cache);
        picker
    }

    /// Rebuilds the rows from the cache, keeping the selection and the package under the cursor.
    pub fn reload(&mut self, cache: &Cache) {
        let current = self.rows.get(self.cursor).and_then(as_package).cloned();
        self.rows = rows(cache);
        let packages = || self.rows.iter().enumerate().filter(|(_, r)| as_package(r).is_some());
        let mut at = packages().filter(|(_, r)| as_package(r) == current.as_ref());
        self.cursor = at.next().or_else(|| packages().next()).map_or(0, |(i, _)| i);
    }

    pub fn handle(&mut self, input: Input) -> Action {
        let is_package = |i: &usize| as_package(&self.rows[*i]).is_some();
        match input {
            Input::Up => self.cursor = (0..self.cursor).rev().find(is_package).unwrap_or(self.cursor),
            Input::Down => self.cursor = (self.cursor + 1..self.rows.len()).find(is_package).unwrap_or(self.cursor),
            Input::Toggle => {
                if let Some(p) = self.rows.get(self.cursor).and_then(as_package)
                    && !self.selected.remove(p)
                {
                    self.selected.insert(p.clone());
                }
            }
            Input::Submit => {
                let install = self.selected.difference(&self.installed).map(Package::spec).collect();
                let remove = self.installed.difference(&self.selected).map(Package::spec).collect();
                return Action::Submit { install, remove };
            }
            Input::Update => return Action::Update,
            Input::Cancel => return Action::Cancel,
        }
        Action::None
    }

    /// The screen's lines, and the line the cursor is on.
    pub fn render(&self) -> (Vec<String>, Option<usize>) {
        let mut lines: Vec<String> = self
            .rows
            .iter()
            .map(|r| match r {
                Row::Header(label) => label.clone(),
                Row::Note(note) => note.to_string(),
                Row::Package(p) => format!("[{}] {}", if self.selected.contains(p) { 'x' } else { ' ' }, p.name),
                Row::Blank => String::new(),
            })
            .collect();
        lines.push(String::new());
        lines.extend(self.status.lines().map(String::from));
        lines.push(HELP.into());
        let cursor = self.rows.get(self.cursor).and_then(as_package).map(|_| self.cursor);
        (lines, cursor)
    }
}

fn as_package(row: &Row) -> Option<&Package> {
    match row {
        Row::Package(p) => Some(p),
        _ => None,
    }
}

/// Known registries in order, then registries that only installed packages still name.
fn rows(cache: &Cache) -> Vec<Row> {
    let mut ids: Vec<String> = cache.registries.iter().map(KnownRegistry::id).collect();
    for p in &cache.packages {
        if !ids.contains(&p.registry) {
            ids.push(p.registry.clone());
        }
    }
    let mut rows = vec![];
    for id in ids {
        let known = cache.registries.iter().find(|r| r.id() == id);
        let mut names: BTreeSet<&str> = known.iter().flat_map(|r| r.packages.iter().map(String::as_str)).collect();
        names.extend(cache.packages.iter().filter(|p| p.registry == id).map(|p| p.name.as_str()));
        if !rows.is_empty() {
            rows.push(Row::Blank);
        }
        rows.push(Row::Header(id.strip_prefix("github.com/").unwrap_or(&id).to_string()));
        if names.is_empty() {
            let fetched = known.is_none_or(|r| r.commit.is_some());
            rows.push(Row::Note(if fetched { "(no packages)" } else { "(not fetched)" }));
        }
        let packages = names.into_iter().map(|name| Package { registry: id.clone(), name: name.into() });
        rows.extend(packages.map(Row::Package));
    }
    rows
}

/// Fetches the registries, returning one line per failure.
fn fetch_all(home: &Path, ids: &[String], fetch: Fetch) -> String {
    let errors = ids.iter().filter_map(|id| install::update_registry(home, id, fetch).err());
    errors.map(|e| e.message).collect::<Vec<_>>().join("\n")
}

/// Opens the picker after fetching the registries that were never fetched.
pub fn start(home: &Path, fetch: Fetch) -> Result<Picker, Error> {
    let cache = Cache::load(home)?;
    let unfetched: Vec<String> =
        cache.registries.iter().filter(|r| r.commit.is_none()).map(KnownRegistry::id).collect();
    let status = fetch_all(home, &unfetched, fetch);
    let mut picker = Picker::new(&Cache::load(home)?);
    picker.status = status;
    Ok(picker)
}

/// Refetches every known registry and shows the new package lists.
pub fn update(picker: &mut Picker, home: &Path, fetch: Fetch) {
    let ids: Vec<String> = match Cache::load(home) {
        Ok(cache) => cache.registries.iter().map(KnownRegistry::id).collect(),
        Err(e) => return picker.status = e.message,
    };
    picker.status = fetch_all(home, &ids, fetch);
    match Cache::load(home) {
        Ok(cache) => picker.reload(&cache),
        Err(e) => picker.status = e.message,
    }
}

/// Removes first, so a package can be swapped for another that claims the same extensions.
pub fn apply(home: &Path, install: &[String], remove: &[String], fetch: Fetch) -> Vec<Value> {
    let removed = remove.iter().map(|s| install::remove(home, s));
    removed.chain(install.iter().map(|s| install::install(home, s, fetch))).collect()
}

/// Runs the installer on the terminal (drawn on stderr) and reports what changed.
pub fn run(home: &Path, fetch: Fetch) -> Result<Vec<Value>, Error> {
    let io = |e: std::io::Error| Error::new("io", e.to_string());
    let mut picker = start(home, fetch)?;
    let mut screen = Screen::open().map_err(io)?;
    loop {
        screen.draw(&picker).map_err(io)?;
        let Some(input) = Screen::read().map_err(io)? else { continue };
        match picker.handle(input) {
            Action::None => {}
            Action::Update => {
                picker.status = "updating...".into();
                screen.draw(&picker).map_err(io)?;
                update(&mut picker, home, fetch);
            }
            Action::Cancel => return Ok(vec![]),
            Action::Submit { install, remove } => {
                drop(screen);
                return Ok(apply(home, &install, &remove, fetch));
            }
        }
    }
}

/// Raw mode on stderr, redrawn in place; restored when dropped.
// shortcut: redraws by moving up over the last frame, so a list taller than the terminal smears; scroll if it does.
struct Screen {
    lines: u16,
}

impl Screen {
    fn open() -> std::io::Result<Screen> {
        terminal::enable_raw_mode()?;
        execute!(std::io::stderr(), cursor::Hide)?;
        Ok(Screen { lines: 0 })
    }

    fn draw(&mut self, picker: &Picker) -> std::io::Result<()> {
        let (lines, at) = picker.render();
        let mut err = std::io::stderr().lock();
        if self.lines > 0 {
            queue!(err, cursor::MoveToPreviousLine(self.lines))?;
        }
        queue!(err, Clear(ClearType::FromCursorDown))?;
        for (i, line) in lines.iter().enumerate() {
            if Some(i) == at {
                queue!(err, SetAttribute(Attribute::Reverse), Print(line), SetAttribute(Attribute::Reset))?;
            } else {
                queue!(err, Print(line))?;
            }
            queue!(err, Print("\r\n"))?;
        }
        self.lines = lines.len() as u16;
        err.flush()
    }

    fn read() -> std::io::Result<Option<Input>> {
        let Event::Key(key) = event::read()? else { return Ok(None) };
        if key.kind != KeyEventKind::Press {
            return Ok(None);
        }
        Ok(match key.code {
            KeyCode::Up => Some(Input::Up),
            KeyCode::Down => Some(Input::Down),
            KeyCode::Tab => Some(Input::Toggle),
            KeyCode::Enter => Some(Input::Submit),
            KeyCode::Char('u' | 'U') => Some(Input::Update),
            KeyCode::Esc => Some(Input::Cancel),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(Input::Cancel),
            _ => None,
        })
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(std::io::stderr(), cursor::Show);
        let _ = terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::{InstalledPackage, KnownRegistry, RegistryKind};

    fn known(repo: &str, commit: Option<&str>, packages: &[&str]) -> KnownRegistry {
        let packages = packages.iter().map(|p| p.to_string()).collect();
        KnownRegistry { kind: RegistryKind::Github, repo: repo.into(), commit: commit.map(String::from), packages }
    }

    fn installed(registry: &str, name: &str) -> InstalledPackage {
        InstalledPackage { registry: registry.into(), name: name.into(), commit: "abc".into() }
    }

    /// user/repo has bar and foo (installed); user2/repo has baz.
    fn cache() -> Cache {
        Cache {
            registries: vec![
                known("user/repo", Some("abc"), &["bar", "foo"]),
                known("user2/repo", Some("abc"), &["baz"]),
            ],
            packages: vec![installed("github.com/user/repo", "foo")],
        }
    }

    fn lines(picker: &Picker) -> Vec<String> {
        picker.render().0
    }

    #[test]
    fn known_registries_list_their_packages_under_headers() {
        // Given: パッケージを持つ 2 つの既知のレジストリ
        // When: 画面を描く
        let picker = Picker::new(&cache());
        // Then: レジストリごとに、見出しの下にパッケージが並ぶ
        assert_eq!(lines(&picker)[..6], ["user/repo", "[ ] bar", "[x] foo", "", "user2/repo", "[ ] baz"]);
    }

    #[test]
    fn installed_package_starts_selected() {
        // Given: foo をインストール済みのキャッシュ
        // When: 画面を描く
        let picker = Picker::new(&cache());
        // Then: foo は選択された状態で始まる
        assert_eq!(lines(&picker)[2], "[x] foo");
    }

    #[test]
    fn package_from_unknown_registry_is_listed_under_its_registry() {
        // Given: 既知でないレジストリから入れたパッケージ
        let mut cache = cache();
        cache.packages.push(installed("github.com/gone/repo", "qux"));
        // When: 画面を描く
        let picker = Picker::new(&cache);
        // Then: そのレジストリの見出しの下に並ぶ
        assert_eq!(lines(&picker)[6..9], ["", "gone/repo", "[x] qux"]);
    }

    #[test]
    fn registry_never_fetched_is_shown_as_not_fetched() {
        // Given: 一度も取得していないレジストリ
        let cache = Cache { registries: vec![known("new/repo", None, &[])], packages: vec![] };
        // When: 画面を描く
        let picker = Picker::new(&cache);
        // Then: 未取得と表示される
        assert_eq!(lines(&picker)[..2], ["new/repo", "(not fetched)"]);
    }

    #[test]
    fn screen_ends_with_key_help() {
        // Given / When: 画面を描く
        let picker = Picker::new(&cache());
        // Then: 最後に操作の説明が出る
        assert_eq!(lines(&picker).last().unwrap(), "↑↓: move | tab: select | U: update registry | enter: submit");
    }

    #[test]
    fn cursor_starts_on_first_package() {
        // Given / When: 画面を描く
        let picker = Picker::new(&cache());
        // Then: カーソルは最初のパッケージの行にある
        assert_eq!(picker.render().1, Some(1));
    }

    #[test]
    fn down_moves_to_next_package_skipping_headers() {
        // Given: user/repo の最後のパッケージにあるカーソル
        let mut picker = Picker::new(&cache());
        picker.handle(Input::Down);
        // When: ↓ を押す
        picker.handle(Input::Down);
        // Then: 見出しを飛ばして user2/repo の baz に移る
        assert_eq!(picker.render().1, Some(5));
    }

    #[test]
    fn down_on_last_package_stays() {
        // Given: 最後のパッケージにあるカーソル
        let mut picker = Picker::new(&cache());
        picker.handle(Input::Down);
        picker.handle(Input::Down);
        // When: ↓ を押す
        picker.handle(Input::Down);
        // Then: カーソルは動かない
        assert_eq!(picker.render().1, Some(5));
    }

    #[test]
    fn tab_toggles_package_under_cursor() {
        // Given: 未選択の bar にあるカーソル
        let mut picker = Picker::new(&cache());
        // When: tab を押す
        picker.handle(Input::Toggle);
        // Then: bar が選択される
        assert_eq!(lines(&picker)[1], "[x] bar");
    }

    #[test]
    fn enter_installs_selected_uninstalled_packages() {
        // Given: 未インストールの bar を選択した画面
        let mut picker = Picker::new(&cache());
        picker.handle(Input::Toggle);
        // When: enter を押す
        let action = picker.handle(Input::Submit);
        // Then: bar がインストール対象になる
        assert_eq!(action, Action::Submit { install: vec!["github.com/user/repo:bar".into()], remove: vec![] });
    }

    #[test]
    fn enter_removes_unselected_installed_packages() {
        // Given: インストール済みの foo の選択を外した画面
        let mut picker = Picker::new(&cache());
        picker.handle(Input::Down);
        picker.handle(Input::Toggle);
        // When: enter を押す
        let action = picker.handle(Input::Submit);
        // Then: foo が削除対象になる
        assert_eq!(action, Action::Submit { install: vec![], remove: vec!["github.com/user/repo:foo".into()] });
    }

    #[test]
    fn esc_ends_without_changes() {
        // Given: bar を選択した画面
        let mut picker = Picker::new(&cache());
        picker.handle(Input::Toggle);
        // When: esc を押す
        let action = picker.handle(Input::Cancel);
        // Then: 何も変えずに終わる
        assert_eq!(action, Action::Cancel);
    }

    /// A registry checkout whose root lists `*` and holds a package per name.
    fn registry(names: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("edo-package.toml"), "[registry]\npackages = [\"*\"]\n").unwrap();
        for name in names {
            add_package(dir.path(), name, name);
        }
        dir
    }

    fn add_package(root: &Path, name: &str, ext: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = format!(
            "[package]\nname = \"{name}\"\nextensions = [\"{ext}\"]\n\n[package.syntax]\nmodule = \"builtin:rust\"\n"
        );
        std::fs::write(dir.join("edo-package.toml"), manifest).unwrap();
        std::fs::write(dir.join("edo.scm"), "").unwrap();
    }

    fn serve(root: &Path) -> impl Fn(&str) -> Result<Fetched, Error> {
        let root = root.to_path_buf();
        move |_| Ok(Fetched { dir: root.clone(), commit: "abc".into(), tmp: None })
    }

    #[test]
    fn submit_removes_then_installs() {
        // Given: foo を入れたホームと、foo と同じ拡張子の bar を持つレジストリ
        let repo = registry(&["foo"]);
        let home = tempfile::tempdir().unwrap();
        install::install(home.path(), "user/repo:foo", &serve(repo.path()));
        add_package(repo.path(), "bar", "foo");
        // When: foo の削除と bar のインストールを確定する
        let install = ["github.com/user/repo:bar".to_string()];
        let remove = ["github.com/user/repo:foo".to_string()];
        let reports = apply(home.path(), &install, &remove, &serve(repo.path()));
        // Then: foo が消えてから bar が入るので、拡張子は衝突しない
        assert!(reports.iter().all(|r| r["ok"] == true), "{reports:?}");
        let names: Vec<String> = Cache::load(home.path()).unwrap().packages.into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["bar"]);
    }

    #[test]
    fn start_fetches_registries_never_fetched() {
        // Given: デフォルトのレジストリしか知らず、まだ何も取得していないホーム
        let repo = registry(&["foo"]);
        let home = tempfile::tempdir().unwrap();
        // When: 起動する
        let picker = start(home.path(), &serve(repo.path())).unwrap();
        // Then: デフォルトのレジストリが取得され、そのパッケージが並ぶ
        assert_eq!(lines(&picker)[..2], ["takagiy/edo", "[ ] foo"]);
    }

    #[test]
    fn update_refetches_known_registries() {
        // Given: 起動した後に、レジストリにパッケージが増える
        let repo = registry(&["foo"]);
        let home = tempfile::tempdir().unwrap();
        let mut picker = start(home.path(), &serve(repo.path())).unwrap();
        add_package(repo.path(), "bar", "bar");
        // When: 取り直す
        update(&mut picker, home.path(), &serve(repo.path()));
        // Then: 一覧が新しくなる
        assert_eq!(lines(&picker)[..3], ["takagiy/edo", "[ ] bar", "[ ] foo"]);
    }

    #[test]
    fn update_keeps_selection_and_cursor() {
        // Given: foo を選択してカーソルを置いた後に、レジストリの foo より前にパッケージが増える
        let repo = registry(&["foo"]);
        let home = tempfile::tempdir().unwrap();
        let mut picker = start(home.path(), &serve(repo.path())).unwrap();
        picker.handle(Input::Toggle);
        add_package(repo.path(), "bar", "bar");
        // When: 取り直す
        update(&mut picker, home.path(), &serve(repo.path()));
        // Then: foo は選択されたままで、カーソルも foo にある
        assert_eq!(lines(&picker)[2], "[x] foo");
        assert_eq!(picker.render().1, Some(2));
    }

    #[test]
    fn failed_fetch_shows_error_at_bottom() {
        // Given: 取得に失敗するレジストリ
        let home = tempfile::tempdir().unwrap();
        let fail = |id: &str| Err(Error::new("network", format!("{id}: unreachable")));
        // When: 起動する
        let picker = start(home.path(), &fail).unwrap();
        // Then: 操作の説明の上にエラーが出る
        let lines = lines(&picker);
        assert_eq!(lines[lines.len() - 2], "github.com/takagiy/edo: unreachable");
    }
}

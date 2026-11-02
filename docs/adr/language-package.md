# 言語対応を宣言的な言語パッケージにし、外部 grammar は WASM で読み込む

## 状況

対応言語を簡単に足せるようにしたい。言語ごとに必要なのは grammar、定義とボディの位置、照合規則、拡張子だけで、コードを書く必要はない。

## 決定

1 言語 = 1 ディレクトリの言語パッケージにする。

- `edo-package.toml` の `[package]` に `name` と `extensions` を、`[package.syntax]` に `module`（`grammar.wasm` か `builtin:<name>`）と、必要なら WASM から言語を取り出す関数の名前 `entrypoint`（省略時は `tree_sitter_<name>`）を書く。
- `edo.scm` に tree-sitter query で定義（`@definition.*`、`@name`）、省略できる場所（`@body`）、定義に紐づく doc（`@doc`）、照合規則（`@match.*`）を書く。
- 組み込みの 8 言語（Rust、TypeScript と TSX、JavaScript、Python、Go、Java、Kotlin、Markdown の 9 パッケージ）も同じ形式で書き、grammar だけ静的リンクする。
- レジストリはリポジトリのルートの `edo-package.toml` の `[registry] packages` に glob で並べる。edo 本体のリポジトリがデフォルトのレジストリを兼ねる。
- インストール先は `<config>/edo/packages/<registry>/<name>/`。何が入っているかは `<config>/edo/edo-cache.toml` に記録する（[パッケージのキャッシュ](package-cache.md) を参照）。言語は名前ではなく拡張子で選び、組み込みと拡張子が衝突したらインストール済みを優先する。

## 結果

- 得たもの: 拡張の作者が触るのは toml と query だけ。組み込み言語がそのまま拡張のお手本になる。query だけ差し替える拡張も作れる。
- 失ったもの: wasmtime の分だけバイナリが大きくなる。拡張の作者は `grammar.wasm` をビルドしてコミットする必要がある。組み込みの grammar の不具合はそのまま受け継ぐ（Kotlin は grammar を選び直した。[Kotlin の grammar](kotlin-grammar.md) を参照）。

## 採らなかった案

- ネイティブの .so/.dll の動的ロード: OS ごとのビルドが要り、ABI のずれで落ちる。
- コンパイル時だけの追加: edo 本体を変えないと言語を足せない。
- Rust の trait によるプラグイン: query で書けないことがほとんどなく、重いだけ。
- インストール時に grammar をソースからビルドする: emscripten や wasi-sdk が要る。

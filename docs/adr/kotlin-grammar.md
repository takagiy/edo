# Kotlin の grammar に brokk を使い、C のソースを submodule から自前でビルドする

## 状況

最初に組み込んだ tree-sitter-kotlin-ng は、`@Suppress("x") data object Foo` を式として誤ってパースし、`@Serializable` の次の行の `data class` では ERROR を出す。アノテーション付きの宣言は Kotlin でよく書かれる。

brokk-tree-sitter-kotlin はどちらも正しくパースするが、crate が `tree-sitter-language = "=0.1.7"` を固定しており、`^0.1.8` を要求する tree-sitter 0.27 と同じ依存グラフに入れられない。C のソースには手を入れる必要がなく、問題は manifest の固定だけ。

## 決定

workspace に `crates/edo-tree-sitter-kotlin` を作り、brokk のリポジトリを v0.4.8 の浅い git submodule（`brokk/`）として参照する。

- `build.rs` が `brokk/src/parser.c` と `scanner.c` を `cc` でコンパイルする。ほかの Kotlin grammar とぶつからないように、公開シンボルを `edo_tree_sitter_kotlin*` に変える。
- `Cargo.toml` は自前で書き、`tree-sitter-language = "0.1"` に依存する。brokk の manifest は使わない。
- submodule がなければ、`git submodule update --init` を促すメッセージでビルドを止める。

## 結果

- 得たもの: アノテーション付きの宣言を正しく扱える。brokk の中身（61 MB の `parser.c` を含む）をリポジトリに入れずに済む。brokk の更新は submodule のタグを進めるだけで取り込める。
- 失ったもの: clone のときに `--recurse-submodules`（または後からの `git submodule update --init`）が要る。61 MB の C をコンパイルするので、初回のビルドが遅くなる。

## 採らなかった案

- tree-sitter-kotlin-ng のまま使う: アノテーション付きの宣言を誤ってパースする。
- willbooster-tree-sitter-kotlin: ABI バージョンが WillBooster 独自のフォーク向け（`0x5742000b`）で、本家の tree-sitter が読み込めない。
- brokk のソースを `vendor/` にコピーする: 61 MB をリポジトリに入れることになる。
- brokk の Cargo.toml の固定を緩める `.patch` を当てる: その manifest はビルドに使わないので、パッチの中身が効かない。
- ビルド時に crates.io から .crate をダウンロードする: ビルドにネットワークが要る。

# v1 に入れなかったもの

## きっかけ

v1 の設計を詰める中で、後回しにしたもの。

## 案

- `edo lang update` と `<repo>@<ref>:<name>` によるバージョン固定。インストール時に `source` は記録してある。
- プロジェクトごとの言語パッケージ（リポジトリの `.edo/packages/`）。
- JSON / YAML / TOML の組み込み、またはテキストモード。
- CLI の `write --expect <hash>` によるステイル検査（MCP はサーバーが mtime を覚えて警告する）。
- `--lang` による言語の上書き、ファイル名や shebang による判定。
- github.com 以外のレジストリ（GitLab など）からのダウンロード。`<repo>` の解析は対応済みで、取得だけが github.com 限定。

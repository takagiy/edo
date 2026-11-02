# ハーネスへの組み込み

`edo harness init` が書き込む設定。

## Claude Code

- [x] MCP は `claude mcp add --scope user` に edo の絶対パスと `mcp` を渡して登録する
- [x] hook は `~/.claude/settings.json` の `PreToolUse`（Read、Edit、Write）と `PostToolUse`（Bash）に、edo の絶対パスで入る
- [x] settings.json のほかの設定とほかの hook はそのまま残る
- [x] もう一度入れても hook は重複しない
- [x] 別の場所の edo で入れ直すと、hook のパスが置き換わる
- [x] 書き換える前の settings.json は `.bak` に残る
- [x] `--remove` で edo の hook が消え、ほかの hook は残る
- [x] `--remove` で MCP は `claude mcp remove --scope user` で外れる

## 共通

- [x] ハーネスを省くと、設定ディレクトリがあるハーネスすべてが対象になる
- [x] 設定ディレクトリがないハーネスを指定すると、そのハーネスはエラーになる
- [x] 報告はハーネスごとの JSON の配列になる

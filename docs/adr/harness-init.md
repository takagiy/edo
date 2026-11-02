# ハーネスへの組み込みは、edo 自身が MCP の登録と 2 つの hook をユーザー全体の設定に書き込む

## 状況

edo はハーネスの Read/Write/Edit を置き換えるための道具だが、MCP サーバーを登録するだけでは、エージェントは builtin のツールを使い続けることが多い。Claude Code、Codex、Cursor に、edo を使うための設定を 1 つのコマンドで入れたい。rtk は `rtk init` で hook と説明のファイルを入れている。

調べた時点（2026 年 11 月）の各ハーネスの仕様:

| | MCP の登録 | 実行前に止める | 実行後に文を足す |
|---|---|---|---|
| Claude Code | `claude mcp add --scope user`（登録先の `~/.claude.json` は Claude Code が常に書き換える） | `PreToolUse` の `permissionDecision: deny` と理由。`Read`、`Edit`、`Write` は `tool_input.file_path` | `PostToolUse` の `additionalContext`。Bash は `tool_input.command` |
| Codex | `~/.codex/config.toml` の `[mcp_servers.<name>]` | `PreToolUse` で `apply_patch` を止められる。パスはパッチの本文にしかない。読むための専用のツールはなく、シェルで読む | `PostToolUse` の `additionalContext`（文だけを足せるかは未確認）。ユーザーが `/hooks` で信頼するまで hook は動かない |
| Cursor | `~/.cursor/mcp.json`（追加・削除の CLI はない） | `~/.cursor/hooks.json` の `preToolUse`（`Read`、`Write`）と `agent_message`。`tool_input` の形は未確認 | `postToolUse` の `additional_context` だけ（許可したときの `agent_message` は届かない） |

## 決定

- `edo harness init [ClaudeCode|Codex|Cursor ...] [--remove]` が、各ハーネスのユーザー全体の設定に、MCP の登録（`edo mcp`）と 2 つの hook を書き込む。ハーネスを省くと、設定ディレクトリがあるものすべてが対象になる。報告はハーネスごとの JSON の配列。
- 書き込むコマンドは、`edo harness init` を実行した edo の絶対パスにする。何度実行しても同じ結果になり、edo が入れたものは中身（edo のパスと `mcp` / `hook <ハーネス>:<イベント>`）で見分けて置き換える。`--remove` はそれを取り除く。書き換える前のファイルは `.bak` に 1 世代残す。
- Claude Code の MCP は `claude` の CLI で登録する。そのほかの設定はファイルを直接書く。プロジェクトの設定には書かない。MCP と hook は常に一緒に入れる。
- hook は `edo hook <ハーネス>:<イベント>` で、2 つだけ。
  - `PreToolUse`: builtin の Read/Edit/Write が edo の対応言語のファイルに触れるなら止め、edo の対応するツールを理由として返す。
  - `PostToolUse`: シェルで対応ファイルを表示した（`cat` などの最初の単語とパス）なら、edo が使えることを事実を述べる形で添える。
- hook の中は「入口 → 判断 → 出口」に分ける。入口はハーネスの入力を `Call { action: Read|Edit|Write|Shell, paths }` に直し、判断はハーネスによらず `Allow | Deny | Advise` を決め、出口はそれをハーネスの返事の形に直す。ハーネスの違い（Codex のパッチ、Cursor のイベント名と入力の形）は入口と出口に閉じ込める。
- hook が自分の不具合で失敗したら、常に通す。出口はそのときも正しい「許可」を返し、原因は stderr に 1 行書く。
- 使い方の説明のファイル（`EDO.md` など）は入れない。MCP のツールの説明と、hook が返す理由で足りる。

## 結果

- 得たもの: 1 つのコマンドで 3 つのハーネスに入り、外せる。PATH を引き継がない GUI のアプリでも edo が見つかる。止める先を edo が扱えるファイルに限るので、エージェントが行き詰まらない。説明のファイルがない分、毎回のセッションのコンテキストが増えない。
- 失ったもの: edo を別の場所に入れ直したら、`edo harness init` をもう一度実行する必要がある。Codex ではシェルで読むことを止められず、実行後に勧めるだけになる。Cursor の入力の形と、Codex の `PostToolUse` で文だけを足せるかは、実物で確かめるまで推測が残る。各ハーネスの仕様が変わると、入口と出口を直す必要がある。Codex では、入れた後にユーザーが `/hooks` で信頼する手間がかかる。
- 採らなかった案:
  - Claude Code のプラグインとして配る案は、Codex と Cursor には使えず、仕組みが 2 つに分かれるので採らなかった。
  - APM（Microsoft の Agent Package Manager）に任せる案は、使う人に APM のインストールが要り、PC ごとに違う edo の絶対パスを書けず、ユーザー全体の hook の書き込みに不具合の実績があるので採らなかった。
  - プロジェクトの設定に入れる案は、コミットされると edo のないチームメンバーの環境で失敗するので採らなかった。
  - コマンド名の `edo` だけを書く案は、ハーネスの PATH に edo がないと動かないので採らなかった。
  - builtin の Edit/Write を権限の設定で全部禁止する案は、edo が扱えないファイルも編集できなくなるので採らなかった。
  - hook の中で `edo read` を実行して結果を返す案は、止める役目と代わりに実行する役目が混ざるので採らなかった。
  - hook の失敗で止める（fail-closed）案は、hook の不具合でエージェントが何もできなくなるので採らなかった。

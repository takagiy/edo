# ハーネスへの組み込みは、edo 自身が MCP の登録と 2 つの hook をユーザー全体の設定に書き込む

## 状況

edo はハーネスの Read/Write/Edit を置き換えるための道具だが、MCP サーバーを登録するだけでは、エージェントは builtin のツールを使い続けることが多い。Claude Code、Codex、Cursor に、edo を使うための設定を 1 つのコマンドで入れたい。rtk は `rtk init` で hook と説明のファイルを入れている。

調べた時点（2026 年 11 月）の各ハーネスの仕様:

| | MCP の登録 | 実行前に止める | 実行後に文を足す |
|---|---|---|---|
| Claude Code | `claude mcp add --scope user`（登録先の `~/.claude.json` は Claude Code が常に書き換える） | `PreToolUse` の `permissionDecision: deny` と理由。`Read`、`Edit`、`Write` は `tool_input.file_path` | `PostToolUse` の `additionalContext`。Bash は `tool_input.command` |
| Codex | `codex mcp add`（登録先は `~/.codex/config.toml` の `[mcp_servers.<name>]`）。hook は `~/.codex/hooks.json` で、Claude Code の `hooks` と同じ形 | `PreToolUse` で `apply_patch` を止められる。パスはパッチの本文にしかない。読むための専用のツールはなく、シェルで読む | `PostToolUse` の `additionalContext`（`permissionDecision` なしで文だけを足せる）。ユーザーが `/hooks` で信頼するまで hook は動かない |
| Cursor | `~/.cursor/mcp.json`（追加・削除の CLI はない） | `~/.cursor/hooks.json` の `preToolUse`（`Read`、`Write`、入力は `tool_input.file_path`）と `agent_message`。hook はグループを作らず `{"matcher","command"}` を並べ、ファイルには `"version": 1` が要る | `postToolUse` の `additional_context`（シェルは `Shell`、`tool_input.command`）。許可のときも正しい JSON を返さないと操作が止まる。既定で `~/.claude/settings.json` の Claude Code の hook も実行する（設定「Include Third-Party Plugins, Skills, and Other Configs」） |

## 決定

- `edo harness init [ClaudeCode|Codex|Cursor ...] [--remove]` が、各ハーネスのユーザー全体の設定に、MCP の登録（`edo mcp`）と 2 つの hook を書き込む。ハーネスを省くと、設定ディレクトリがあるものすべてが対象になる。報告はハーネスごとの配列。
- 書き込むコマンドは、`edo harness init` を実行した edo の絶対パスにする。何度実行しても同じ結果になり、edo が入れたものは中身（edo のパスと `mcp` / `hook <ハーネス>:<イベント>`）で見分けて置き換える。`--remove` はそれを取り除く。書き換える前のファイルは `.bak` に 1 世代残す。
- hook のコマンドは、パスを引用符で囲まずに書く。Codex は Windows で hook を PowerShell で実行し、PowerShell では引用符で囲んだパスの後に引数を続けると構文エラーになるため。どのシェルでも動く引用の仕方はないので、空白を含むパスの edo では入れずにエラーにする。
- MCP は各ハーネスの CLI（`claude mcp`、`codex mcp`）で登録し、hook は JSON のファイルを直接書く。Claude Code と Codex は hook のファイルの形が同じなので、ハーネスごとの違いは CLI、ファイル名、matcher の表に収まる。プロジェクトの設定には書かない。MCP と hook は常に一緒に入れる。
- Cursor には MCP を追加・削除する CLI がないので、`~/.cursor/mcp.json` の `mcpServers.edo` を直接書く。Cursor は Claude Code の hook も実行するため、両方に edo を入れると Cursor では edo の hook が 2 回動く。止める理由と勧める文が 2 回届くだけで害はないので、重複は許す（Claude Code の hook の側で Cursor からの呼び出しを見分けて黙る案は、Claude Code 用の入口が Cursor の事情を知る特別扱いになるので採らなかった）。
- hook は `edo hook <ハーネス>:<イベント>` で、2 つだけ。
  - `PreToolUse`: builtin の Read/Edit/Write が edo の対応言語のファイルに触れるなら止め、edo の対応するツールを理由として返す。
  - `PostToolUse`: シェルで対応ファイルを表示した（`cat` などの最初の単語とパス）なら、edo が使えることを事実を述べる形で添える。
- hook の中は「入口 → 判断 → 出口」に分ける。入口はハーネスの入力を `Call { action: Read|Edit|Write|Shell, paths }` に直し、判断はハーネスによらず `Allow | Deny | Advise` を決め、出口はそれをハーネスの返事の形に直す。ハーネスの違い（Codex のパッチ、Cursor のイベント名と入力の形）は入口と出口に閉じ込める。
- hook が自分の不具合で失敗したら、常に通す。出口はそのときも正しい「許可」を返し、原因は stderr に 1 行書く。
- 使い方の説明のファイル（`EDO.md` など）は入れない。MCP のツールの説明と、hook が返す理由で足りる。

## 結果

- 得たもの: 1 つのコマンドで 3 つのハーネスに入り、外せる。PATH を引き継がない GUI のアプリでも edo が見つかる。止める先を edo が扱えるファイルに限るので、エージェントが行き詰まらない。説明のファイルがない分、毎回のセッションのコンテキストが増えない。
- 失ったもの: edo を別の場所に入れ直したら、`edo harness init` をもう一度実行する必要がある。Codex ではシェルで読むことを止められず、実行後に勧めるだけになる。各ハーネスの仕様が変わると、入口と出口を直す必要がある。Codex では、入れた後にユーザーが `/hooks` で信頼する手間がかかる。
- 採らなかった案:
  - Claude Code のプラグインとして配る案は、Codex と Cursor には使えず、仕組みが 2 つに分かれるので採らなかった。
  - APM（Microsoft の Agent Package Manager）に任せる案は、使う人に APM のインストールが要り、PC ごとに違う edo の絶対パスを書けず、ユーザー全体の hook の書き込みに不具合の実績があるので採らなかった。
  - プロジェクトの設定に入れる案は、コミットされると edo のないチームメンバーの環境で失敗するので採らなかった。
  - コマンド名の `edo` だけを書く案は、ハーネスの PATH に edo がないと動かないので採らなかった。
  - builtin の Edit/Write を権限の設定で全部禁止する案は、edo が扱えないファイルも編集できなくなるので採らなかった。
  - hook の中で `edo read` を実行して結果を返す案は、止める役目と代わりに実行する役目が混ざるので採らなかった。
  - hook の失敗で止める（fail-closed）案は、hook の不具合でエージェントが何もできなくなるので採らなかった。

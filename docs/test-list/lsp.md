# LSP の診断

言語パッケージの `[package.lsp]`、MCP の write の報告。

## 言語パッケージ

- [x] `[package.lsp]` の command、root_markers、language_id を読む
- [x] language_id を省くと package.name になる
- [x] `distribution` の type が mise で mise があれば、`mise exec <tools> -- <command>` で起動する
- [x] mise がなければ、command をそのまま起動する
- [x] ルートは、root_markers のどれかがある、ファイルにいちばん近い親ディレクトリになる
- [x] root_markers が見つからなければ、ファイルのあるディレクトリがルートになる

## 書き込みの報告に付ける LSP の警告

- [x] 書き込みで増えた LSP のエラーが `warnings.new_lsp_errors` に line と message で入る
- [x] 前からあった LSP のエラーは報告されない
- [x] エラーでない診断は報告されない
- [x] 診断が届かなければ `warnings.lsp` が starting になる
- [x] サーバーが終了していれば `warnings.lsp` が stopped になり、次の書き込みで起動し直すようにサーバーを捨てる
- [x] 書く前の文書の診断が届かなければ、書いた後の文書は問い合わせない
- [x] `[package.lsp]` のない言語のファイルには、LSP の報告は何も付かない

## 一覧

- [x] `edo lang list` に、各言語の LSP のコマンドと、それが見つかるかどうかが出る

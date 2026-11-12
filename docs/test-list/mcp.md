# MCP

- [x] ツールの一覧は read と write を返す
- [x] read ツールは path と scope を受け取り、テキストを返す
- [x] write ツールは CLI と同じ MISON の報告を返す
- [x] 失敗した呼び出しの結果は isError が立つ
- [x] path には Windows の絶対パスを渡せる
- [x] 最後の read の後に変更されたファイルを行範囲で書くと、stale の警告が付く
- [x] stale の警告は、read と write でパスの書き方が違っても付く
- [x] edo 自身の write の後に行範囲で書いても、stale の警告は付かない
- [x] 識別子のスコープで書くときは、stale の警告は付かない
- [x] write ツールの `mode` で一致の後ろに挿入できる
- [x] read ツールは path と scope に加えて reads の要素も読み、path の分、reads の分の順に並べる
- [x] write ツールは path の書き換えに加えて edits の要素も書き、replaced は path の分、edits の分の順に並ぶ
- [x] glob で読んだ後に外で書き換えられたファイルに、行範囲のスコープで書くと、stale の警告が付く

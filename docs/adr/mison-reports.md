# エージェントへの報告は MISON で書く

## 状況

edo の CLI と MCP は、write の結果やエラーを JSON で返していた。報告を読むのはエージェントで、JSON の引用符や括弧はそのままトークンを使う。同じデータを短く書ける [MISON](https://github.com/takagiy/mison) を作り、crate として公開した。

## 決定

- エージェントに返す報告は、すべて MISON で書く。対象は CLI の stdout（write、lang、harness init、エラー）、MCP のツールの結果、glob の read が最後に付ける飛ばした拡張子ごとの件数の行。
- 報告のデータは今までどおり `serde_json::Value` で組み立て、書き出す直前に `report`（`src/lib.rs`）で MISON にする。
- mison 0.2 は配列の区切りを型の高さで決めるが、`Value` には型がない。そこで `report` は、各配列の高さを要素の入れ子の深さから求めて `mison::Writer` に渡す。
- 行範囲は `"2-5"` のように `scope` と同じ書き方にする。MISON の範囲の書き方とも同じなので、エージェントは報告の範囲をそのまま `scope` に渡せる。
- hook の答えは JSON のまま残す。読むのはハーネスで、ハーネスが JSON を求めるため。

報告は次のようになる。複数の報告は 1 行に 1 つずつ並ぶ。

```
ok=false file=a.rs error=ambiguous matches=(range=2-2 parents="fn a()" signature="x();"),(range=5-5 parents="fn b()" signature="x();")
ok=true file=a.rs replaced=2-2,5-5 warnings=(new_syntax_errors=(line=2 text="y(;"),(line=5 text="y(;"))
```

## 結果

- 得たもの: 報告が短くなり、エージェントのコンテキストを使う量が減る。上の ambiguous の報告は、JSON の 172 文字から 134 文字になった。
- 失ったもの: 報告を JSON として読んでいたスクリプト（`edo write ... | jq` など）は読めなくなる。行範囲は数の組でなく文字列になる。

## 採らなかった案

- `--format json` のような切り替えを残す: エージェント以外に報告を読む使い手がまだいない。
- 報告ごとに型を定義する: `Value` を書く数十行のアダプタで足りるので、今の報告には要らない。
- 行範囲を `[2, 5]` のままにする: MISON では `(2,5)` になり、`scope` にそのまま渡せない。

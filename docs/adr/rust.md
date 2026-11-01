# 実装言語に Rust を使う

## 状況

edo はエージェントが何度も呼ぶ CLI であり、MCP サーバーも兼ねる。tree-sitter と各言語の grammar を使い、外部の grammar も実行時に読み込みたい。主な利用環境に Windows が含まれる。

## 決定

Rust で実装し、単一のバイナリとして配布する。MCP には `rmcp`、外部 grammar の読み込みには `tree-sitter` の `wasm` feature を使う。

## 結果

- 得たもの: 起動が速い。ランタイムが要らない。tree-sitter の公式バインディングと grammar crate がそのまま使える。
- 失ったもの: wasmtime を入れるのでビルド時間とバイナリサイズが増える。MCP のために非同期ランタイム（tokio）が入る。

## 採らなかった案

- TypeScript: MCP SDK は一番成熟しているが、`node-tree-sitter` のネイティブビルドが Windows で詰まりやすく、`web-tree-sitter` は起動が遅い。
- Go: go-tree-sitter が cgo 必須で、Windows のクロスビルドが面倒。
- Python: 配布と起動速度の面で CLI に向かない。


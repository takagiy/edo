# 言語

- [x] 組み込み言語は宣言した拡張子で選ばれる
- [x] どの言語も宣言していない拡張子では言語が選ばれない
- [x] パッケージのマニフェストから名前と拡張子を読む
- [x] `syntax.module = "builtin:<name>"` は組み込みの grammar を指す
- [x] `syntax.module` にファイル名を書くと WASM の grammar を読み込む
- [x] `syntax.entrypoint` を書くと WASM の中のその関数から言語を読み込む
- [x] `tree_sitter_` で始まらない `syntax.entrypoint` はパッケージの読み込みエラーになる
- [x] インストール済みのパッケージが組み込みと同じ拡張子を宣言していれば、インストール済みが選ばれる
- [x] 文法の誤った query はパッケージの読み込みエラーになる
- [x] 組み込みの Kotlin はアノテーション付きの data object をパースできる
- [x] 組み込みの Rust は、型引数付きの型の impl を型引数を除いた名前にする
- [x] 組み込みの Rust、TypeScript、TSX、JavaScript、Python、Go には LSP サーバーの設定がある
- [x] 組み込みの TSX の LSP の language_id は typescriptreact になる

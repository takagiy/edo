# ビルドの道具を mise で揃え、全 OS で cargo-zigbuild を通す

## 状況

edo のビルドには、Rust のツールチェーンのほかに、tree-sitter の grammar の C をコンパイルする C コンパイラと、wasmtime-c-api-impl のビルドスクリプトが呼ぶ cmake が要る。Windows の gnu 向けには、さらに `windows-sys` のリンクに `dlltool.exe` が要る。これまでは、たまたま入っていた WinLibs（gcc、cmake、dlltool を同梱）に頼っていた。

rustup の既定のホストは PC ごとに違い、この PC では msvc だが MSVC のリンカーは動かない。

cc クレートは Rust のターゲット名（`x86_64-pc-windows-gnu`）を `--target` として C コンパイラに渡すが、`zig cc` はこの書式を受け付けない。cargo-zigbuild はこれと dlltool を吸収するラッパーを作るが、働くのは `--target` を付けたときだけ。

## 決定

`mise.toml` で Rust、cmake、zig、cargo-zigbuild を揃え、ビルドするタスク（build / test / lint）はすべての OS で `cargo-zigbuild … --target {{vars.target}}` を通す。`vars.target` はホストの OS と CPU から決まるターゲット名で、OS による違いはこの値だけに置く。

release タスクは、`vars.release_targets` に並べた 6 つのターゲット（Windows / macOS / Linux の x86_64 と aarch64）を同じ cargo-zigbuild でクロスビルドし、`crates/xtask` が `target/edo-<ターゲット>.tar.gz` を作る。tarball を `tar` コマンドで作らないのは、Windows のファイルシステムに実行ビットがなく、bsdtar には権限を指定するオプションもないため。xtask は権限を 0755 と明示して書き込む。

Rust は、Windows だけ `1.99-x86_64-pc-windows-gnu` に固定する。ビルドスクリプトはホスト向けにコンパイルされるので、ホストが msvc だと MSVC のリンカーが要るため。

## 結果

- 得たもの: `mise install` だけで、Windows でも MinGW や Visual Studio なしでビルドとテストが通る。タスクに OS ごとの分岐がない。どの OS からでも、6 つのプラットフォーム向けのリリースを作れる。
- 失ったもの: Linux と macOS でも C とリンクが zig 経由に変わり、この変更は Windows でしか確かめていない。リリースのバイナリも、Windows x86_64 版しか実際に起動して確かめていない。macOS では、フレームワークを使う依存があると SDK が要る可能性がある。ターゲットを明示するので、ビルド結果は `target/<ターゲット名>/` に出る。mise を通さない素の `cargo` は、Windows ではこれまでどおり MinGW を要する。

## 採らなかった案

- Windows のときだけ cargo-zigbuild を通す（`run_windows`）: 動くが、タスクごとに Windows 用の行が要る。
- 環境の側を完全にする（cc に zig 用のラッパーを、rustc に `-C dlltool` を渡す）: 素の `cargo` まで全 OS で動くが、ターゲット名を書き換えるラッパーを自前で保守することになり、cargo-zigbuild の一部を作り直すことになる。
- Rust を mise で管理しない: PC ごとの rustup の設定に左右され、Windows で MinGW が別に要る。
- C コンパイラに llvm-mingw を入れる: zig と同じ役割だが、dlltool とターゲット名の問題は残る。

{
  description = "trader2 - bitFlyer BTC/JPY Automated Trading Bot";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
        };
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = [
            rustToolchain

            # SQLite (rusqlite bundled feature があるので任意だが開発ツール用に追加)
            pkgs.sqlite

            # Ollama (News AI フェーズで使用)
            pkgs.ollama

            # 開発ユーティリティ
            pkgs.cargo-watch   # ファイル変更時に自動ビルド/テスト
            pkgs.cargo-nextest # 高速テストランナー
            pkgs.cargo-tarpaulin # カバレッジ計測

            # 基本ツール
            pkgs.pkg-config
            pkgs.openssl
            pkgs.cacert
          ];

          # openssl-sys クレートが参照する環境変数
          PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
          OPENSSL_NO_VENDOR = "1";

          # Ollama モデル設定
          OLLAMA_MODEL = "gemma4:latest";

          shellHook = ''
            echo "trader2 dev environment ready"
            echo "  rust: $(rustc --version)"
            echo "  cargo: $(cargo --version)"

            # .env が存在すればロード（direnv 経由でも読まれるが念のため）
            if [ -f .env ]; then
              set -a
              source .env
              set +a
            fi
          '';
        };
      });
}

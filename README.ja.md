# Angelic Angel

> 「Angelic Angel/Hello,星を数えて」は、2015年7月1日に Lantis から発売された μ's によるシングルで、楽曲は劇場版『ラブライブ！The School Idol Movie』の挿入歌。
>
> — [Wikipedia](https://ja.wikipedia.org/wiki/Angelic_Angel/Hello,%E6%98%9F%E3%82%92%E6%95%B0%E3%81%88%E3%81%A6)

Twitter/X の通知を Mozilla の Web Push 基盤を通じてリアルタイムに受信する CLI ツールです。自分がフォローしていて、ツイート通知を有効化しているユーザのツイートをストリーミングできます。

[English README](README.md)

## 概要

Angelic Angel はブラウザの Web Push クライアントをエミュレートし、Twitter/X のプッシュ通知を受信します。[Mozilla AutoPush](https://autopush.readthedocs.io/) に WebSocket で接続し、ECE (Encrypted Content-Encoding) で暗号化された通知を復号して、設定された Webhook エンドポイントに転送します。

フォロー中かつ **ツイート通知をオン** にしているユーザのツイートがリアルタイムで届きます。

### 仕組み

```
Twitter/X  ──push──▶  Mozilla AutoPush サーバ  ◀──WebSocket──  Angelic Angel  ──HTTP POST──▶  Webhook
```

1. Angelic Angel が Mozilla AutoPush サーバに Web Push サブスクライバとして登録します。
2. 取得したプッシュサブスクリプションのエンドポイントを Twitter の通知設定 API に登録します。
3. Twitter がプッシュ通知を送信すると、Firefox が使用するものと同じ Mozilla AutoPush サーバを経由して届きます。
4. Angelic Angel が WebSocket 経由で通知を受信・復号し、Webhook にペイロードを転送します。

### 重要事項

- **データの取得元**: すべての通知データは Mozilla の Web Push サーバ (`push.services.mozilla.com`) から受信しています。通知データの取得のために Twitter/X に直接アクセスすることはありません。
- **API の使用は最小限**: Twitter/X の API はプッシュサブスクリプションの初回登録時 (`register` コマンド) にのみ使用されます。通知の受信中に API コールは発生しません。
- **スクレイピング不使用**: このツールは Web スクレイピングを一切行いません。ブラウザがプッシュ通知を配信するのと同じ、標準的な W3C Push API のフローを利用しています。

## 必要環境

- Rust (edition 2024)
- Twitter/X アカウントの認証情報 (`auth_token` と `ct0` Cookie)

### `auth_token` と `ct0` の取得方法

1. Web ブラウザで [x.com](https://x.com) を開いてログインします。
2. 開発者ツール (F12) を開き、**Application** (または **ストレージ**) タブを選択します。
3. **Cookie** → `https://x.com` から `auth_token` と `ct0` の値を確認できます。

## インストール

```sh
cargo install --path .
```

## 使い方

### 1. 設定の初期化

```sh
# 対話モード
angelic-angel init

# 引数を指定する場合
angelic-angel init --auth-token YOUR_AUTH_TOKEN --ct0 YOUR_CT0
```

Twitter の認証情報を含む `angelic-angel.toml` が作成されます。

### 2. プッシュサブスクリプションの登録

```sh
angelic-angel register
```

Mozilla AutoPush に新しいプッシュサブスクリプションを登録し、そのエンドポイントを Twitter のプッシュ通知 API に登録します。

### 3. 通知の受信開始

```sh
WEBHOOK_ENDPOINT=https://your-webhook.example.com/endpoint angelic-angel listen
```

`WEBHOOK_ENDPOINT` 環境変数で、復号された通知ペイロードの HTTP POST 送信先を指定します。

### その他のコマンド

```sh
# 現在の設定と登録状態を確認
angelic-angel status

# プッシュサブスクリプションを解除
angelic-angel unregister
```

### オプション

| フラグ | 説明 |
|--------|------|
| `-c, --config <PATH>` | 設定ファイルのパス (デフォルト: `angelic-angel.toml`) |
| `-v, --verbose` | デバッグログを有効化 |

## Docker

Docker Compose 2.24 以上が必要です。

```sh
docker compose build
cp .env.example .env   # 編集して実際の Webhook URL を設定

# 認証情報は対話入力（シェル履歴に秘密情報を残さないため）
docker compose run --rm -it angelic-angel init
docker compose run --rm angelic-angel register
docker compose run --rm angelic-angel status

docker compose up -d
docker compose logs -f
docker compose stop
```

- コンテナは `/data/angelic-angel.toml` に認証情報と登録情報を保存します。名前付きボリュームは `docker compose down` では保持され、`docker compose down -v` で削除されます。
- 通知の転送には `WEBHOOK_ENDPOINT` が必要ですが、`init` と `status` に `.env` は不要です。
- コンテナ内の `localhost` はコンテナ自身を指します。ホスト上の Webhook には `http://host.docker.internal:8080/endpoint` のような URL を使ってください。Linux では、ホスト側のサービスをブリッジ IP や `0.0.0.0` など、Docker から到達できるアドレスで待ち受ける必要があります。
- `.env` を編集したら `docker compose up -d` でコンテナを再作成してください。`restart` だけでは新しい値は反映されません。
- デフォルトのコマンドは `listen` です。他のコマンドで置き換えられます（例: `docker compose run --rm angelic-angel unregister`）。
- `init` と `register` の前に `up` を実行しないでください。失敗して再起動ポリシーによりループします。

## 開発

Rust は 1.90.0 に固定されています（`rust-toolchain.toml`、CI、Docker、Dev Container）。

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features   # 現在テストは 0 件
docker build -t angelic-angel:local .
scripts/docker-smoke.sh angelic-angel:local        # オフライン・ダミー認証情報のみ
```

ランタイムは strip 済みバイナリを載せた Distroless で、シェル・ソース・ビルドキャッシュを含みません。`scripts/docker-smoke.sh` はダミーの認証情報とネットワーク無効化を使い、設定の永続化、権限、エラー時の終了コード、SIGINT による停止を検証します。必要に応じて別の BusyBox ヘルパーを pull し、作成したテスト用リソースを削除します。

イメージのデバッグには、シェルへの `exec` ではなく `docker logs` と CLI サブコマンド（例: `docker run --rm IMAGE status`）を使ってください。公開イメージは GHCR から取得できます: `docker pull ghcr.io/<owner>/<repo>:latest`。

### Dev Container

`.devcontainer/` は Ubuntu 24.04 ベースの Rust 開発環境（GitHub CLI、Claude Code、Codex CLI、Bun、Docker outside of Docker、act、direnv）を提供します。開く前に、バインドマウントされるホストのディレクトリを作成してください: `mkdir -p ~/.claude ~/.codex`。ビルド成果物と Cargo キャッシュは名前付きボリュームに保存されます。ホストの `gh` はマウントされないため、必要ならコンテナ内で `gh auth login` を実行してください。ホストの AWS/SSH 設定もマウントされません。

初回作成時に、`.codex/config.toml`（Git 管理外）へ参照元の Codex ゲートウェイ設定テンプレートが作成されます。接続先は `https://ai.qleap.jp/v1`、認証には `OPENAI_API_KEY` を使います。既存のファイルは上書きされません。接続先を変更する場合はこの設定を編集してください。

### CI/CD

- `.github/workflows/integration.yaml` はプルリクエスト、`main` 以外のブランチへの push、および再利用ワークフローとして実行され、commitlint（新規コミット範囲）、rustfmt、Clippy（警告はエラー）、`cargo test --locked`、Docker ビルドと `scripts/docker-smoke.sh` を行います。権限は `contents: read` です。
- `.github/workflows/deployment.yaml` は `main` への push、`v*` タグ、手動実行（`main` のみ）で動作し、先に integration を呼び出してから、マルチアーキテクチャ（`linux/amd64`、`linux/arm64`）イメージを `ghcr.io/<owner>/<repo>` へ公開します。タグは `sha-<short>`、`main` では `latest`、`main` の履歴上にあり `Cargo.toml` と一致する `v<version>` タグのときのみ `<version>` です。`packages: write` は公開ジョブだけが持ちます。
- コミットメッセージは Conventional Commits に従います（`.commitlintrc.yaml`）。

## 再接続

Angelic Angel は Firefox 互換の再接続戦略を実装しています:

- 指数バックオフ: 5秒 × 2^n (上限 5 分)
- UAID 無効化時の自動再登録
- サーババックオフ (close code 4774): 30 分間の待機
- 接続成功時にリトライカウンタをリセットする無限リトライ

## ライセンス

MIT

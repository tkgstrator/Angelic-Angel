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
- **API の使用は最小限**: Twitter/X の API はプッシュサブスクリプションの初回登録時 (`register` コマンド) と、WebUI の一括通知設定を明示的に開始したときにのみ使用されます。通知の受信中に API コールは発生しません。
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

### WebUI の操作

WebUI（`angelic-angel serve`）は「通知」「設定」「受信状況」の 3 画面です。Cookie と Webhook の設定のほかに、次の操作があります。

- **Cookie の確認**（「設定」画面）: 「確認」ボタンと、保存のたびに実行される自動確認で、*保存済み*の Cookie が X でまだ使えるかを問い合わせ、アカウント名を表示します。ネットワークエラー・レート制限・X 側の障害などで確認できなかった場合は「確認不可」と表示し、Cookie が無効とは判定しません。入力中で未保存の値は確認の対象ではないため、先に保存してください。
- **受信先の登録**（「設定」画面、Cookie の下）: Web Push の登録と解除を行います。登録中は Cookie を変更できません。
- **フォロー中の全員の投稿通知をオン**（「設定」画面、登録の下）: インラインの確認の後、X でフォロー中のすべてのアカウントの投稿通知をオンにします。対象全員の X 側の設定が変更され、途中で中止しても変更済みのアカウントは元に戻りません。ジョブは同時に 1 つだけ実行され、進捗（対象・成功・失敗・残り）を表示します。失敗や一部のみ成功した結果もそのまま表示し、自動では再試行しません。再実行には改めて確認が必要です。保存済みの Cookie が必要ですが、Web Push の登録は不要です。
- **通知**（「通知」画面）: 受信した通知を新しい順に、アイコン・本文・投稿へのリンク・元データとともに一覧表示します。受信した通知はすべて保存され、件数の上限や自動削除はありません。保存先はローカルの SQLite データベース `<設定ファイル>.notifications.sqlite3`（例: `angelic-angel.toml.notifications.sqlite3`）で、権限 `0600` で作成されます。受信したペイロードを含むため設定ファイルと同様に扱ってください。古い通知は「もっと見る」ボタンで読み込みます。

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

Rust は 1.99.0 に固定されています（`rust-toolchain.toml`、CI、Docker、Dev Container）。

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features   # 現在テストは 0 件
docker build -t angelic-angel:local .
scripts/docker-smoke.sh angelic-angel:local        # オフライン・ダミー認証情報のみ
```

ランタイムは strip 済みバイナリを載せた Distroless で、シェル・ソース・ビルドキャッシュを含みません。`scripts/docker-smoke.sh` はダミーの認証情報とネットワーク無効化を使い、設定の永続化、権限、エラー時の終了コード、SIGINT による停止を検証します。必要に応じて別の BusyBox ヘルパーを pull し、作成したテスト用リソースを削除します。

イメージのデバッグには、シェルへの `exec` ではなく `docker logs` と CLI サブコマンド（例: `docker run --rm IMAGE status`）を使ってください。公開イメージは GHCR から取得できます: `docker pull ghcr.io/<owner>/<repo>:latest`。

### WebUI の開発

リポジトリのルートで `bun dev` を実行すると、受信機をビルド（`cargo build --locked`）してから、2 つの子プロセスを起動します。`angelic-angel serve`（API、`127.0.0.1:8080`）と、`web/` の Vite 開発サーバー（`http://localhost:11475/`。`/api` と `/health` を API へプロキシします）です。フロントエンドの依存関係は事前に一度 `bun install --cwd web --frozen-lockfile` で導入してください。

- リポジトリルートの `angelic-angel.toml` を使います。その設定に登録情報があると `serve` は実際の受信機を起動するため、UI を試すだけなら `DEV_CONFIG` で一時ファイルを指定してください（例: `DEV_CONFIG=.cache/dev/angelic-angel.toml bun dev`）。使用中の設定ファイルは起動時に表示されます。
- WebUI はローカル専用で、認証やログインは不要です。開くとそのまま 3 つの画面が表示されます。`.lock` ファイルは設定ファイルと同じ場所に作られ、Git 管理外です。
- ファイル監視や自動再起動はしません。フロントエンドの編集はホットリロードされますが、Rust の変更には `bun dev` の再起動が必要です。どちらかの子プロセスが終了すると、もう一方も停止します。`Ctrl+C`（SIGINT/SIGTERM）は自分が起動したプロセスだけを停止して終了を待ち、2 回目の `Ctrl+C` で強制終了します。
- Vite と API はどちらも `127.0.0.1` でのみ待ち受けます。WebUI に認証はないため、これらのポートを他のホストやネットワークへ公開しないでください。
- プロキシはブラウザの `Host` と `Origin` をそのまま保ちます（`changeOrigin: false`）。そのため `PUT`/`POST`/`DELETE` に対する API の同一オリジン検査は、Vite 自身が配信したページからのリクエストでのみ通ります。表示された URL で UI を開き、別のオリジン経由では使わないでください。
- ポートは `DEV_API_PORT` と `DEV_WEB_PORT` で変更できます（既定は `8080` と `11475`。Vite のポートは厳密指定で、他のポートへフォールバックしません）。`cd web && bun run dev` は、自分で起動した API に対して Vite だけを動かします（プロキシ先は `VITE_API_TARGET` で変更できます）。

### Dev Container

`.devcontainer/` は Ubuntu 24.04 ベースの Rust 開発環境（GitHub CLI、Claude Code、Codex CLI、Bun、Docker outside of Docker、act、direnv）を提供します。開く前に、バインドマウントされるホストのディレクトリを作成してください: `mkdir -p ~/.claude ~/.codex`。ビルド成果物と Cargo キャッシュは名前付きボリュームに保存されます。ホストの `gh` はマウントされないため、必要ならコンテナ内で `gh auth login` を実行してください。ホストの AWS/SSH 設定もマウントされません。

`CODEX_HOME` はバインドマウントされたホストの `~/.codex`（コンテナ内では `/home/vscode/.codex`）を指すため、Codex の履歴、セッション、認証、スキル、プロジェクトのトラストはホストと共有され、コンテナを作り直しても残ります。`model_provider` と `[model_providers.*]` のゲートウェイ定義（API キーは `env_key = "OPENAI_API_KEY"` のように環境変数名で参照します）はホストの `~/.codex/config.toml` に置いてください。Dev Container はこれらを書き込みません。

初回作成時に、`.codex/config.toml`（Git 管理外）へプロジェクト単位の上書きだけが作成されます: `model = "gpt-6.1-sol"` と `model_reasoning_effort = "xhigh"` です。既存のファイルは上書きされません。モデルを変えるときはこのファイルを編集または削除してください。

### CI/CD

- `.github/workflows/integration.yaml` はプルリクエスト、`master` 以外のブランチへの push、および再利用ワークフローとして実行され、commitlint（新規コミット範囲）、rustfmt、Clippy（警告はエラー）、`cargo test --locked`、Docker ビルドと `scripts/docker-smoke.sh` を行います。権限は `contents: read` です。
- `.github/workflows/deployment.yaml` は `master` への push、`v*` タグ、手動実行（`master` のみ）で動作し、先に integration を呼び出してから、マルチアーキテクチャ（`linux/amd64`、`linux/arm64`）イメージを `ghcr.io/<owner>/<repo>` へ公開します。タグは `sha-<short>`、`master` では `latest`、`master` の履歴上にあり `Cargo.toml` と一致する `v<version>` タグのときのみ `<version>` です。`packages: write` は公開ジョブだけが持ちます。
- コミットメッセージは Conventional Commits に従います（`.commitlintrc.yaml`）。

## 再接続

Angelic Angel は Firefox 互換の再接続戦略を実装しています:

- 指数バックオフ: 5秒 × 2^n (上限 5 分)
- UAID 無効化時の自動再登録
- サーババックオフ (close code 4774): 30 分間の待機
- 接続成功時にリトライカウンタをリセットする無限リトライ

## ライセンス

MIT

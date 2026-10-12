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
- **API の使用は最小限**: Twitter/X の API はプッシュサブスクリプションの初回登録時 (`register` コマンド、または認証情報を環境変数で渡した最初の `listen` による自動登録) と、WebUI の一括通知設定を明示的に開始したときにのみ使用されます。通知の受信中に API コールは発生しません。
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

手順 1 と 2 を省略することもできます。`TWITTER_AUTH_TOKEN` と `TWITTER_CSRF_TOKEN`（`auth_token` と `ct0` の Cookie）を設定しておくと、登録がまだ無いときは最初の `listen` が登録まで行います。値は環境変数から読み取られ、設定ファイルには書き込まれません。詳しくは [Docker](#docker) を参照してください。

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
| `-c, --config <PATH>` | 設定ファイルのパス (環境変数: `ANGELIC_ANGEL_CONFIG`、デフォルト: `angelic-angel.toml`) |
| `-v, --verbose` | デバッグログを有効化 |

## Docker

Docker Compose 2.24 以上が必要です。コンテナに必要なのは、環境変数で渡す X の認証情報と Webhook だけです。初回起動時に自分で登録を済ませ、登録情報をマウントしたディレクトリに保存します。

```sh
cp .env.example .env   # 編集して TWITTER_AUTH_TOKEN、TWITTER_CSRF_TOKEN、Webhook を設定

docker compose up -d --build
docker compose logs -f
docker compose stop
```

- `TWITTER_AUTH_TOKEN` と `TWITTER_CSRF_TOKEN` は、`auth_token` と `ct0` の Cookie の値です（[`auth_token` と `ct0` の取得方法](#auth_token-と-ct0-の取得方法)）。
- `./config` を誰が作っても（自分で作っても、無ければ Docker が作っても）、`chown` も `user:` も不要です。コンテナは root で起動し、設定ファイルを開く前に、既存の設定ファイルの所有者の UID/GID へ切り替えます。ファイルが無い、または root 所有のときは `/config` ディレクトリの所有者へ、ディレクトリも root 所有なら 65532:65532 へ切り替えます。ディレクトリが root 所有のときは、そのディレクトリと、中にある root 所有の `.lock`・履歴データベース・`.tmp-*`（設定ファイルが root 所有ならそれも）を、切り替え先の所有に変えます。以前 `user: "1000:1000"` で動かして作られた `./config` が、いまは root 所有のディレクトリになっていても、そのまま動きます。ディレクトリが設定ファイルとは別の非 root ユーザーの所有のときは、設定ファイルの所有者に合わせてディレクトリは変更しないため、そのユーザーが書き込めないと "cannot open lock file" で失敗することがあります。結果は最初のログ行（`running as 65532:65532 owning /config`）に出力され、以降のプロセスは root ではありません。標準の `CHOWN`、`SETUID`、`SETGID` ケーパビリティが必要です。
- `user:`（または `docker run --user`）で root 以外を明示した場合は、切り替えも `chown` も行いません。その UID が `./config` に書き込める必要があり、書き込めないと "cannot open lock file ... Permission denied" で起動に失敗します。`./config` に自分で置いたファイルは、切り替え先のユーザーが読める状態でなければなりません。root 所有のファイルは、ディレクトリも root 所有のときだけ引き渡されます。
- ファイルではなくディレクトリをマウントしてください。設定は一時ファイルに書いてからリネームして保存され、ロックと履歴データベースも同じディレクトリに置かれます。
- コンテナ内の `localhost` はコンテナ自身を指します。ホスト上の Webhook には `http://host.docker.internal:8080/endpoint` のような URL を使ってください。Linux では、ホスト側のサービスをブリッジ IP や `0.0.0.0` など、Docker から到達できるアドレスで待ち受ける必要があります。
- `.env` を編集したら `docker compose up -d` でコンテナを再作成してください。`restart` だけでは新しい値は反映されません。
- デフォルトのコマンドは `listen` です。他のコマンドで置き換えられます（例: `docker compose run --rm angelic-angel status`）。設定は同時に 1 つのプロセスしか使えないため、先に動作中のコンテナを止めてください（`docker compose stop`）。

### 配布イメージを使う

ビルドせずに GHCR のイメージを使えます。環境変数は、`compose.yaml` と同じディレクトリの `.env` から補間されます。

```yaml
services:
  angelic-angel:
    image: ghcr.io/tkgstrator/angelic-angel:latest
    restart: unless-stopped
    stop_signal: SIGINT
    environment:
      TWITTER_AUTH_TOKEN: ${TWITTER_AUTH_TOKEN}
      TWITTER_CSRF_TOKEN: ${TWITTER_CSRF_TOKEN}
      WEBHOOK_ENDPOINT: ${WEBHOOK_ENDPOINT}
      WEBHOOK_SECRET_KEY: ${WEBHOOK_SECRET_KEY}
    extra_hosts:
      - "host.docker.internal:host-gateway"
    volumes:
      - ./config:/config
```

```sh
# .env（コミットしないこと）
TWITTER_AUTH_TOKEN=your-auth_token-cookie-value
TWITTER_CSRF_TOKEN=your-ct0-cookie-value
WEBHOOK_ENDPOINT=https://your-webhook.example.com/endpoint
WEBHOOK_SECRET_KEY=your-shared-secret
```

- 環境変数での認証情報の指定と自動登録には、これらを含むリリース以降のイメージが必要です。それ以前のリリース（v0.1.1 まで）には含まれていません。`latest` が古いイメージのままのときは、`docker compose pull` で最新を取得してください。
- 再現性が必要な運用では、`latest` ではなくバージョンタグ（`X.Y.Z` の形式。例: `ghcr.io/tkgstrator/angelic-angel:0.2.0`）に固定できます。

### 環境変数

| 変数 | 説明 |
|------|------|
| `TWITTER_AUTH_TOKEN` | `auth_token` Cookie。`TWITTER_CSRF_TOKEN` と一緒に設定します |
| `TWITTER_CSRF_TOKEN` | `ct0` Cookie。`TWITTER_AUTH_TOKEN` と一緒に設定します |
| `WEBHOOK_ENDPOINT` | Webhook の URL。空白でない値は設定ファイルより優先されます |
| `WEBHOOK_SECRET_KEY` | Webhook リクエストの署名に使う共有シークレット。Webhook URL を設定する場合は必須です |
| `ANGELIC_ANGEL_CONFIG` | 設定ファイルのパス。イメージは `/config/angelic-angel.toml` を設定済みで、`--config` が優先されます |

- 認証情報の 2 つは対です。両方が設定されている（空白でない）ときは設定ファイルの `[twitter]` の代わりに使われ、両方とも無いときは設定ファイルが使われます。**片方だけが設定されているとエラーで失敗します**（設定ファイルの値へは戻りません）。
- 認証情報の値が設定ファイルに書き込まれることはありません。AutoPush が登録を無効化したときの登録の更新（数日後に起きることがあります）でも使われるため、コンテナが動いている間は設定したままにしてください。`status` は、値を表示せずに環境変数から取得していることだけを示します。

### 初回起動と保存される状態

- 登録が無く、認証情報が両方設定されていると、`listen` は AutoPush と X に登録して結果を保存し、受信を始めます。既存の設定ファイルのほかの設定（Webhook、通知フィルタ、`[twitter]`）は保持され、ファイルが無ければ権限 `0600` で作成されます。X に登録する前に保存できることを確かめるため、読み取り専用のマウントでは X 側に登録が残らないまま失敗します。
- 登録情報（UAID と鍵）は `./config/angelic-angel.toml`、通知の履歴は `./config/angelic-angel.toml.notifications.sqlite3` に保存されます。再起動をまたいで `./config` を残してください。認証情報と同様に扱ってください。
- 自動登録に失敗した場合（Cookie が無効になっているなど）は、エラーを出力してから 30 秒待って、0 以外の終了コードで終了します。`restart: unless-stopped` によって X のログイン用エンドポイントへ短い間隔で繰り返しアクセスしないためです。待機中に `docker stop`（SIGINT）を実行すると、すぐに終了します。X に接続する前に分かるエラー（認証情報が片方だけ、`./config` に書き込めないなど）はすぐに終了します。
- 認証情報も登録も無い場合、`listen` は "No registration found" で終了し、再起動ポリシーにより再試行を続けます。認証情報を設定するか、登録済みの設定ファイルを `./config` に置いてください。

### 認証情報を差し替える

```sh
docker compose stop
docker compose run --rm angelic-angel unregister
sudo rm config/angelic-angel.toml     # そのあと .env を新しい値に編集
docker compose up -d
```

`unregister` で AutoPush の登録を解除し、設定ファイルを消すと、次の起動で新しい認証情報により登録し直されます。そのファイルに保存されていた Webhook やフィルタの設定も消えます。履歴データベースは残ります。

### 複数の場所で動かさない

同じ登録（設定ファイル）を 2 か所で同時に動かさないでください。2 つのコンテナがそれぞれ環境変数から自動登録すると UAID が別々になり、同じアカウントの通知が 2 系統届いて、Webhook にすべて二重に転送されます。設定ディレクトリごとにコンテナは 1 つにしてください。

### 環境変数を使わない場合

設定ファイルを持ち込むこともできます。登録済みの `angelic-angel.toml` を `./config`（所有者は上記のとおり）に置くか、コンテナ内で作成します。

```sh
# 認証情報は対話入力（シェル履歴に秘密情報を残さないため）
docker compose run --rm -it angelic-angel init
docker compose run --rm angelic-angel register
docker compose run --rm angelic-angel status
docker compose up -d
```

登録ができる前に `up` を実行しないでください。失敗して再起動ポリシーによりループします。

### `/data` ボリュームからの移行

以前のバージョンは、`/data` にマウントした名前付きボリューム `angelic-angel-data` にすべてを保存していました。切り替える前に `./config` へコピーしてください。

```sh
docker compose down                       # ボリュームは残ります（-v は付けない）
docker volume ls | grep angelic-angel-data   # 名前は <プロジェクト名>_angelic-angel-data
mkdir config
docker run --rm -v <プロジェクト名>_angelic-angel-data:/from:ro -v "$PWD/config":/to busybox:1.37.0 \
  sh -c 'cp -a /from/. /to/ && chown -R 65532:65532 /to && chmod 700 /to'
docker compose up -d
```

`./config` で動くことを確認したら、古いボリュームを `docker volume rm` で削除してください。

## 開発

Rust は 1.99.0 に固定されています（`rust-toolchain.toml`、CI、Docker、Dev Container）。

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features   # 現在テストは 0 件
docker build -t angelic-angel:local .
scripts/docker-smoke.sh angelic-angel:local        # オフライン・ダミー認証情報のみ
```

ランタイムは strip 済みバイナリを載せた Distroless で、シェル・ソース・ビルドキャッシュを含みません。`scripts/docker-smoke.sh` はダミーの認証情報とネットワーク無効化を使い、設定の永続化、`ANGELIC_ANGEL_CONFIG`、権限、bind mount したディレクトリの所有者への切り替え（root 所有・別 UID 所有）と `--user` 明示時の挙動、エラー時の終了コード（30 秒待機する自動登録の失敗を含む）、SIGINT による停止を検証します。必要に応じて別の BusyBox ヘルパーを pull し、作成したテスト用リソースを削除します。

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
- `.github/workflows/deployment.yaml` は `master` への push、`v*` タグ、手動実行（`master` のみ）で動作し、先に integration を呼び出してから、マルチアーキテクチャ（`linux/amd64`、`linux/arm64`）イメージを `ghcr.io/<owner>/<repo>` へ公開します。アーキテクチャごとにネイティブランナー（amd64 は `ubuntu-24.04`、arm64 は `ubuntu-24.04-arm`、QEMU は不使用）でビルドし、結果を 1 つのマルチアーキテクチャイメージにまとめます。タグは `sha-<short>`、`master` では `latest`、`master` の履歴上にあり `Cargo.toml` と一致する `v<version>` タグのときのみ `<version>` です。`packages: write` はビルドジョブとマージジョブだけが持ちます。
- コミットメッセージは Conventional Commits に従います（`.commitlintrc.yaml`）。

## 再接続

Angelic Angel は Firefox 互換の再接続戦略を実装しています:

- 指数バックオフ: 5秒 × 2^n (上限 5 分)
- UAID 無効化時の自動再登録
- サーババックオフ (close code 4774): 30 分間の待機
- 接続成功時にリトライカウンタをリセットする無限リトライ

## ライセンス

MIT

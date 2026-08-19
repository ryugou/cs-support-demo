# admin-ui

Issue #31 管理画面（`/admin`）の Angular v22 standalone SPA。設計の正本は
`docs/superpowers/specs/2026-08-16-admin-dashboard-design.md` §5。

## `/admin` 配信用ビルド

サーバは `server/config.toml` / `server/config.local-https.toml` の
`admin_static_dir = "../admin-ui/dist/admin-ui/browser"` から、このディレクトリの
`ng build` 成果物を配信する（このディレクトリから見て `dist/admin-ui/browser`）。

```sh
npm run build -- --configuration=production --base-href=/admin/
```

**`--base-href=/admin/` は必須。** 省略すると `index.html` の `<base href="/">` がルート
基準のまま出力され、サーバの `/admin` 配下で配信したときにアセット（JS/CSS チャンク）が
`/admin/main-xxx.js` ではなく `/main-xxx.js` を要求して 404 になり、画面が起動しない
（白画面）。

## client_id の埋め込みについて

`src/environments/environment.prod.ts` / `environment.ts` の `googleClientId` は、
ビルド時 placeholder（`__CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID__`）のままソースに入っている。
実値への置換は **Docker イメージビルド時**（`Dockerfile` の `admin-ui-builder` ステージ、
`ARG CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID` を受け取って `sed` で置換。値が空、または
英数字・`-`・`.`・`_` 以外を含む場合はビルドを失敗させる fail-closed 検査あり）にのみ行う。

上記の `npm run build` をローカルで直接実行しても placeholder は置換されない。その成果物を
`admin_static_dir` に置いて `/admin` を開いても、Google Identity Services の初期化が不正な
client_id で行われるため **ログインできない**。ログインまで確認したい場合は、Google Cloud
Console 発行済みの実際の client_id で `environment.prod.ts` を一時的に書き換えてから
ビルドすること（コミットしない）。CI での置換手順の正本はリポジトリ `CLAUDE.md` の
「管理画面（/admin）の GitHub Actions variable」節。

## サーバと組み合わせたローカル起動

1. 上記のとおりビルドし、`admin-ui/dist/admin-ui/browser` に成果物を生成する。
2. `server/` を起動する（起動コマンドはリポジトリ `CLAUDE.md` の「ローカル MCP 起動手順」節が
   正本）。
3. ブラウザで `https://127.0.0.1:3443/admin/` を開く。

`ng serve`（`npm start`、ポート 4200）は **proxy 設定（`proxyConfig`）を持たない。**
`environment.ts` の `apiBase`（`/admin/api`）はサーバ側のパスであり、
`ng serve` 単体では到達できない（同一オリジンに `server` プロセスが存在しないため、
`GET http://localhost:4200/admin/api/...` は 404 になる）。API を叩く動作
確認は、上記の本番相当ビルドを `server` から配信させる形で行うこと。`ng serve` は
テンプレート・スタイルの見た目確認にのみ使う。

## テスト

```sh
npm run test -- --watch=false
```

Vitest（`ng test` 経由）で `src/app/**/*.spec.ts` を実行する。

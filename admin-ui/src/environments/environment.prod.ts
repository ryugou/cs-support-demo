/**
 * 本番ビルド用 environment（`ng build` の production configuration が `fileReplacements` で
 * environment.ts の代わりにこのファイルを使う）。
 *
 * `googleClientId` はビルド時 placeholder のまま出荷する。実値は Docker イメージビルド時に
 * `sed` 等でこの文字列を置換する想定。置換対象の文字列を変更する場合は静的配信・Dockerfile
 * 担当と合意すること。
 */
export const environment = {
  production: true,
  googleClientId: '__CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID__',
  apiBase: '/urtect/admin/api',
};

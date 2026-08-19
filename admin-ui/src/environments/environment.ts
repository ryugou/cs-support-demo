/**
 * ローカル開発用 environment。
 *
 * `googleClientId` はビルド時 placeholder のまま出荷する。実値は Docker イメージビルド時に
 * `sed` 等でこの文字列を置換する想定（後続タスク、CLAUDE.md「LINE アダプタ」節と同様の
 * 「秘密はビルド成果物に焼き込まず注入する」方針に揃える）。この文字列そのものを変更すると
 * 置換スクリプト側も追随が必要になるため、変更する場合は静的配信・Dockerfile 担当と合意すること。
 */
export const environment = {
  production: false,
  googleClientId: '__CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID__',
  apiBase: '/admin/api',
};

// homesec advisor サービス本体のプレースホルダ。
//
// plan `docs/superpowers/plans/2026-08-17-homesec-advisor.md` Task 1 の範囲は
// schema・config・ビルド配線のみで、advisor パイプライン本体（LLM 理解・応答種別決定・
// 材料検索・下書き生成・HTTP handler・axum router 起動）は Task 3〜6 で実装する。
// この時点ではイメージへ同梱するバイナリを成立させることだけが目的（`cargo build`
// が通り、Dockerfile の `--bin homesec_advisor` / `COPY` が解決すること）。
fn main() {}

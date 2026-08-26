//! 定型文(design doc `2026-08-17-homesec-advisor-design.md` §4.3, §4.4 の canned 応答)。
//!
//! `safety` / `out_of_domain` / `fallback` は引数を取らないため定数として持つ。
//! `lead_solicit` / `lead_time_pref_reask` は config 由来の値を埋め込むため関数として
//! 持つ。`lead_confirmed` も同様に関数として持つが、埋め込むのは顧客発話の原文ではなく
//! `decide::format_confirmed_slot` が組み立てた正規化済みの値である(`lead_confirmed` の doc
//! comment 参照。顧客発話をそのまま埋め込むと NG 辞書・出口関門を経由しない経路になる)。
//! `clarify` / `answer` の定型文はここに置かない(LLM Call #2 で生成する。design doc §4.3
//! 手順6・7、Task 5 の範囲)。
//!
//! **Issue #50 バッチ2**: design doc §13.2 のモード遷移で `AdvisorAction::Handoff` を廃止した
//! ことに伴い、旧 handoff 定型文(`handoff` 関数)は削除した。`urtect_support` はその場で
//! `crate::advisor::cs_support::run_support_turn` を呼び、CS の応答をそのまま返す
//! (`crate::advisor::api::support_mode_reply_parts` 参照)。

use crate::harness::hours;

/// emergency(design doc §4.3 手順1)の定型文。dialogue-examples パターン 6 の AI 初回応答と
/// 一致させてある。
pub const SAFETY_TEXT: &str = "まず安全を最優先してください。少しでも危険を感じたら、ためらわず 110 番に電話してください。緊急でない場合は警察相談専用電話 #9110 でも相談できます。ドアと窓の施錠を確認して、無理に外へ出たり声をかけたりしないでください。";

/// in_domain == false(design doc §4.3 手順3)の定型文。dialogue-examples パターン 9 の
/// 構造(謝る→守備範囲外と伝える→対応できる範囲を添えて次につなげる)を保った一般化版
/// (パターン 9 の「投資信託」は個別トピックであり、定型文は特定トピックへ言及できないため)。
pub const OUT_OF_DOMAIN_TEXT: &str = "ごめんなさい、私はホームセキュリティ専門のアドバイザーなので、その内容にはお答えできないんです。住まいの防犯、ご家族の見守り、火災などの防災まわりなら、なんでも相談してください。";

/// LLM Call #1/#2 の失敗・出口関門違反時の定型文。dialogue-examples に該当パターンが無いため、
/// ペルソナ(専属アドバイザー、企業 CS 定型句を使わない)を保った一般的な文言として新規に定めた。
pub const FALLBACK_TEXT: &str =
    "うまく答えを整理できませんでした。もう一度、状況を教えてもらえますか?";

/// 時間帯受付モードの初回の呼びかけ(design doc §4.3 手順5)。dialogue-examples パターン 11 の
/// 3 ターン目 AI 応答をそのまま使うが、営業時間は `hours::business_hours_label` で動的に埋め込む。
pub fn lead_solicit(cfg: &crate::config::BusinessHoursConfig) -> String {
    format!(
        "ありがとうございます、では担当者から連絡しますね。ご都合のいい時間帯はありますか?ご連絡できるのは{}です。",
        hours::business_hours_label(cfg)
    )
}

/// 時間帯受付を継続する(まだ確定していない)ときの再質問(design doc §4.4 手順2)。
/// dialogue-examples パターン 11 の 4 ターン目 AI 応答(営業時間外希望への即時案内)をそのまま
/// 使うが、営業時間は動的に埋め込む。抽出が「時間帯の話ではない」と判定された場合の再質問にも
/// このまま流用する。
pub fn lead_time_pref_reask(cfg: &crate::config::BusinessHoursConfig) -> String {
    format!(
        "ごめんなさい、ご連絡できるのが{}の間なんです。この中でしたら、いつがご都合いいですか?",
        hours::business_hours_label(cfg)
    )
}

/// 時間帯確定(design doc §4.4 手順3)。dialogue-examples パターン 11 の最終 AI 応答をそのまま
/// 使う。
///
/// `slot` には `decide::format_confirmed_slot` が組み立てた**正規化済み**の文字列だけを渡す
/// こと。顧客発話の原文(`extraction.raw` / `conv.preferred_contact_time`)を渡してはならない
/// — 顧客発話をそのまま定型文へ反射すると NG 辞書・出口関門を経由しない経路になる
/// (`decide.rs` の `decide_time_pref` doc comment 参照)。
pub fn lead_confirmed(slot: &str) -> String {
    format!(
        "{slot}ですね、担当者からご連絡します。それまでに気になることが出てきたら、いつでもここで聞いてくださいね。"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BusinessHoursConfig;
    use crate::harness::egress::{EgressVerdict, EmitChannel, EmitContext, NgDictionary};

    /// Task 2 で作成済みの advisor 固有 NG 辞書(`server/data/homesec/ng.json`)を読む。
    fn ng_dictionary() -> NgDictionary {
        NgDictionary::from_path(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/data/homesec/ng.json"
        )))
        .expect("homesec ng dictionary (server/data/homesec/ng.json) must load")
    }

    fn ctx() -> EmitContext {
        EmitContext {
            channel: EmitChannel::CustomerChat,
        }
    }

    fn assert_passes_ng_gate(text: &str) {
        let verdict = crate::harness::egress::egress_gate(text, &ctx(), &ng_dictionary());
        assert!(
            matches!(verdict, EgressVerdict::Pass),
            "canned text must not trip the NG dictionary: {text:?} -> {verdict:?}"
        );
    }

    // --- 定型文が NG 辞書に抵触しないこと(design doc 不変条件) ---

    #[test]
    fn safety_text_passes_ng_gate() {
        assert_passes_ng_gate(SAFETY_TEXT);
    }

    /// dialogue-examples パターン 6 の AI 初回応答と一字一句一致することを固定するリグレッション
    /// テスト。以前は「110番」「#9110でも」のように半角スペースが抜けており、ドキュメント上部の
    /// コメントの「一致させてある」という主張が実際には成立していなかった(監査で発見)。
    #[test]
    fn safety_text_matches_dialogue_example_pattern_6_verbatim() {
        assert_eq!(
            SAFETY_TEXT,
            "まず安全を最優先してください。少しでも危険を感じたら、ためらわず 110 番に電話\
             してください。緊急でない場合は警察相談専用電話 #9110 でも相談できます。ドアと窓の\
             施錠を確認して、無理に外へ出たり声をかけたりしないでください。"
        );
    }

    #[test]
    fn out_of_domain_text_passes_ng_gate() {
        assert_passes_ng_gate(OUT_OF_DOMAIN_TEXT);
    }

    #[test]
    fn fallback_text_passes_ng_gate() {
        assert_passes_ng_gate(FALLBACK_TEXT);
    }

    #[test]
    fn lead_solicit_text_passes_ng_gate() {
        assert_passes_ng_gate(&lead_solicit(&BusinessHoursConfig::default()));
    }

    #[test]
    fn lead_time_pref_reask_text_passes_ng_gate() {
        assert_passes_ng_gate(&lead_time_pref_reask(&BusinessHoursConfig::default()));
    }

    #[test]
    fn lead_confirmed_text_passes_ng_gate() {
        assert_passes_ng_gate(&lead_confirmed("平日13:00〜15:00"));
    }

    // --- 文言の内容 ---

    #[test]
    fn lead_solicit_includes_the_business_hours_label_and_follows_config_changes() {
        let cfg = BusinessHoursConfig::default();
        let text = lead_solicit(&cfg);
        assert!(
            text.contains(&hours::business_hours_label(&cfg)),
            "lead_solicit text: {text}"
        );

        let mut other_cfg = cfg.clone();
        other_cfg.days = "everyday".to_string();
        other_cfg.start = "09:00".to_string();
        other_cfg.end = "21:00".to_string();
        let other_text = lead_solicit(&other_cfg);
        assert!(
            other_text.contains(&hours::business_hours_label(&other_cfg)),
            "lead_solicit text must track config changes: {other_text}"
        );
        assert_ne!(text, other_text);
    }

    #[test]
    fn lead_time_pref_reask_includes_the_business_hours_label_and_follows_config_changes() {
        let cfg = BusinessHoursConfig::default();
        let text = lead_time_pref_reask(&cfg);
        assert!(
            text.contains(&hours::business_hours_label(&cfg)),
            "lead_time_pref_reask text: {text}"
        );

        let mut other_cfg = cfg.clone();
        other_cfg.start = "08:00".to_string();
        let other_text = lead_time_pref_reask(&other_cfg);
        assert!(
            other_text.contains(&hours::business_hours_label(&other_cfg)),
            "lead_time_pref_reask text must track config changes: {other_text}"
        );
        assert_ne!(text, other_text);
    }

    #[test]
    fn lead_confirmed_includes_the_given_slot_verbatim() {
        // "土曜の午前中" のような曜日+あいまい時間帯の文字列は decide::format_confirmed_slot が
        // 絶対に生成しない(常に "平日13:00〜15:00" のような実効範囲の形になる)。実際に
        // 生成されうる形で verbatim 埋め込みを確認する。
        let text = lead_confirmed("平日13:00〜15:00");
        assert!(
            text.contains("平日13:00〜15:00"),
            "lead_confirmed text: {text}"
        );
    }
}

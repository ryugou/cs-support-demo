//! 応答種別の決定(design doc `2026-08-17-homesec-advisor-design.md` §4.3, §4.4)。
//!
//! `decide` は純関数(LLM Call #1 の結果と case 状態から応答種別を決めるだけ)。時間帯受付
//! モード中の「確定か継続か」の判定は非同期の希望時間帯抽出(`time_pref::extract_time_preference`)
//! を要するため `decide` からは分離し、[`decide_time_pref`] が担う。

/// design doc の会話設計をコードに落とした応答種別(8 種類)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvisorAction {
    /// design doc §4.3 手順1、§9 不変条件5。**呼び出し側の契約**: この action を返したら
    /// `conv.awaiting_time_pref = false` と `conv.time_pref_false_count = 0` をセットして
    /// 書き戻すこと(安全案内を最優先し、次ターンは通常フローへ復帰させる。dialogue-examples
    /// パターン 6)。忘れると、緊急対応中に armed のまま残った `awaiting_time_pref` により
    /// 次ターンが必ず希望時間帯抽出パスへ入ってしまう(例: 「夜になるのが怖いです」が
    /// `is_time_preference = true` に倒れ、110番案内の直後に時間帯の再質問が出る)。
    /// リード獲得は失われない(`lead_requested` は false のままなので、顧客が改めて連絡を
    /// 希望すれば手順5が再び成立する)。
    Safety,
    TimePrefContinue,
    OutOfDomain,
    Handoff,
    /// design doc §4.3 手順5。**呼び出し側の契約**: この action を返したら、返信を組み立てる前に
    /// 次の 3 つをセットすること(`api.rs` の `arm_time_pref_solicitation` と同じ 3 操作):
    /// - `conv.awaiting_time_pref = true`
    /// - `conv.time_pref_false_count = 0`
    /// - `conv.clarify_turns = 0`(聞き返しループとは別の会話段階へ移るため 0 に戻す)
    ///
    /// 忘れると時間帯受付モードへ遷移せず、リードフローが開始しない。
    LeadSolicit,
    /// design doc §4.3 手順6。**呼び出し側の契約**: この action を返したら `conv.clarify_turns`
    /// を 1 増やして support_case へ書き戻すこと(`api.rs` の `conv.clarify_turns += 1` と
    /// 同じ操作)。忘れると `clarify_turns` が永久に 0 のままになり、`decide_in_domain_flow` の
    /// `conv.clarify_turns < clarify_max` ゲートが機能しない。結果、`concern` を答えない顧客に
    /// 対して `Clarify` を無限に返し続ける(design doc §4.3 手順6 の `clarify_turns < 3` 予算が
    /// コード上まったく機能しなくなる)。
    Clarify {
        missing: Vec<crate::advisor::understand::ConditionKey>,
    },
    Answer,
    /// design doc §4.4 手順3。**呼び出し側の契約**: この action を返したら
    /// `AdvisorCaseAttrs.lead_requested = true` をセットして [`advisor_attr_updates`] で
    /// support_case へ書き戻すこと。忘れると同一会話で再び [`AdvisorAction::LeadSolicit`] が
    /// 発火しうる([`decide`] は `lead_requested` を見て手順5を判定するため)。
    LeadConfirmed {
        slot: String,
    },
}

/// design doc §4.3 の優先順(1〜7)をそのままコード化した純関数。
///
/// `_lead_offered` はこの関数の分岐には使わない。他の case 属性(`lead_requested`)との
/// 対称性のため受け取っているだけで、「1 会話に 1 回だけ提案する」という抑止は LLM Call #2 の
/// プロンプト注入規則(design doc §4.4 手順1)の責務であり、`decide` の分岐対象ではない
/// (Task 5 の範囲)。
///
/// 手順2(時間帯受付モード中の「確定か継続か」)は非同期の希望時間帯抽出を要するため、ここでは
/// `conv.awaiting_time_pref == true` の間は無条件で [`AdvisorAction::TimePrefContinue`] を返す。
/// 実際の確定判定は呼び出し元が抽出結果を得たあとに [`decide_time_pref`] を呼んで行う
/// (これにより `decide` は純関数のまま保てる)。
///
/// **累積条件の契約(design doc §4.2)**: `u.conditions` はこのターン単体の観測ではなく、
/// **累積済み全量**である前提で判定する(手順6 `missing_conditions` は `u.conditions` を
/// そのまま見る)。呼び出し側は `decide` を呼ぶ前に、support_case に保存された累積条件を
/// [`parse_accumulated_conditions`] で復元し、今回ターンの観測条件と [`merge_conditions`] で
/// merge した結果を `Understanding.conditions` に差し替えてから渡すこと。これを怠ると、
/// 前ターンで既に確定した条件が消えたように見え、`Clarify` を無限に繰り返す(reviewer 指摘の
/// 再現手順: 1 ターン目 concern=intrusion → Clarify{Housing}、2 ターン目
/// housing=apartment_rented のみを渡すと、concern が消えたように見えて再度
/// Clarify{Concern} になってしまう)。
pub fn decide(
    u: &crate::advisor::understand::Understanding,
    conv: &crate::harness::CaseConvState,
    _lead_offered: bool,
    lead_requested: bool,
    clarify_max: u32,
) -> AdvisorAction {
    // 手順1: emergency は他の何より優先する。
    if u.emergency {
        return AdvisorAction::Safety;
    }
    // 手順2: 時間帯受付モード中は decide 自身は確定判定を行わない(上記 doc comment 参照)。
    if conv.awaiting_time_pref {
        return AdvisorAction::TimePrefContinue;
    }
    decide_in_domain_flow(u, conv, lead_requested, clarify_max, false)
}

/// 提案に必要な条件のうち未取得のものを返す(design doc §4.3 手順6)。
///
/// `concern` 自体が無い間は `housing` の要否を判定できない(`concern` の値によって `housing` が
/// 必要かどうかが決まるため)ので `[Concern]` のみを返す。`concern == intrusion` のときだけ
/// `housing` も必要とする。
fn missing_conditions(
    u: &crate::advisor::understand::Understanding,
) -> Vec<crate::advisor::understand::ConditionKey> {
    use crate::advisor::understand::ConditionKey;

    let concern = u
        .conditions
        .iter()
        .find(|(key, _)| *key == ConditionKey::Concern)
        .map(|(_, value)| value.as_str());

    match concern {
        None => vec![ConditionKey::Concern],
        Some("intrusion") => {
            let has_housing = u
                .conditions
                .iter()
                .any(|(key, _)| *key == ConditionKey::Housing);
            if has_housing {
                Vec::new()
            } else {
                vec![ConditionKey::Housing]
            }
        }
        Some(_) => Vec::new(),
    }
}

/// 手順3〜7(emergency・時間帯受付モードの判定を終えたあとの通常フロー)。[`decide`] 本体と
/// [`decide_time_pref`] の「時間帯の話ではなかった」経路の両方から呼ばれる共有ロジック
/// (design doc §3.2「解釈・状態機械は既存実装のまま」= CS と同様、時間帯の話でなければ
/// 通常の判定へ続行する。reviewer 指摘: 従来は無条件で `TimePrefContinue` を返しており、
/// 顧客の実際の質問が無視されていた)。
///
/// `suppress_lead_solicit`: true のときは手順5([`AdvisorAction::LeadSolicit`])を評価せず、
/// 手順6・手順7へそのまま進む。**時間帯受付フローの内側**([`decide_time_pref`] の
/// `PassToEvaluate` 分岐・打ち切り分岐、[`decide_time_pref_extraction_failed`] の打ち切り分岐)
/// から呼ぶときは常に true を渡すこと(reviewer 一次レビュー Critical 指摘)。
///
/// **なぜ抑止するのか**: `AdvisorAction::LeadSolicit` の doc comment が定める呼び出し側の契約は
/// 「返したら `conv.awaiting_time_pref = true` と `conv.time_pref_false_count = 0` をセット
/// する」。時間帯受付モードを打ち切って `awaiting_time_pref = false` にした直後にこの関数から
/// `LeadSolicit` を返すと、呼び出し側が契約どおりに `awaiting_time_pref` を再び true・
/// `time_pref_false_count` を 0 に戻してしまい、**打ち切った直後の同一ターンで受付モードを
/// 再開始する**。理解 LLM(`understand.rs` のプロンプト)は「お願いします」を含む発話に対して
/// `lead_interest = true` を返す例を明示しているため、営業時間外の希望を「20時でお願いします」
/// のように出す顧客はほぼ確実にこの経路を踏み、「受付開始 → 再質問 → 打ち切り(のはずが同一
/// ターンで即再開始)」を無限に繰り返す(`time_pref_false_count` による 2 回連続打ち切りが
/// 完全に無効化される livelock)。
///
/// **残る制約(このスコープでは解決しない)**: ここで抑止できるのは「打ち切りと同一ターン」の
/// 再開始のみ。打ち切って `awaiting_time_pref = false` になった**次のターン以降**は
/// [`decide`] 本体(`suppress_lead_solicit = false`)経由になるため、手順5は通常どおり再び
/// 成立しうる。顧客が営業時間外の希望を出し続ける限り、「受付開始 → 再質問 → 打ち切り →
/// 通常応答 → (次ターンで lead_interest 再検出)→ 受付開始」という複数ターンにまたがる周期は
/// 残る。これを完全に断つには「1 会話での受付試行回数」を support_case 属性として永続化し、
/// この関数の手順5 判定に組み込む必要があるが、それは呼び出し側(Task 6)の配線と design doc
/// §4.2/§4.4 の spec 更新を伴うため今回のスコープ外。Task 6 の実装者はこの制約を踏まえること。
fn decide_in_domain_flow(
    u: &crate::advisor::understand::Understanding,
    conv: &crate::harness::CaseConvState,
    lead_requested: bool,
    clarify_max: u32,
    suppress_lead_solicit: bool,
) -> AdvisorAction {
    // 手順3
    if !u.in_domain {
        return AdvisorAction::OutOfDomain;
    }
    // 手順4
    if u.urtect_support {
        return AdvisorAction::Handoff;
    }
    // 手順5(抑止時はスキップ。上記 doc comment 参照)
    if !suppress_lead_solicit && u.lead_interest && !lead_requested {
        return AdvisorAction::LeadSolicit;
    }
    // 手順6
    let missing = missing_conditions(u);
    if !missing.is_empty() && conv.clarify_turns < clarify_max {
        return AdvisorAction::Clarify { missing };
    }
    // 手順7
    AdvisorAction::Answer
}

/// design doc §4.3 手順2 と §4.4 手順2〜3 の実装。`conv.awaiting_time_pref == true` のときに、
/// 呼び出し元が非同期で `time_pref::extract_time_preference` を呼んだ**あとに**渡す抽出結果を
/// 受けて、実際に「確定」か「継続」かを決める。
///
/// `handle_time_pref` は無変更のまま呼ぶ(受付モード継続時の false カウンタ・2 回連続不成立での
/// 自動解除といった state 遷移は既存実装のままにする)。ただし `handle_time_pref` は現行 CS の
/// escalation フロー向けの実装で、`is_time_preference == true` である限り**営業時間外でも**
/// 受理して `awaiting_time_pref` を解除し、時間帯を確定させてしまう仕様になっている
/// (「◯◯で承りました。なお対応時間は…」という付記付きの確定応答)。
///
/// advisor のリード獲得フローはこの標準挙動から意図的に逸脱する: CS の escalation は
/// 「受理して案内して終える」で構わないが、advisor は営業リードを取る導線なので、
/// 対応時間内の希望が出るまで会話で追い直したい(dialogue-examples パターン 11)。そのため
/// `handle_time_pref` の戻り値が `Reply` だった場合、ここで独自に「完全包含」判定
/// ([`effective_business_window`])をやり直し、いずれの window も完全には収まらなかった
/// 場合だけ `awaiting_time_pref` を再アームし、`handle_time_pref` が書き込んだ
/// `preferred_contact_time`(時間外注記付きの値)をクリアして「まだ確定していない」状態に
/// 戻す。**部分的にでも重なれば確定していた旧ロジックは reviewer 指摘により廃止した**
/// (「16時から20時なら」(営業は〜18時)のような希望をそのまま確約してしまい、design doc の
/// persona 規則「ウソをつかない」に反していたため)。
///
/// 確定時の `AdvisorAction::LeadConfirmed.slot` は、顧客発話の逐語引用
/// (`conv.preferred_contact_time` / `extraction.raw`)ではなく、[`effective_business_window`]
/// が返す実効範囲(境界省略側を営業時間の境界で補った (start, end))から [`format_confirmed_slot`]
/// で正規化して組み立てる(reviewer 指摘: 顧客発話をそのまま定型文へ反射すると NG 辞書・出口
/// 関門を経由しない経路になる)。**window の生の start/end ではなく実効範囲を渡すのは、
/// 判定した範囲と顧客に見せる文言を必ず一致させるため**(reviewer 指摘 Warning: 以前は境界
/// 省略側を描画から落としており、「17時までに」の希望が「平日17:00までですね」(朝から連絡が
/// 来ると読める)のように判定した範囲より広い約束になっていた)。`conv.preferred_contact_time`
/// 自体は `handle_time_pref` が書き込んだ原文引用のまま残す(管理画面で人間が見る内部メモ
/// としては原文引用のままで問題ない)。
///
/// 完全包含 window が無い(=再アーム)場合、`conv.time_pref_false_count` を使って 2 回連続の
/// 打ち切りを設ける(reviewer 指摘 W3: 従来は再アームに上限が無く、営業時間外の希望を出し
/// 続ける顧客との会話が終わらなかった)。**注意**: `handle_time_pref` は `Reply` を返す経路
/// (`is_time_preference == true`)で必ず `time_pref_false_count` を 0 にリセットしてから返る
/// (`time_pref.rs` は今回の変更対象外で、CS の escalation フロー向けの既存仕様のまま)。その
/// ため、リセット後の値をそのまま起点に数えると「連続」回数が毎ターン 1 で頭打ちになり、
/// 打ち切り条件へ到達できない(実測して確認済み)。ここでは `handle_time_pref` を呼ぶ**前**の
/// 値を保持しておき、それを起点に数える(呼び出し前の値は、直前ターンがこの再アーム分岐
/// だったときに書き残した値、または直前ターンが `PassToEvaluate` 分岐(時間帯の話では
/// なかった)だったときに `handle_time_pref` 自身が積んだカウントを引き継ぐ — どちらも
/// 「時間帯受付モード中に進展が無かったターン数」として同じフィールドを共有する設計)。
///
/// `handle_time_pref` が `TimePrefAction::PassToEvaluate`(「時間帯の話ではなかった」)を返した
/// 場合は、design doc §3.2 のとおり通常の判定フローへ続行する([`decide_in_domain_flow`] に
/// 委譲する。reviewer 指摘: 従来は無条件で `TimePrefContinue` を返しており、顧客の実際の質問が
/// 無視されていた)。`awaiting_time_pref` はここでは変更しない(`handle_time_pref` が 2 回連続
/// 不成立で自動解除するかどうかを判断済み)。
///
/// **呼び出し側の契約**: この関数は `conv` を破壊的に更新する(`time_pref_false_count` /
/// `time_pref_extraction_error_count` / `awaiting_time_pref` / `preferred_contact_time`)。
/// 返却後、応答を返す前に必ず support_case へ保存すること(`api.rs` の `save_conv_state` と
/// 同じ)。保存を怠ると、次ターンの呼び出しはこの関数が書いた値ではなく永続化前の古い値から
/// 再開する。特に再アーム分岐(下記)は保存漏れがあると `time_pref_false_count` が毎ターン
/// 0 起点になり、2 回連続打ち切り条件へ永久に到達しない(前ラウンドで潰した livelock が
/// 別経路で復活する)。
pub fn decide_time_pref(
    u: &crate::advisor::understand::Understanding,
    extraction: &crate::harness::time_pref::TimePrefExtraction,
    conv: &mut crate::harness::CaseConvState,
    cfg: &crate::config::BusinessHoursConfig,
    lead_requested: bool,
    clarify_max: u32,
) -> AdvisorAction {
    // emergency は時間帯受付モード中でも最優先(design doc §9 不変条件5)。呼び出し側が
    // 事前にチェックしている想定でも、ここでも防御的に確認する(このモード中は
    // handle_time_pref を呼んで state を変更してしまう前に emergency を弾く必要がある)。
    // ここで conv を変更しないのは意図的(handle_time_pref を呼ぶ前に早期returnするため)だが、
    // awaiting_time_pref の解除自体は呼び出し側の責務(AdvisorAction::Safety の doc comment
    // 参照)。呼び出し側が解除を怠ると、緊急対応中も時間帯受付モードが armed のまま残る。
    if u.emergency {
        return AdvisorAction::Safety;
    }
    // reviewer 一次レビュー Warning 指摘: api.rs 1018 行目(`handle_time_pref` を呼ぶ直前)と
    // 同じ意味づけで、抽出に成功したターン(=この関数が呼ばれたターン)で連続失敗カウンタを
    // リセットする。これが無いと「失敗→成功→失敗→成功→失敗」のように連続していない失敗でも
    // カウンタが積み上がり、`CaseConvState::time_pref_extraction_error_count` の doc が定義する
    // 「連続回数」の意味に反して誤って打ち切ってしまう。emergency の早期 return より後に置く
    // (emergency 分岐は conv を一切変更しない、という既存の性質を保つ)。
    conv.time_pref_extraction_error_count = 0;
    let unproductive_turns_before_this_call = conv.time_pref_false_count;
    let action = crate::harness::time_pref::handle_time_pref(extraction, conv, cfg);
    match action {
        crate::harness::time_pref::TimePrefAction::PassToEvaluate => {
            // 「時間帯の話ではなかった」→ design doc §3.2 のとおり通常の判定フローへ続行する
            // (awaiting_time_pref はここでは変更しない。handle_time_pref が 2 回連続不成立で
            // 自動解除するかどうかを判断済み)。suppress_lead_solicit = true: ここは時間帯受付
            // フローの内側であり、手順5(LeadSolicit)を成立させると呼び出し側の契約により
            // 受付モードが再開始してしまう(decide_in_domain_flow の doc comment 参照)。
            decide_in_domain_flow(u, conv, lead_requested, clarify_max, true)
        }
        crate::harness::time_pref::TimePrefAction::Reply(_) => {
            let fitting_window = extraction
                .windows
                .iter()
                .find_map(|w| effective_business_window(w, cfg).map(|eff| (w.days, eff)));
            match fitting_window {
                Some((days, eff)) => {
                    conv.time_pref_false_count = 0;
                    AdvisorAction::LeadConfirmed {
                        slot: format_confirmed_slot(days, eff, cfg),
                    }
                }
                None => {
                    conv.preferred_contact_time = None;
                    let unproductive_turns = unproductive_turns_before_this_call + 1;
                    conv.time_pref_false_count = unproductive_turns;
                    if unproductive_turns >= 2 {
                        conv.awaiting_time_pref = false;
                        conv.time_pref_false_count = 0;
                        // CLAUDE.md: エラーをログも吐かずに握りつぶすことを禁止。顧客は希望
                        // 時間帯を 2 回連続で示したが、いずれも営業時間に収まらず打ち切った
                        // (=リードの取りこぼし)。顧客発話の本文は出さず、件数のみ残す。
                        tracing::warn!(
                            route = "advisor_time_pref",
                            unproductive_turns,
                            "advisor gave up soliciting a business-hours-fitting contact time \
                             after 2 consecutive non-fitting turns; the lead is lost with no \
                             trace in the case attrs beyond this log line. If this recurs \
                             frequently, review business_hours config against actual customer \
                             availability"
                        );
                        // suppress_lead_solicit = true: 打ち切った直後の同一ターンで手順5
                        // (LeadSolicit)を成立させると、呼び出し側の契約により受付モードが
                        // 即座に再開始し、この打ち切りそのものが無意味になる
                        // (decide_in_domain_flow の doc comment 参照)。
                        decide_in_domain_flow(u, conv, lead_requested, clarify_max, true)
                    } else {
                        conv.awaiting_time_pref = true;
                        AdvisorAction::TimePrefContinue
                    }
                }
            }
        }
    }
}

/// 希望時間帯(`PrefWindow`)が営業時間に完全に収まっているかを判定し、完全包含なら実効範囲
/// `(eff_start, eff_end)` を返す純関数。完全包含でなければ `None`。
///
/// `overlaps_business_hours`(部分的にでも重なれば true)ではなく完全包含を要求する。
/// 部分重なりを確定条件にすると「16時から20時なら」(営業は〜18時)のような希望を
/// そのまま確約してしまい、design doc の persona 規則(ウソをつかない)に反する
/// (reviewer 指摘の再現手順そのもの)。
///
/// 境界省略側(`None`)は「顧客がその側を明言していない」ことを意味し、営業時間側の境界を
/// そのまま採用する(その側では営業時間による制約を顧客側が受け入れている、という解釈)。
/// そのため `start=None, end=None`(「いつでもいい」)は、曜日が両立する限り常に完全包含として
/// 扱われる。**戻り値の実効範囲は、確定時に顧客へ提示する文言([`format_confirmed_slot`])が
/// そのまま使う** — 判定した範囲そのものを顧客へ見せ、判定より広い約束をしないため
/// (reviewer 指摘 Warning: 以前は境界省略側を描画から落としており、「17時までに」の希望が
/// 「平日17:00までですね」(朝から連絡が来ると読める)のように、判定した範囲より広い約束に
/// なっていた)。
///
/// **曜日の両立判定**: `hours::overlaps_business_hours`(公開関数)に事前条件として委譲する。
/// `hours.rs` に対して今回許されている変更は `parse_hm` の可視性変更のみなので、非公開の
/// `days_overlap` をここへ複製しない。曜日が両立しなければ、時刻がどうであれ完全包含は
/// あり得ない(例: 希望が「土曜」で営業が平日のみなら、時刻を指定していなくても完全包含では
/// ない)。曜日が両立しない場合、`overlaps_business_hours` は時刻を見るまでもなく `false` を
/// 返すので、これをそのまま「曜日ゲート」として使える。
fn effective_business_window(
    w: &crate::harness::time_pref::PrefWindow,
    cfg: &crate::config::BusinessHoursConfig,
) -> Option<(chrono::NaiveTime, chrono::NaiveTime)> {
    if !crate::harness::hours::overlaps_business_hours(cfg, w.days, w.start, w.end) {
        return None;
    }
    let cfg_start = crate::harness::hours::parse_hm(&cfg.start)?;
    let cfg_end = crate::harness::hours::parse_hm(&cfg.end)?;
    let eff_start = w.start.unwrap_or(cfg_start);
    let eff_end = w.end.unwrap_or(cfg_end);
    if eff_start <= eff_end && eff_start >= cfg_start && eff_end <= cfg_end {
        Some((eff_start, eff_end))
    } else {
        None
    }
}

/// [`effective_business_window`] の薄いラッパー(完全包含かどうかの bool のみを返す)。本番の
/// 呼び出し元は実効範囲そのものを必要とするため既に [`effective_business_window`] を直接
/// 呼んでおり、この関数はテスト(bool だけで十分な既存テスト群)専用に残している。
#[cfg(test)]
fn window_fits_business_hours(
    w: &crate::harness::time_pref::PrefWindow,
    cfg: &crate::config::BusinessHoursConfig,
) -> bool {
    effective_business_window(w, cfg).is_some()
}

/// 完全包含と判定された実効範囲([`effective_business_window`] の戻り値)から、顧客向け定型文に
/// 埋め込む正規化済みの表示文字列を組み立てる。顧客発話の逐語引用(`extraction.raw`)は使わない
/// — NG 辞書・出口関門を経由せず定型文へ反射される経路になるため(reviewer 指摘)。
///
/// **常に実効範囲を `HH:MM〜HH:MM` で描画する(曜日粒度は落ちる)**。例えば「金曜の17時ごろ」
/// (`PrefWindow { days: Weekday, start: Some(17:00), end: None }`)は「平日17:00〜18:00」になり、
/// 「金曜」という曜日の粒度は失われる。これは意図的なトレードオフで、判定した実効範囲と
/// 顧客に見せる文言を必ず一致させることを優先した(design doc §2.1 ペルソナ規則「ウソを
/// つかない」)。判定した範囲より狭い/広い文言を返すと、顧客に対して確約していない時間帯を
/// 確約したことになる。
///
/// 曜日ラベル: `Weekday` → `"平日"`、`Weekend` → `"土日"`、`Any` → `cfg.days == "everyday"` なら
/// `"毎日"`、それ以外は `"平日"`(顧客が曜日を指定していない以上、実際に連絡できる営業曜日を
/// 提示する)。**この `Any` の規則は `hours::business_hours_label` の規則(`"mon-fri"` と未知値は
/// どちらも `"平日"` 表記)と同期させること。** `hours.rs` の `days_overlap` / ラベル生成は
/// 非公開で、今回 `hours.rs` に許されている変更は `parse_hm` の可視性変更のみなので、ここへ
/// 複製している。
fn format_confirmed_slot(
    days: crate::harness::hours::PrefDays,
    eff: (chrono::NaiveTime, chrono::NaiveTime),
    cfg: &crate::config::BusinessHoursConfig,
) -> String {
    use crate::harness::hours::PrefDays;
    let days_label = match days {
        PrefDays::Weekday => "平日",
        PrefDays::Weekend => "土日",
        PrefDays::Any if cfg.days == "everyday" => "毎日",
        PrefDays::Any => "平日",
    };
    let (start, end) = eff;
    format!(
        "{days_label}{}〜{}",
        start.format("%H:%M"),
        end.format("%H:%M")
    )
}

/// 希望時間帯抽出インフラ自体が失敗した(`time_pref::extract_time_preference` が `Err` を
/// 返した)場合に呼び出し側(Task 6)が使う。[`decide_time_pref`] とは別関数にしているのは、
/// 抽出が成功しなかった以上 `TimePrefExtraction` を渡せないため。
///
/// `api.rs` の `note_time_pref_extraction_failure`(CS の escalation フロー向け)と同じ
/// 閾値・同じフィールド(`conv.time_pref_extraction_error_count`)を使う。3 回連続で抽出
/// インフラが失敗したら、抽出に頼らず通常の判定フローへ委譲する(reviewer 指摘 W1: advisor
/// 側にはこの自動解除ロジックが無く、抽出が壊れ続けると `awaiting_time_pref` から永久に
/// 抜けられなかった)。
const TIME_PREF_EXTRACTION_ERROR_LIMIT: u32 = 3; // api.rs::TIME_PREF_EXTRACTION_ERROR_LIMIT と同値を維持すること

/// **呼び出し側の契約**: この関数は `conv` を破壊的に更新する(`time_pref_extraction_error_count` /
/// `awaiting_time_pref` / `time_pref_false_count`)。返却後、応答を返す前に必ず support_case へ
/// 保存すること(`api.rs` の `save_conv_state` と同じ)。保存を怠ると `time_pref_extraction_error_count`
/// が毎ターン 0 起点になり、3 回連続到達による自動解除が永久に到達しない。
pub fn decide_time_pref_extraction_failed(
    u: &crate::advisor::understand::Understanding,
    conv: &mut crate::harness::CaseConvState,
    lead_requested: bool,
    clarify_max: u32,
) -> AdvisorAction {
    // ここで conv を変更しないのは意図的(抽出インフラ失敗カウンタを積む前に早期returnする
    // ため)だが、awaiting_time_pref の解除自体は呼び出し側の責務(AdvisorAction::Safety の
    // doc comment 参照)。呼び出し側が解除を怠ると、緊急対応中も時間帯受付モードが armed の
    // まま残る。
    if u.emergency {
        return AdvisorAction::Safety;
    }
    conv.time_pref_extraction_error_count += 1;
    if conv.time_pref_extraction_error_count >= TIME_PREF_EXTRACTION_ERROR_LIMIT {
        conv.awaiting_time_pref = false;
        conv.time_pref_false_count = 0;
        conv.time_pref_extraction_error_count = 0;
        // CLAUDE.md: エラーをログも吐かずに握りつぶすことを禁止。希望時間帯抽出インフラが
        // 3 回連続で失敗し、リードの取りこぼしを黙って諦めている。顧客発話の本文は出さず、
        // 失敗回数のみ残す。
        tracing::warn!(
            route = "advisor_time_pref",
            error_count = TIME_PREF_EXTRACTION_ERROR_LIMIT,
            "advisor gave up soliciting a contact time after 3 consecutive time preference \
             extraction infra failures; the lead is lost with no trace in the case attrs \
             beyond this log line. Investigate the extraction infra failure (LLM call error / \
             response parse failure) that preceded this"
        );
        // suppress_lead_solicit = true: 抽出インフラ 3 回連続失敗による打ち切りも、時間帯受付
        // フローの内側からの委譲である以上、同一ターンでの受付モード再開始を防ぐ必要がある
        // (decide_in_domain_flow の doc comment 参照)。
        decide_in_domain_flow(u, conv, lead_requested, clarify_max, true)
    } else {
        AdvisorAction::TimePrefContinue
    }
}

/// support_case の advisor 固有属性 3 つ(design doc §4.4 手順4)。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AdvisorCaseAttrs {
    pub lead_offered: bool,
    pub lead_requested: bool,
    pub shown_product_cards: String,
}

/// support_case の属性 map から [`AdvisorCaseAttrs`] を復元する純関数。欠落・空文字は既定値
/// (false / false / 空文字列)に倒す(`harness::conv_state_from_attrs` と同じ書き方)。
pub fn parse_advisor_case_attrs(
    attrs: &std::collections::HashMap<String, String>,
) -> AdvisorCaseAttrs {
    let get = |key: &str| attrs.get(key).map(String::as_str).unwrap_or("");
    AdvisorCaseAttrs {
        lead_offered: get("lead_offered") == "true",
        lead_requested: get("lead_requested") == "true",
        shown_product_cards: get("shown_product_cards").to_string(),
    }
}

/// [`AdvisorCaseAttrs`] を support_case へ書き戻す差分タプルを組み立てる。呼び出し元が
/// 既存属性へ read-merge-write する(`harness::merge_conv_state_attributes` と同じ形)。
pub fn advisor_attr_updates(attrs: &AdvisorCaseAttrs) -> Vec<(String, String)> {
    vec![
        ("lead_offered".to_string(), attrs.lead_offered.to_string()),
        (
            "lead_requested".to_string(),
            attrs.lead_requested.to_string(),
        ),
        (
            "shown_product_cards".to_string(),
            attrs.shown_product_cards.clone(),
        ),
    ]
}

/// 累積条件(design doc §4.2)を support_case の属性として保持するときのキー名対応表。
/// 1 key につき値は高々 1 つ(CS の signal 累積のような集合ではない)ため、5 key それぞれを
/// 専用の属性として持つ(`AdvisorCaseAttrs` と同じ「個別の named attribute」方式)。
const CONDITION_ATTR_KEYS: [(&str, crate::advisor::understand::ConditionKey); 5] = [
    (
        "advisor_cond_housing",
        crate::advisor::understand::ConditionKey::Housing,
    ),
    (
        "advisor_cond_target",
        crate::advisor::understand::ConditionKey::Target,
    ),
    (
        "advisor_cond_concern",
        crate::advisor::understand::ConditionKey::Concern,
    ),
    (
        "advisor_cond_budget",
        crate::advisor::understand::ConditionKey::Budget,
    ),
    (
        "advisor_cond_install",
        crate::advisor::understand::ConditionKey::Install,
    ),
];

/// 累積条件を support_case の属性 map から復元する純関数。欠落・空文字はそのキーが
/// 「未取得」であることを表す(`Vec` に含めない)。
pub fn parse_accumulated_conditions(
    attrs: &std::collections::HashMap<String, String>,
) -> Vec<(crate::advisor::understand::ConditionKey, String)> {
    CONDITION_ATTR_KEYS
        .iter()
        .filter_map(|(attr_key, condition_key)| {
            attrs
                .get(*attr_key)
                .filter(|v| !v.is_empty())
                .map(|v| (*condition_key, v.clone()))
        })
        .collect()
}

/// 今回ターンで観測した条件(`Understanding.conditions`)を既存の累積条件へ merge する純関数
/// (design doc §4.2「条件は support_case に累積し、ターンごとに全量で再評価する」の実装)。
/// 同じ key は今回の値で上書きし、それ以外の key は既存の値を維持する。
///
/// **契約**: [`decide`] を呼ぶ前に、呼び出し側はこの関数で累積条件と今回ターンの観測条件を
/// merge し、`Understanding.conditions` をこの結果に差し替えてから渡すこと。`decide` 自身は
/// 「渡された `conditions` が既に累積済み全量である」ことを前提にしており、単一ターン分の
/// 観測だけを渡すと、(この修正後は)前ターンまでに確定した条件が反映されないまま判定される
/// (以前は「聞き返しの繰り返し」とだけ書いていたが、実際にはそれより重い。
/// [`accumulated_condition_updates`] は非空の値だけを書き込む設計に直したため、merge を
/// 忘れて単一ターン分だけを渡しても support_case 上の既存条件そのものは消えなくなった。ただし
/// `decide` に渡る `Understanding.conditions` が不完全なままなので、`missing_conditions` は
/// 既に確定済みの条件を「未取得」と誤判定し、`Clarify` を繰り返す。reviewer 指摘の再現手順:
/// 1 ターン目 concern=intrusion → Clarify{Housing}、2 ターン目 housing=apartment_rented のみを
/// 渡すと、concern が消えたように見えて再度 Clarify{Concern} になってしまう)。
pub fn merge_conditions(
    accumulated: &[(crate::advisor::understand::ConditionKey, String)],
    observed_this_turn: &[(crate::advisor::understand::ConditionKey, String)],
) -> Vec<(crate::advisor::understand::ConditionKey, String)> {
    let mut merged = accumulated.to_vec();
    for (key, value) in observed_this_turn {
        if let Some(existing) = merged.iter_mut().find(|(k, _)| k == key) {
            existing.1 = value.clone();
        } else {
            merged.push((*key, value.clone()));
        }
    }
    merged
}

/// merge 済みの累積条件を support_case へ書き戻す属性差分を組み立てる。
///
/// **この関数は追加・上書きのみを行い、決して既存の累積条件を消さない**: 値が非空のキーだけを
/// 返す(未取得キーは戻り値に含めない)。当初は 5 キー全てを必ず返し、未取得キーには空文字を
/// 書いていたが、`merge_conditions` を挟まずにこの関数へ渡す(= 呼び出し側の契約違反)と、
/// その空文字が support_case 上の既存の累積条件をそのまま上書き消去してしまっていた
/// (`parse_accumulated_conditions` は空文字を「未取得」として読むため、一度消えると復元
/// 不能)。design doc §4.2 に条件を消す仕様は無く、`merge_conditions` の doc(上記)が警告する
/// 「聞き返しの繰り返し」よりずっと重い帰結だったため、この関数側を「追加・上書きのみ」に
/// 直した。
pub fn accumulated_condition_updates(
    conditions: &[(crate::advisor::understand::ConditionKey, String)],
) -> Vec<(String, String)> {
    CONDITION_ATTR_KEYS
        .iter()
        .filter_map(|(attr_key, condition_key)| {
            conditions
                .iter()
                .find(|(k, _)| k == condition_key)
                .filter(|(_, v)| !v.is_empty())
                .map(|(_, v)| (attr_key.to_string(), v.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisor::understand::{ConditionKey, Understanding};
    use crate::config::BusinessHoursConfig;
    use crate::harness::hours::PrefDays;
    use crate::harness::time_pref::{PrefWindow, TimePrefExtraction};
    use crate::harness::CaseConvState;
    use crate::test_support::capture_logs;
    use chrono::NaiveTime;

    fn base_understanding() -> Understanding {
        Understanding {
            in_domain: true,
            emergency: false,
            urtect_support: false,
            lead_interest: false,
            summary_ja: "テスト用の要約".to_string(),
            conditions: Vec::new(),
        }
    }

    fn base_conv() -> CaseConvState {
        CaseConvState {
            clarify_turns: 0,
            awaiting_time_pref: false,
            time_pref_false_count: 0,
            preferred_contact_time: None,
            time_pref_extraction_error_count: 0,
        }
    }

    fn time(s: &str) -> NaiveTime {
        NaiveTime::parse_from_str(s, "%H:%M").expect("test fixture must be a valid HH:MM")
    }

    // --- decide: 手順 1〜7 単体 ---

    #[test]
    fn emergency_returns_safety_regardless_of_other_fields() {
        let mut u = base_understanding();
        u.emergency = true;
        u.in_domain = false;
        u.urtect_support = true;
        u.lead_interest = true;
        let conv = base_conv();

        assert_eq!(decide(&u, &conv, false, false, 3), AdvisorAction::Safety);
    }

    #[test]
    fn awaiting_time_pref_returns_time_pref_continue() {
        let u = base_understanding();
        let mut conv = base_conv();
        conv.awaiting_time_pref = true;

        assert_eq!(
            decide(&u, &conv, false, false, 3),
            AdvisorAction::TimePrefContinue
        );
    }

    #[test]
    fn out_of_domain_when_not_in_domain() {
        let mut u = base_understanding();
        u.in_domain = false;
        let conv = base_conv();

        assert_eq!(
            decide(&u, &conv, false, false, 3),
            AdvisorAction::OutOfDomain
        );
    }

    #[test]
    fn handoff_when_urtect_support() {
        let mut u = base_understanding();
        u.urtect_support = true;
        let conv = base_conv();

        assert_eq!(decide(&u, &conv, false, false, 3), AdvisorAction::Handoff);
    }

    #[test]
    fn lead_solicit_when_lead_interest_and_not_yet_requested() {
        let mut u = base_understanding();
        u.lead_interest = true;
        let conv = base_conv();

        assert_eq!(
            decide(&u, &conv, false, false, 3),
            AdvisorAction::LeadSolicit
        );
    }

    #[test]
    fn clarify_missing_concern_when_no_conditions_captured() {
        let u = base_understanding();
        let conv = base_conv();

        assert_eq!(
            decide(&u, &conv, false, false, 3),
            AdvisorAction::Clarify {
                missing: vec![ConditionKey::Concern]
            }
        );
    }

    #[test]
    fn clarify_missing_housing_when_concern_is_intrusion_without_housing() {
        let mut u = base_understanding();
        u.conditions
            .push((ConditionKey::Concern, "intrusion".to_string()));
        let conv = base_conv();

        assert_eq!(
            decide(&u, &conv, false, false, 3),
            AdvisorAction::Clarify {
                missing: vec![ConditionKey::Housing]
            }
        );
    }

    #[test]
    fn answer_when_concern_is_non_intrusion_and_housing_not_required() {
        let mut u = base_understanding();
        u.conditions
            .push((ConditionKey::Concern, "monitoring".to_string()));
        let conv = base_conv();

        assert_eq!(decide(&u, &conv, false, false, 3), AdvisorAction::Answer);
    }

    #[test]
    fn answer_when_clarify_budget_exhausted_even_with_missing_conditions() {
        let u = base_understanding(); // concern も未取得のまま
        let mut conv = base_conv();
        conv.clarify_turns = 3;

        assert_eq!(decide(&u, &conv, false, false, 3), AdvisorAction::Answer);
    }

    #[test]
    fn answer_when_no_conditions_are_missing() {
        let mut u = base_understanding();
        u.conditions
            .push((ConditionKey::Concern, "intrusion".to_string()));
        u.conditions
            .push((ConditionKey::Housing, "detached_owned".to_string()));
        let conv = base_conv();

        assert_eq!(decide(&u, &conv, false, false, 3), AdvisorAction::Answer);
    }

    // --- decide: 優先順の交差ケース ---

    #[test]
    fn emergency_wins_over_lead_interest() {
        let mut u = base_understanding();
        u.emergency = true;
        u.lead_interest = true;
        let conv = base_conv();

        assert_eq!(decide(&u, &conv, false, false, 3), AdvisorAction::Safety);
    }

    #[test]
    fn emergency_wins_over_awaiting_time_pref() {
        // design doc §9 不変条件5(最重要交差): emergency は時間帯受付モード中でも最優先。
        let mut u = base_understanding();
        u.emergency = true;
        let mut conv = base_conv();
        conv.awaiting_time_pref = true;

        assert_eq!(decide(&u, &conv, false, false, 3), AdvisorAction::Safety);
    }

    #[test]
    fn awaiting_time_pref_wins_over_out_of_domain() {
        let mut u = base_understanding();
        u.in_domain = false;
        let mut conv = base_conv();
        conv.awaiting_time_pref = true;

        assert_eq!(
            decide(&u, &conv, false, false, 3),
            AdvisorAction::TimePrefContinue
        );
    }

    #[test]
    fn out_of_domain_wins_over_handoff() {
        let mut u = base_understanding();
        u.in_domain = false;
        u.urtect_support = true;
        let conv = base_conv();

        assert_eq!(
            decide(&u, &conv, false, false, 3),
            AdvisorAction::OutOfDomain
        );
    }

    #[test]
    fn handoff_wins_over_lead_solicit() {
        let mut u = base_understanding();
        u.urtect_support = true;
        u.lead_interest = true;
        let conv = base_conv();

        assert_eq!(decide(&u, &conv, false, false, 3), AdvisorAction::Handoff);
    }

    #[test]
    fn lead_solicit_is_skipped_once_lead_already_requested() {
        let mut u = base_understanding();
        u.lead_interest = true;
        let conv = base_conv();

        // lead_requested = true なので手順5は不成立、後続(手順6: concern 未取得)へ進む。
        assert_eq!(
            decide(&u, &conv, false, true, 3),
            AdvisorAction::Clarify {
                missing: vec![ConditionKey::Concern]
            }
        );
    }

    // --- merge_conditions ---

    #[test]
    fn merge_conditions_overwrites_value_for_the_same_key() {
        let accumulated = vec![(ConditionKey::Housing, "detached_owned".to_string())];
        let observed = vec![(ConditionKey::Housing, "apartment_rented".to_string())];

        let merged = merge_conditions(&accumulated, &observed);

        assert_eq!(
            merged,
            vec![(ConditionKey::Housing, "apartment_rented".to_string())]
        );
    }

    #[test]
    fn merge_conditions_keeps_accumulated_keys_not_observed_this_turn_and_appends_new_ones() {
        let accumulated = vec![(ConditionKey::Concern, "intrusion".to_string())];
        let observed = vec![(ConditionKey::Housing, "apartment_rented".to_string())];

        let merged = merge_conditions(&accumulated, &observed);

        assert_eq!(
            merged,
            vec![
                (ConditionKey::Concern, "intrusion".to_string()),
                (ConditionKey::Housing, "apartment_rented".to_string()),
            ]
        );
    }

    // --- 累積条件の support_case 属性への往復 ---

    #[test]
    fn parse_accumulated_conditions_ignores_missing_and_empty_attrs() {
        let mut attrs = std::collections::HashMap::new();
        attrs.insert("advisor_cond_concern".to_string(), "intrusion".to_string());
        attrs.insert("advisor_cond_housing".to_string(), "".to_string()); // 空文字は未取得扱い
                                                                          // advisor_cond_target / advisor_cond_budget / advisor_cond_install は欠落

        let conditions = parse_accumulated_conditions(&attrs);

        assert_eq!(
            conditions,
            vec![(ConditionKey::Concern, "intrusion".to_string())]
        );
    }

    #[test]
    fn accumulated_condition_updates_omits_absent_keys_instead_of_writing_empty_string() {
        // 修正7(Warning): 以前は 5 キー全てを返し、未取得キーには "" を書いていた。
        // merge_conditions を挟まずに(=契約違反で)この関数へ渡すと、その "" が
        // support_case 上の既存の累積条件を上書き消去してしまう
        // (parse_accumulated_conditions は "" を「未取得」として読むため復元不能)。
        // この関数は追加・上書きのみを行い、未取得キーは戻り値に含めない。
        let conditions = vec![(ConditionKey::Concern, "intrusion".to_string())];

        let updates = accumulated_condition_updates(&conditions);

        assert_eq!(
            updates,
            vec![("advisor_cond_concern".to_string(), "intrusion".to_string())],
            "未取得の4キー(housing/target/budget/install)は書き込み対象に含めないこと"
        );
    }

    // --- 2 ターンにわたる累積の再現(reviewer Critical 指摘) ---

    #[test]
    fn accumulates_conditions_across_two_turns_so_previously_captured_conditions_are_not_reasked() {
        let conv = base_conv();

        // --- 1 ターン目: concern=intrusion のみを観測 ---
        let mut turn1 = base_understanding();
        turn1
            .conditions
            .push((ConditionKey::Concern, "intrusion".to_string()));

        let action1 = decide(&turn1, &conv, false, false, 3);
        assert_eq!(
            action1,
            AdvisorAction::Clarify {
                missing: vec![ConditionKey::Housing]
            },
            "concern=intrusion だけでは housing が未取得なので Clarify になる"
        );

        // 1 ターン目の観測条件を support_case 属性へ書き戻す。
        let mut attrs = std::collections::HashMap::new();
        for (key, value) in accumulated_condition_updates(&turn1.conditions) {
            attrs.insert(key, value);
        }

        // --- 2 ターン目: housing=apartment_rented のみを観測(concern はこのターンの LLM
        //     応答には含まれない — reviewer 指摘の再現条件そのもの) ---
        let accumulated = parse_accumulated_conditions(&attrs);
        let observed_turn2 = vec![(ConditionKey::Housing, "apartment_rented".to_string())];
        let merged = merge_conditions(&accumulated, &observed_turn2);

        let mut turn2 = base_understanding();
        turn2.conditions = merged;

        let action2 = decide(&turn2, &conv, false, false, 3);

        assert_eq!(
            action2,
            AdvisorAction::Answer,
            "累積 merge により concern=intrusion が保持されているため、2 ターン目で Housing を \
             補えば Answer まで進む(累積せずにこのターンの観測だけを見ると、concern が消えた \
             ように見えて再び Clarify{{Concern}} になってしまうのが reviewer 指摘の再現手順)"
        );
    }

    // --- window_fits_business_hours / effective_business_window ---

    fn default_hours() -> BusinessHoursConfig {
        BusinessHoursConfig::default() // mon-fri 10:00-18:00 Asia/Tokyo
    }

    #[test]
    fn window_fits_business_hours_true_when_fully_within_business_hours() {
        let cfg = default_hours();
        let w = PrefWindow {
            days: PrefDays::Weekday,
            start: Some(time("11:00")),
            end: Some(time("15:00")),
        };
        assert!(window_fits_business_hours(&w, &cfg));
    }

    #[test]
    fn window_fits_business_hours_false_when_only_partially_overlapping() {
        let cfg = default_hours(); // 10:00-18:00
        let w = PrefWindow {
            days: PrefDays::Weekday,
            start: Some(time("16:00")),
            end: Some(time("20:00")),
        };
        assert!(!window_fits_business_hours(&w, &cfg));
    }

    #[test]
    fn window_fits_business_hours_true_when_both_bounds_omitted() {
        let cfg = default_hours();
        let w = PrefWindow {
            days: PrefDays::Weekday,
            start: None,
            end: None,
        };
        assert!(window_fits_business_hours(&w, &cfg));
    }

    #[test]
    fn window_fits_business_hours_false_when_end_is_before_business_start() {
        let cfg = default_hours(); // 10:00-18:00
        let w = PrefWindow {
            days: PrefDays::Weekday,
            start: None,              // 省略 → 営業開始(10:00)に寄る
            end: Some(time("09:00")), // 始業前終了
        };
        assert!(!window_fits_business_hours(&w, &cfg));
    }

    #[test]
    fn window_fits_business_hours_false_when_days_do_not_match_configured_business_days() {
        let cfg = default_hours(); // mon-fri のみ
        let w = PrefWindow {
            days: PrefDays::Weekend,
            start: None,
            end: None,
        };
        assert!(!window_fits_business_hours(&w, &cfg));
    }

    // --- effective_business_window: 実効範囲そのものを固定する(修正5) ---

    #[test]
    fn effective_business_window_any_with_both_bounds_omitted_yields_full_business_hours() {
        // 「いつでもいいです」(PrefDays::Any, 両端 None)
        let cfg = default_hours(); // mon-fri 10:00-18:00
        let w = PrefWindow {
            days: PrefDays::Any,
            start: None,
            end: None,
        };
        assert_eq!(
            effective_business_window(&w, &cfg),
            Some((time("10:00"), time("18:00")))
        );
    }

    #[test]
    fn effective_business_window_start_only_extends_to_business_close() {
        // 「17時以降で」
        let cfg = default_hours();
        let w = PrefWindow {
            days: PrefDays::Weekday,
            start: Some(time("17:00")),
            end: None,
        };
        assert_eq!(
            effective_business_window(&w, &cfg),
            Some((time("17:00"), time("18:00")))
        );
    }

    #[test]
    fn effective_business_window_end_only_starts_from_business_open() {
        // 「17時までに」
        let cfg = default_hours();
        let w = PrefWindow {
            days: PrefDays::Weekday,
            start: None,
            end: Some(time("17:00")),
        };
        assert_eq!(
            effective_business_window(&w, &cfg),
            Some((time("10:00"), time("17:00")))
        );
    }

    #[test]
    fn effective_business_window_both_bounds_given_are_returned_as_is() {
        // 「13時から15時」
        let cfg = default_hours();
        let w = PrefWindow {
            days: PrefDays::Weekday,
            start: Some(time("13:00")),
            end: Some(time("15:00")),
        };
        assert_eq!(
            effective_business_window(&w, &cfg),
            Some((time("13:00"), time("15:00")))
        );
    }

    #[test]
    fn effective_business_window_none_when_only_partially_overlapping() {
        let cfg = default_hours(); // 10:00-18:00
        let w = PrefWindow {
            days: PrefDays::Weekday,
            start: Some(time("16:00")),
            end: Some(time("20:00")),
        };
        assert_eq!(effective_business_window(&w, &cfg), None);
    }

    #[test]
    fn effective_business_window_none_when_days_do_not_match_configured_business_days() {
        let cfg = default_hours(); // mon-fri のみ
        let w = PrefWindow {
            days: PrefDays::Weekend,
            start: None,
            end: None,
        };
        assert_eq!(effective_business_window(&w, &cfg), None);
    }

    // --- PrefDays::Any(以前は Weekday / Weekend しかテストされていなかった) ---

    #[test]
    fn any_days_fits_mon_fri_business_hours() {
        let cfg = default_hours(); // mon-fri
        let w = PrefWindow {
            days: PrefDays::Any,
            start: None,
            end: None,
        };
        assert!(window_fits_business_hours(&w, &cfg));
    }

    #[test]
    fn any_days_fits_everyday_business_hours() {
        let mut cfg = default_hours();
        cfg.days = "everyday".to_string();
        let w = PrefWindow {
            days: PrefDays::Any,
            start: None,
            end: None,
        };
        assert!(window_fits_business_hours(&w, &cfg));
        assert_eq!(
            effective_business_window(&w, &cfg),
            Some((time("10:00"), time("18:00")))
        );
    }

    // --- config 破損時の fail closed(hours.rs の同種テストに揃える) ---

    #[test]
    fn effective_business_window_none_when_cfg_days_is_unrecognized() {
        let mut cfg = default_hours();
        cfg.days = "sometimes".to_string();
        let w = PrefWindow {
            days: PrefDays::Weekday,
            start: None,
            end: None,
        };
        assert_eq!(
            effective_business_window(&w, &cfg),
            None,
            "破損した config で確定させてはいけない(fail closed)"
        );
    }

    #[test]
    fn effective_business_window_none_when_cfg_start_is_not_a_time() {
        let mut cfg = default_hours();
        cfg.start = "not-a-time".to_string();
        let w = PrefWindow {
            days: PrefDays::Weekday,
            start: None,
            end: None,
        };
        assert_eq!(
            effective_business_window(&w, &cfg),
            None,
            "破損した config で確定させてはいけない(fail closed)"
        );
    }

    // --- format_confirmed_slot ---

    #[test]
    fn format_confirmed_slot_renders_the_effective_range_for_weekday() {
        let cfg = default_hours();
        assert_eq!(
            format_confirmed_slot(PrefDays::Weekday, (time("13:00"), time("15:00")), &cfg),
            "平日13:00〜15:00"
        );
    }

    #[test]
    fn format_confirmed_slot_renders_the_effective_range_for_weekend() {
        let cfg = default_hours();
        assert_eq!(
            format_confirmed_slot(PrefDays::Weekend, (time("10:00"), time("12:00")), &cfg),
            "土日10:00〜12:00"
        );
    }

    #[test]
    fn format_confirmed_slot_any_renders_as_weekday_when_config_is_mon_fri() {
        let cfg = default_hours(); // mon-fri
        assert_eq!(
            format_confirmed_slot(PrefDays::Any, (time("10:00"), time("18:00")), &cfg),
            "平日10:00〜18:00",
            "顧客が曜日を指定していない場合は実際に連絡できる営業曜日(平日)を提示する"
        );
    }

    #[test]
    fn format_confirmed_slot_any_renders_as_everyday_when_config_is_everyday() {
        let mut cfg = default_hours();
        cfg.days = "everyday".to_string();
        assert_eq!(
            format_confirmed_slot(PrefDays::Any, (time("10:00"), time("18:00")), &cfg),
            "毎日10:00〜18:00"
        );
    }

    // --- 修正5: 判定した実効範囲と顧客に見せる文言が必ず一致することを、実際に生成される
    // 4 パターンで固定する(境界省略側の描画による「判定より広い約束」の回帰防止) ---

    #[test]
    fn format_confirmed_slot_matches_effective_window_for_all_bound_combinations() {
        let cfg = default_hours(); // mon-fri 10:00-18:00

        let cases = [
            // (顧客発話の想定, window, 期待される表示文字列)
            (
                "いつでもいいです",
                PrefWindow {
                    days: PrefDays::Any,
                    start: None,
                    end: None,
                },
                "平日10:00〜18:00",
            ),
            (
                "17時以降で",
                PrefWindow {
                    days: PrefDays::Weekday,
                    start: Some(time("17:00")),
                    end: None,
                },
                "平日17:00〜18:00",
            ),
            (
                "17時までに",
                PrefWindow {
                    days: PrefDays::Weekday,
                    start: None,
                    end: Some(time("17:00")),
                },
                "平日10:00〜17:00",
            ),
            (
                "13時から15時",
                PrefWindow {
                    days: PrefDays::Weekday,
                    start: Some(time("13:00")),
                    end: Some(time("15:00")),
                },
                "平日13:00〜15:00",
            ),
        ];

        for (label, w, expected) in cases {
            let eff = effective_business_window(&w, &cfg)
                .unwrap_or_else(|| panic!("{label}: window must fit business hours"));
            assert_eq!(
                format_confirmed_slot(w.days, eff, &cfg),
                expected,
                "{label}"
            );
        }
    }

    // --- decide_time_pref ---

    fn extraction_false(raw: &str) -> TimePrefExtraction {
        TimePrefExtraction {
            is_time_preference: false,
            windows: Vec::new(),
            raw: raw.to_string(),
        }
    }

    fn extraction_true(windows: Vec<PrefWindow>, raw: &str) -> TimePrefExtraction {
        TimePrefExtraction {
            is_time_preference: true,
            windows,
            raw: raw.to_string(),
        }
    }

    #[test]
    fn decide_time_pref_emergency_wins_even_while_awaiting() {
        let cfg = default_hours();
        let mut u = base_understanding();
        u.emergency = true;
        let mut conv = base_conv();
        conv.awaiting_time_pref = true;
        let extraction = extraction_false("別の話です");

        assert_eq!(
            decide_time_pref(&u, &extraction, &mut conv, &cfg, false, 3),
            AdvisorAction::Safety
        );
        assert!(
            conv.awaiting_time_pref,
            "emergency で早期returnした場合、conv の time_pref state は変更しない"
        );
    }

    #[test]
    fn decide_time_pref_false_extraction_defers_to_normal_flow_and_keeps_awaiting_until_two_in_a_row(
    ) {
        let cfg = default_hours();
        let u = base_understanding(); // concern 未取得 → decide_in_domain_flow は Clarify{Concern}
        let mut conv = base_conv();
        conv.awaiting_time_pref = true;
        let extraction = extraction_false("別の話です");

        let action1 = decide_time_pref(&u, &extraction, &mut conv, &cfg, false, 3);
        assert_eq!(
            action1,
            AdvisorAction::Clarify {
                missing: vec![ConditionKey::Concern]
            },
            "「時間帯の話ではなかった」ので通常の判定フロー(decide_in_domain_flow)の結果が返る"
        );
        assert!(conv.awaiting_time_pref, "1回目では自動解除しない");

        let action2 = decide_time_pref(&u, &extraction, &mut conv, &cfg, false, 3);
        assert_eq!(
            action2,
            AdvisorAction::Clarify {
                missing: vec![ConditionKey::Concern]
            }
        );
        assert!(
            !conv.awaiting_time_pref,
            "2回連続で自動解除する(handle_time_pref の既存挙動)"
        );
    }

    #[test]
    fn decide_time_pref_confirms_with_normalized_slot_when_window_fully_fits_business_hours() {
        let cfg = default_hours();
        let u = base_understanding();
        let mut conv = base_conv();
        conv.awaiting_time_pref = true;
        let w = PrefWindow {
            days: PrefDays::Weekday,
            start: None,
            end: None,
        };
        let extraction = extraction_true(vec![w.clone()], "金曜の17時ごろ");

        let action = decide_time_pref(&u, &extraction, &mut conv, &cfg, false, 3);

        match &action {
            AdvisorAction::LeadConfirmed { slot } => {
                assert_eq!(
                    slot, "平日10:00〜18:00",
                    "slot は完全包含 window から正規化して組み立てる(境界省略側は営業時間の \
                     境界(10:00〜18:00)で補う。被テスト関数自身で期待値を計算する恒真 \
                     アサーションを避けるため、期待文字列はリテラルで書く)"
                );
                assert_ne!(
                    slot, "金曜の17時ごろ",
                    "顧客発話の原文がそのまま漏れていないこと(NG 辞書迂回の防止)"
                );
            }
            other => panic!("expected LeadConfirmed, got {other:?}"),
        }
        assert!(!conv.awaiting_time_pref);
        assert_eq!(
            conv.preferred_contact_time.as_deref(),
            Some("金曜の17時ごろ"),
            "内部メモ(handle_time_pref が書き込んだ値)は原文引用のままで良い"
        );
    }

    #[test]
    fn decide_time_pref_confirmed_slot_produces_the_expected_customer_facing_text_for_all_bound_combinations(
    ) {
        // 修正5: 「顧客へ実際に出る最終文字列」を、decide_time_pref が返した LeadConfirmed.slot
        // を canned::lead_confirmed に通した結果として固定する(これまで 1 件も無かった)。
        let cfg = default_hours(); // mon-fri 10:00-18:00
        let u = base_understanding();

        let cases = [
            (
                PrefWindow {
                    days: PrefDays::Any,
                    start: None,
                    end: None,
                },
                "平日10:00〜18:00ですね、担当者からご連絡します。それまでに気になることが出て\
                 きたら、いつでもここで聞いてくださいね。",
            ),
            (
                PrefWindow {
                    days: PrefDays::Weekday,
                    start: Some(time("17:00")),
                    end: None,
                },
                "平日17:00〜18:00ですね、担当者からご連絡します。それまでに気になることが出て\
                 きたら、いつでもここで聞いてくださいね。",
            ),
            (
                PrefWindow {
                    days: PrefDays::Weekday,
                    start: None,
                    end: Some(time("17:00")),
                },
                "平日10:00〜17:00ですね、担当者からご連絡します。それまでに気になることが出て\
                 きたら、いつでもここで聞いてくださいね。",
            ),
            (
                PrefWindow {
                    days: PrefDays::Weekday,
                    start: Some(time("13:00")),
                    end: Some(time("15:00")),
                },
                "平日13:00〜15:00ですね、担当者からご連絡します。それまでに気になることが出て\
                 きたら、いつでもここで聞いてくださいね。",
            ),
        ];

        for (w, expected_text) in cases {
            let mut conv = base_conv();
            conv.awaiting_time_pref = true;
            let extraction = extraction_true(vec![w], "テスト発話");

            let action = decide_time_pref(&u, &extraction, &mut conv, &cfg, false, 3);
            let AdvisorAction::LeadConfirmed { slot } = action else {
                panic!("expected LeadConfirmed, got {action:?}");
            };
            assert_eq!(crate::advisor::canned::lead_confirmed(&slot), expected_text);
        }
    }

    #[test]
    fn decide_time_pref_reasks_when_window_day_does_not_fit_business_days() {
        let cfg = default_hours(); // mon-fri のみ
        let u = base_understanding();
        let mut conv = base_conv();
        conv.awaiting_time_pref = true;
        let extraction = extraction_true(
            vec![PrefWindow {
                days: PrefDays::Weekend,
                start: None,
                end: None,
            }],
            "土曜の午前中",
        );

        let action = decide_time_pref(&u, &extraction, &mut conv, &cfg, false, 3);

        assert_eq!(action, AdvisorAction::TimePrefContinue);
        assert!(
            conv.awaiting_time_pref,
            "対応可能な希望が出るまで再アームする(handle_time_pref の標準挙動からの逸脱)"
        );
        assert_eq!(
            conv.preferred_contact_time, None,
            "handle_time_pref が一度書き込んだ値をクリアする"
        );
        assert_eq!(conv.time_pref_false_count, 1);
    }

    #[test]
    fn decide_time_pref_reasks_when_window_only_partially_overlaps_business_hours_time_range() {
        let cfg = default_hours(); // mon-fri 10:00-18:00
        let u = base_understanding();
        let mut conv = base_conv();
        conv.awaiting_time_pref = true;
        let extraction = extraction_true(
            vec![PrefWindow {
                days: PrefDays::Weekday,
                start: Some(time("16:00")),
                end: Some(time("20:00")),
            }],
            "16時から20時なら",
        );

        let action = decide_time_pref(&u, &extraction, &mut conv, &cfg, false, 3);

        assert_eq!(
            action,
            AdvisorAction::TimePrefContinue,
            "部分的な重なりだけでは確定させない(完全包含のみ確定させる)"
        );
        assert!(conv.awaiting_time_pref);
    }

    #[test]
    fn decide_time_pref_true_extraction_with_empty_windows_rearms_fail_closed() {
        // is_time_preference=true なのに windows が空という抽出側の不整合(fail-closed 経路)。
        // handle_time_pref 自体がこのケースを「重ならない」として扱うことは time_pref.rs の
        // 既存テストで確認済み。ここでは advisor 側の window_fits_business_hours ベースの
        // ロジックでも同様に re-arm 側に倒れることを確認する。
        let cfg = default_hours();
        let u = base_understanding();
        let mut conv = base_conv();
        conv.awaiting_time_pref = true;
        let extraction = TimePrefExtraction {
            is_time_preference: true,
            windows: Vec::new(),
            raw: "よくわからない時間帯の話".to_string(),
        };

        let action = decide_time_pref(&u, &extraction, &mut conv, &cfg, false, 3);

        assert_eq!(action, AdvisorAction::TimePrefContinue);
        assert!(conv.awaiting_time_pref);
    }

    #[test]
    fn decide_time_pref_gives_up_after_two_consecutive_non_fitting_turns_and_defers_to_normal_flow()
    {
        let cfg = default_hours(); // mon-fri
        let mut u = base_understanding();
        u.conditions
            .push((ConditionKey::Concern, "monitoring".to_string()));
        let mut conv = base_conv();
        conv.awaiting_time_pref = true;
        let extraction = extraction_true(
            vec![PrefWindow {
                days: PrefDays::Weekend,
                start: None,
                end: None,
            }],
            "土曜の午前中",
        );

        let action1 = decide_time_pref(&u, &extraction, &mut conv, &cfg, false, 3);
        assert_eq!(
            action1,
            AdvisorAction::TimePrefContinue,
            "1回目は再アームする"
        );
        assert!(conv.awaiting_time_pref);
        assert_eq!(conv.time_pref_false_count, 1);

        let action2 = decide_time_pref(&u, &extraction, &mut conv, &cfg, false, 3);
        assert_eq!(
            action2,
            AdvisorAction::Answer,
            "2回連続で営業時間に収まらなかったら諦めて通常フローへ委譲する(W3)"
        );
        assert!(!conv.awaiting_time_pref, "打ち切り後は受付モードを解除する");
        assert_eq!(
            conv.time_pref_false_count, 0,
            "打ち切り後はカウンタをリセットする"
        );
    }

    #[test]
    fn decide_time_pref_gives_up_after_two_consecutive_non_fitting_turns_warns_the_lead_loss() {
        // 修正6(Warning): 打ち切りは無言で state を捨てず、運用者が気づけるよう warn する
        // (顧客発話の本文はログに出さない)。
        let cfg = default_hours(); // mon-fri
        let mut u = base_understanding();
        u.conditions
            .push((ConditionKey::Concern, "monitoring".to_string()));
        let mut conv = base_conv();
        conv.awaiting_time_pref = true;
        let extraction = extraction_true(
            vec![PrefWindow {
                days: PrefDays::Weekend,
                start: None,
                end: None,
            }],
            "土曜の午前中",
        );
        decide_time_pref(&u, &extraction, &mut conv, &cfg, false, 3);

        let (action2, logs) =
            capture_logs(|| decide_time_pref(&u, &extraction, &mut conv, &cfg, false, 3));

        assert_eq!(action2, AdvisorAction::Answer);
        assert!(
            logs.contains("WARN"),
            "打ち切りでリードを取りこぼしたことが warn として残らなければならない: {logs}"
        );
        assert!(
            !logs.contains("土曜の午前中"),
            "顧客発話の本文をログへ出してはいけない: {logs}"
        );
    }

    // --- decide_time_pref_extraction_failed ---

    #[test]
    fn decide_time_pref_extraction_failed_first_time_keeps_awaiting_and_continues() {
        let u = base_understanding();
        let mut conv = base_conv();
        conv.awaiting_time_pref = true;

        let action = decide_time_pref_extraction_failed(&u, &mut conv, false, 3);

        assert_eq!(action, AdvisorAction::TimePrefContinue);
        assert!(conv.awaiting_time_pref);
        assert_eq!(conv.time_pref_extraction_error_count, 1);
    }

    #[test]
    fn decide_time_pref_extraction_failed_second_time_keeps_awaiting_and_continues() {
        let u = base_understanding();
        let mut conv = base_conv();
        conv.awaiting_time_pref = true;
        conv.time_pref_extraction_error_count = 1;

        let action = decide_time_pref_extraction_failed(&u, &mut conv, false, 3);

        assert_eq!(action, AdvisorAction::TimePrefContinue);
        assert!(conv.awaiting_time_pref);
        assert_eq!(conv.time_pref_extraction_error_count, 2);
    }

    #[test]
    fn decide_time_pref_extraction_failed_third_time_gives_up_and_defers_to_normal_flow() {
        let mut u = base_understanding();
        u.conditions
            .push((ConditionKey::Concern, "monitoring".to_string()));
        let mut conv = base_conv();
        conv.awaiting_time_pref = true;
        conv.time_pref_extraction_error_count = 2;

        let action = decide_time_pref_extraction_failed(&u, &mut conv, false, 3);

        assert_eq!(action, AdvisorAction::Answer);
        assert!(!conv.awaiting_time_pref);
        assert_eq!(conv.time_pref_extraction_error_count, 0);
        assert_eq!(conv.time_pref_false_count, 0);
    }

    #[test]
    fn decide_time_pref_extraction_failed_third_time_warns_the_lead_loss() {
        // 修正6(Warning): 抽出インフラの 3 回連続失敗による打ち切りも無言で state を捨てない。
        let mut u = base_understanding();
        u.conditions
            .push((ConditionKey::Concern, "monitoring".to_string()));
        let mut conv = base_conv();
        conv.awaiting_time_pref = true;
        conv.time_pref_extraction_error_count = 2;

        let (action, logs) =
            capture_logs(|| decide_time_pref_extraction_failed(&u, &mut conv, false, 3));

        assert_eq!(action, AdvisorAction::Answer);
        assert!(
            logs.contains("WARN"),
            "打ち切りでリードを取りこぼしたことが warn として残らなければならない: {logs}"
        );
    }

    #[test]
    fn decide_time_pref_gives_up_does_not_resume_lead_solicit_even_when_lead_interest_true() {
        // reviewer 指摘 Critical: 打ち切り直後の同一ターンで LeadSolicit を返すと、呼び出し側の
        // 契約(awaiting_time_pref = true / time_pref_false_count = 0 をセット)により受付モードが
        // 即座に再開始し、打ち切りが無意味になる(livelock)。理解 LLM は「お願いします」を含む
        // 発話に lead_interest = true を返しうるため、営業時間外希望を出し続ける顧客がこの
        // 経路を踏む。
        let cfg = default_hours(); // mon-fri
        let mut u = base_understanding();
        u.conditions
            .push((ConditionKey::Concern, "monitoring".to_string()));
        u.lead_interest = true; // 「20時でお願いします」のような発話を理解 LLM が誤検出する想定
        let mut conv = base_conv();
        conv.awaiting_time_pref = true;
        let extraction = extraction_true(
            vec![PrefWindow {
                days: PrefDays::Weekend,
                start: None,
                end: None,
            }],
            "土曜の午前中でお願いします",
        );

        let action1 = decide_time_pref(&u, &extraction, &mut conv, &cfg, false, 3);
        assert_eq!(
            action1,
            AdvisorAction::TimePrefContinue,
            "1回目は再アームする"
        );

        let action2 = decide_time_pref(&u, &extraction, &mut conv, &cfg, false, 3);

        assert_ne!(
            action2,
            AdvisorAction::LeadSolicit,
            "打ち切りターンで LeadSolicit を返すと同一ターンで受付モードを再開始してしまう"
        );
        assert_eq!(
            action2,
            AdvisorAction::Answer,
            "手順5を抑止した結果、手順6(missing なし)を経て手順7 Answer まで進む"
        );
        assert!(
            !conv.awaiting_time_pref,
            "打ち切り後、受付モードが再開始されず解除されたままであること"
        );
    }

    #[test]
    fn decide_time_pref_pass_to_evaluate_does_not_resume_lead_solicit_even_when_lead_interest_true()
    {
        // reviewer 指摘 Critical: PassToEvaluate(「時間帯の話ではなかった」)経路も
        // decide_in_domain_flow へ委譲するため、同じ livelock 経路を持つ。
        let cfg = default_hours();
        let mut u = base_understanding();
        u.conditions
            .push((ConditionKey::Concern, "monitoring".to_string()));
        u.lead_interest = true;
        let mut conv = base_conv();
        conv.awaiting_time_pref = true;
        let extraction = extraction_false("全然関係ない質問です");

        let action = decide_time_pref(&u, &extraction, &mut conv, &cfg, false, 3);

        assert_ne!(action, AdvisorAction::LeadSolicit);
        assert_eq!(action, AdvisorAction::Answer);
    }

    #[test]
    fn decide_time_pref_extraction_failed_third_time_does_not_resume_lead_solicit_even_when_lead_interest_true(
    ) {
        // reviewer 指摘 Critical: 抽出インフラ 3 回連続失敗による打ち切りも同じ livelock 経路。
        let mut u = base_understanding();
        u.conditions
            .push((ConditionKey::Concern, "monitoring".to_string()));
        u.lead_interest = true;
        let mut conv = base_conv();
        conv.awaiting_time_pref = true;
        conv.time_pref_extraction_error_count = 2;

        let action = decide_time_pref_extraction_failed(&u, &mut conv, false, 3);

        assert_ne!(action, AdvisorAction::LeadSolicit);
        assert_eq!(action, AdvisorAction::Answer);
        assert!(!conv.awaiting_time_pref);
    }

    #[test]
    fn decide_time_pref_resets_extraction_error_count_on_successful_extraction() {
        // Warning: api.rs 1018 行目と同じ意味づけ — 抽出に成功したターンでカウンタを 0 に
        // 戻す。これが無いと「失敗→成功→失敗→成功→失敗」のように連続していない失敗でも
        // カウンタが積み上がり、CaseConvState::time_pref_extraction_error_count の doc が
        // 定義する「連続回数」の意味に反して誤って打ち切ってしまう。
        let cfg = default_hours();
        let u = base_understanding();
        let mut conv = base_conv();
        conv.awaiting_time_pref = true;
        conv.time_pref_extraction_error_count = 2;
        let extraction = extraction_false("別の話です");

        decide_time_pref(&u, &extraction, &mut conv, &cfg, false, 3);

        assert_eq!(conv.time_pref_extraction_error_count, 0);
    }

    #[test]
    fn decide_time_pref_extraction_failed_emergency_wins() {
        let mut u = base_understanding();
        u.emergency = true;
        let mut conv = base_conv();
        conv.awaiting_time_pref = true;
        conv.time_pref_extraction_error_count = 2;

        assert_eq!(
            decide_time_pref_extraction_failed(&u, &mut conv, false, 3),
            AdvisorAction::Safety
        );
        assert_eq!(
            conv.time_pref_extraction_error_count, 2,
            "emergency で早期returnした場合、カウンタは変更しない"
        );
    }

    // --- case 属性ヘルパ ---

    #[test]
    fn parse_advisor_case_attrs_defaults_when_missing() {
        let attrs = std::collections::HashMap::new();

        assert_eq!(
            parse_advisor_case_attrs(&attrs),
            AdvisorCaseAttrs::default()
        );
    }

    #[test]
    fn parse_advisor_case_attrs_reads_present_values() {
        let attrs: std::collections::HashMap<String, String> = [
            ("lead_offered".to_string(), "true".to_string()),
            ("lead_requested".to_string(), "true".to_string()),
            (
                "shown_product_cards".to_string(),
                "own_product:adc-v724,partner_product:foo".to_string(),
            ),
        ]
        .into_iter()
        .collect();

        assert_eq!(
            parse_advisor_case_attrs(&attrs),
            AdvisorCaseAttrs {
                lead_offered: true,
                lead_requested: true,
                shown_product_cards: "own_product:adc-v724,partner_product:foo".to_string(),
            }
        );
    }

    #[test]
    fn advisor_attr_updates_serializes_bools_as_true_false_strings() {
        let attrs = AdvisorCaseAttrs {
            lead_offered: true,
            lead_requested: false,
            shown_product_cards: "statistic:foo".to_string(),
        };

        let updates = advisor_attr_updates(&attrs);

        assert_eq!(updates.len(), 3);
        let map: std::collections::HashMap<_, _> = updates.into_iter().collect();
        assert_eq!(map.get("lead_offered").map(String::as_str), Some("true"));
        assert_eq!(map.get("lead_requested").map(String::as_str), Some("false"));
        assert_eq!(
            map.get("shown_product_cards").map(String::as_str),
            Some("statistic:foo")
        );
    }
}

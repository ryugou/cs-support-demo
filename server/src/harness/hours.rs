//! 営業時間の判定（会話フロー v1.1 design doc §6）。
//!
//! config `[api].business_hours` から「現在時刻が営業時間内か」「希望時間帯が営業時間と
//! 重なるか」「営業時間の表記文字列」を導出する。
//!
//! tz 名・"HH:MM" の parse に失敗した場合は **fail-closed で `false`（営業時間外）** を返す。
//! 「営業時間内」と誤って案内し、対応可能と偽ってしまう方が、設定ミスで一時的に
//! 「常に対応時間外」と案内してしまうより有害と判断した（後者は運用者が warn ログに
//! 気づけば即座に気づく形の壊れ方をする）。

use crate::config::BusinessHoursConfig;
use chrono::{DateTime, Datelike, NaiveTime, Timelike, Utc, Weekday};

/// 希望時間帯の曜日区分。`Task 5`（`time_pref.rs`）が抽出した希望時間帯にもこの型を使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefDays {
    Weekday,
    Weekend,
    Any,
}

/// "HH:MM" を `NaiveTime` へ parse する。
pub(crate) fn parse_hm(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s, "%H:%M").ok()
}

/// `cfg.days` と、判定対象の曜日区分が重なるかを判定する。
///
/// `"mon-fri"` / `"everyday"` 以外の値は設定ミスとして fail-closed（重ならない扱い）にし、
/// warn を出す。
fn days_overlap(cfg_days: &str, target: DaysOverlapTarget) -> bool {
    match cfg_days {
        "everyday" => true,
        "mon-fri" => target.is_weekday_compatible(),
        other => {
            tracing::warn!(
                days = other,
                "business_hours.days is neither \"mon-fri\" nor \"everyday\"; treating as no \
                 overlap (fail closed). Fix the config value to one of the two supported days \
                 patterns"
            );
            false
        }
    }
}

/// `days_overlap` が比較する対象を、現在時刻の曜日（`is_within_business_hours`）と希望時間帯の
/// 曜日区分（`overlaps_business_hours`）の両方から共通して扱うための内部橋渡し型。
enum DaysOverlapTarget {
    Weekday(Weekday),
    Pref(PrefDays),
}

impl DaysOverlapTarget {
    fn is_weekday_compatible(&self) -> bool {
        match self {
            DaysOverlapTarget::Weekday(w) => !matches!(w, Weekday::Sat | Weekday::Sun),
            DaysOverlapTarget::Pref(p) => matches!(p, PrefDays::Weekday | PrefDays::Any),
        }
    }
}

/// 現在時刻（UTC）が営業時間内かを判定する。tz 変換・"HH:MM" parse に失敗した場合は
/// fail-closed で `false` を返し、不正な設定値を warn する。
pub fn is_within_business_hours(cfg: &BusinessHoursConfig, now_utc: DateTime<Utc>) -> bool {
    let Ok(tz) = cfg.tz.parse::<chrono_tz::Tz>() else {
        tracing::warn!(
            tz = %cfg.tz,
            "business_hours.tz is not a valid IANA timezone name; treating as out of business \
             hours (fail closed). Fix the config value"
        );
        return false;
    };
    let Some(start) = parse_hm(&cfg.start) else {
        tracing::warn!(
            start = %cfg.start,
            "business_hours.start is not \"HH:MM\"; treating as out of business hours (fail \
             closed). Fix the config value"
        );
        return false;
    };
    let Some(end) = parse_hm(&cfg.end) else {
        tracing::warn!(
            end = %cfg.end,
            "business_hours.end is not \"HH:MM\"; treating as out of business hours (fail \
             closed). Fix the config value"
        );
        return false;
    };

    let local = now_utc.with_timezone(&tz);
    if !days_overlap(&cfg.days, DaysOverlapTarget::Weekday(local.weekday())) {
        return false;
    }
    let t = NaiveTime::from_hms_opt(local.hour(), local.minute(), local.second())
        .expect("hour/minute/second from a valid DateTime must form a valid NaiveTime");
    t >= start && t < end
}

/// 営業時間の表記文字列を組み立てる（例: "平日 10:00〜18:00"）。
pub fn business_hours_label(cfg: &BusinessHoursConfig) -> String {
    let days_label = match cfg.days.as_str() {
        "everyday" => "毎日",
        // "mon-fri" と未知値（設定ミス）はどちらも「平日」表記に倒す。表記は判定のような
        // fail-closed 対象ではなく、既定値である mon-fri が最も無難な表示だから。
        _ => "平日",
    };
    format!("{days_label} {}〜{}", cfg.start, cfg.end)
}

/// 希望時間帯（`days`, `start`, `end`）が営業時間と重なるかを判定する。
///
/// `start`/`end` が両方 `None` の場合は「終日」として時間側の判定を skip し、曜日の重なりのみで
/// 決める。片方だけ `Some` のときの厳密な扱いは Task 5（`time_pref.rs`）の関心事なので、ここでは
/// `unwrap_or` で「全日側」（`None` の境界を最大限広く取る側）に倒す。
pub fn overlaps_business_hours(
    cfg: &BusinessHoursConfig,
    days: PrefDays,
    start: Option<NaiveTime>,
    end: Option<NaiveTime>,
) -> bool {
    let Some(cfg_start) = parse_hm(&cfg.start) else {
        tracing::warn!(
            start = %cfg.start,
            "business_hours.start is not \"HH:MM\"; treating as no overlap (fail closed)"
        );
        return false;
    };
    let Some(cfg_end) = parse_hm(&cfg.end) else {
        tracing::warn!(
            end = %cfg.end,
            "business_hours.end is not \"HH:MM\"; treating as no overlap (fail closed)"
        );
        return false;
    };

    if !days_overlap(&cfg.days, DaysOverlapTarget::Pref(days)) {
        return false;
    }

    if start.is_none() && end.is_none() {
        return true;
    }
    let end_of_day =
        NaiveTime::from_hms_opt(23, 59, 59).expect("23:59:59 is always a valid NaiveTime");
    let pref_start = start.unwrap_or(NaiveTime::MIN);
    let pref_end = end.unwrap_or(end_of_day);
    pref_start < cfg_end && cfg_start < pref_end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_hours() -> BusinessHoursConfig {
        BusinessHoursConfig::default() // mon-fri 10:00-18:00 Asia/Tokyo
    }

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse()
            .expect("test fixture must be a valid RFC3339 timestamp")
    }

    #[test]
    fn boundary_is_half_open() {
        let cfg = default_hours();
        // 2026-08-12 は水曜。JST 10:00 = UTC 01:00
        assert!(is_within_business_hours(&cfg, utc("2026-08-12T01:00:00Z"))); // 10:00 は内
        assert!(!is_within_business_hours(&cfg, utc("2026-08-12T09:00:00Z"))); // 18:00 は外
        assert!(!is_within_business_hours(&cfg, utc("2026-08-15T02:00:00Z"))); // 土曜は外
    }

    #[test]
    fn overlap_untimed_preference_counts_as_all_day() {
        let cfg = default_hours();
        assert!(overlaps_business_hours(&cfg, PrefDays::Weekday, None, None));
        assert!(!overlaps_business_hours(
            &cfg,
            PrefDays::Weekend,
            None,
            None
        )); // mon-fri 設定なら週末は重ならない
    }

    #[test]
    fn everyday_config_overlaps_weekend_preference() {
        let mut cfg = default_hours();
        cfg.days = "everyday".to_string();
        assert!(overlaps_business_hours(&cfg, PrefDays::Weekend, None, None));
        assert!(is_within_business_hours(&cfg, utc("2026-08-15T02:00:00Z"))); // 土曜 11:00 JST
    }

    #[test]
    fn overlap_requires_time_ranges_to_intersect() {
        let cfg = default_hours(); // 10:00-18:00
                                   // 希望「19:00-21:00」は営業終了後なので重ならない。
        let evening_start = NaiveTime::parse_from_str("19:00", "%H:%M").unwrap();
        let evening_end = NaiveTime::parse_from_str("21:00", "%H:%M").unwrap();
        assert!(!overlaps_business_hours(
            &cfg,
            PrefDays::Weekday,
            Some(evening_start),
            Some(evening_end)
        ));
        // 希望「9:00-11:00」は営業開始(10:00)と重なる。
        let morning_start = NaiveTime::parse_from_str("09:00", "%H:%M").unwrap();
        let morning_end = NaiveTime::parse_from_str("11:00", "%H:%M").unwrap();
        assert!(overlaps_business_hours(
            &cfg,
            PrefDays::Weekday,
            Some(morning_start),
            Some(morning_end)
        ));
    }

    #[test]
    fn label_formats_mon_fri_and_everyday() {
        let mut cfg = default_hours();
        assert_eq!(business_hours_label(&cfg), "平日 10:00〜18:00");
        cfg.days = "everyday".to_string();
        assert_eq!(business_hours_label(&cfg), "毎日 10:00〜18:00");
    }

    #[test]
    fn invalid_tz_fails_closed_to_out_of_hours() {
        let mut cfg = default_hours();
        cfg.tz = "Not/A/Timezone".to_string();
        assert!(!is_within_business_hours(&cfg, utc("2026-08-12T01:00:00Z")));
    }

    #[test]
    fn invalid_time_format_fails_closed() {
        let mut cfg = default_hours();
        cfg.start = "not-a-time".to_string();
        assert!(!is_within_business_hours(&cfg, utc("2026-08-12T01:00:00Z")));
        assert!(!overlaps_business_hours(
            &cfg,
            PrefDays::Weekday,
            None,
            None
        ));
    }

    #[test]
    fn unknown_days_value_fails_closed() {
        let mut cfg = default_hours();
        cfg.days = "sometimes".to_string();
        assert!(!is_within_business_hours(&cfg, utc("2026-08-12T01:00:00Z")));
        assert!(!overlaps_business_hours(
            &cfg,
            PrefDays::Weekday,
            None,
            None
        ));
    }
}

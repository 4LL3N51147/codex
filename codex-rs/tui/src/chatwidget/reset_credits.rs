use crate::clock_format::ClockFormat;
use chrono::DateTime;
use chrono::Local;
use chrono::Utc;
use codex_app_server_protocol::RateLimitResetCreditStatus;
use codex_app_server_protocol::RateLimitResetCreditsSummary;

#[derive(Debug, Eq, PartialEq)]
pub(super) struct ResetCreditOption {
    pub(super) credit_id: Option<String>,
    pub(super) name: String,
    pub(super) detail: Option<String>,
    pub(super) description: String,
}

pub(super) fn reset_credit_options(
    summary: &RateLimitResetCreditsSummary,
    clock_format: ClockFormat,
) -> Vec<ResetCreditOption> {
    let available_count = summary.available_count.max(0);
    let detail_limit = usize::try_from(available_count).unwrap_or(usize::MAX);
    let mut available_credits = summary
        .credits
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|credit| credit.status == RateLimitResetCreditStatus::Available)
        .collect::<Vec<_>>();
    available_credits.sort_by_key(|credit| credit.expires_at.unwrap_or(i64::MAX));

    let mut options = available_credits
        .into_iter()
        .take(detail_limit)
        .map(|credit| {
            let expiration = match credit.expires_at {
                Some(expires_at) => DateTime::<Utc>::from_timestamp(expires_at, 0)
                    .map(|expires_at| {
                        let expires_at = expires_at.with_timezone(&Local);
                        format!(
                            "Expires {} on {}",
                            expires_at.format(clock_format.time_format()),
                            expires_at.format("%-d %b %Y")
                        )
                    })
                    .unwrap_or_else(|| "Expiration unavailable".to_string()),
                None => "Does not expire".to_string(),
            };
            let reset_title = credit
                .title
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .unwrap_or("Full reset");
            let reset_description = credit
                .description
                .as_deref()
                .map(str::trim)
                .filter(|description| !description.is_empty())
                .unwrap_or("Reset your current usage limits");
            ResetCreditOption {
                credit_id: Some(credit.id.clone()),
                name: reset_title.to_string(),
                detail: Some(expiration),
                description: reset_description.to_string(),
            }
        })
        .collect::<Vec<_>>();

    if options.is_empty() {
        options.push(ResetCreditOption {
            credit_id: None,
            name: "Full reset".to_string(),
            detail: None,
            description: "Reset your current usage limits".to_string(),
        });
    }

    options
}

/// Earliest usable expiry; missing dates and stale credits cannot trigger a warning.
pub(super) fn next_banked_reset_expiry(
    summary: &RateLimitResetCreditsSummary,
    now: i64,
) -> Option<i64> {
    if summary.available_count <= 0 {
        return None;
    }
    summary
        .credits
        .as_deref()?
        .iter()
        .filter(|credit| credit.status == RateLimitResetCreditStatus::Available)
        .filter_map(|credit| credit.expires_at)
        .filter(|expires_at| *expires_at > now)
        .min()
}

/// Only show the warning when a reset is inside the configured expiry window.
pub(super) fn banked_reset_status(
    summary: &RateLimitResetCreditsSummary,
    now: i64,
    warning_minutes: u64,
) -> Option<String> {
    let expires_at = next_banked_reset_expiry(summary, now)?;
    let seconds = expires_at.saturating_sub(now);
    if warning_minutes == 0 || seconds as u64 > warning_minutes.saturating_mul(60) {
        return None;
    }
    // Round up so a usable reset never appears to have zero time remaining.
    let remaining = if seconds >= 86_400 {
        format!("{}d", (seconds - 1) / 86_400 + 1)
    } else if seconds >= 3_600 {
        format!("{}h", (seconds - 1) / 3_600 + 1)
    } else {
        format!("{}m", (seconds - 1) / 60 + 1)
    };
    Some(format!("Banked reset expires in {remaining} · /usage"))
}

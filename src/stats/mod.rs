pub mod achievements;
pub mod aggregation;
pub mod models;
pub mod reports;
pub mod store;

pub use aggregation::{
    compute_hit_percentages, compute_kd, compute_win_rate, parse_privacy_toggle_event_value,
    persist_share_anonymous_stats, start_analytics, start_analytics_with_config,
    AnalyticsCollector, AnalyticsConfig, AnalyticsHandle, DEFAULT_FIREBASE_COLLECTION,
    DEFAULT_FIREBASE_PROJECT_ID, PRIVACY_SETTING_PATH,
};
pub use models::{
    GameAggregateStats, GamePeriodStats, GameSession, KeybindTelemetrySummary, MatchStatsRecord,
    MonthlyStatsSummary, TelemetryPayload,
};
pub use reports::YearWrappedSummary;
pub use store::{resolve_local_match_stats_path, LocalMatchStatsStore};

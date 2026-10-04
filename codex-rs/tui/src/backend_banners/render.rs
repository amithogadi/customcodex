//! Render backend-owned copy and ordered CTA labels with the shared inline banner UI.

use super::BackendBanner;
use super::BannerPresentation;
use crate::app_event::AppEvent;
use crate::bottom_pane::ActionableBanner;
use crate::bottom_pane::BannerDismissal;
use crate::bottom_pane::SelectionItem;
use crate::clock_format::ClockFormat;
use chrono::DateTime;
use chrono::Local;

impl BackendBanner {
    pub(crate) fn actionable_banner(&self, clock_format: ClockFormat) -> ActionableBanner {
        let reset_time = self
            .reset_at
            .and_then(|timestamp| DateTime::from_timestamp(timestamp, /*nsecs*/ 0))
            .map(|time| {
                crate::status::format_reset_timestamp(
                    time.with_timezone(&Local),
                    Local::now(),
                    clock_format,
                )
            });
        let copy = |text: &str| {
            let text: String = text
                .chars()
                .filter(|c| !c.is_control() || *c == '\n')
                .collect();
            match reset_time.as_deref() {
                Some(reset_time) => text.replace("{time}", reset_time),
                None => text,
            }
        };
        ActionableBanner {
            title: copy(&self.title),
            description: copy(&self.description),
            actions: Vec::new(),
            dismissal: if self.presentation == BannerPresentation::Dismissible {
                BannerDismissal::Dismissible
            } else {
                BannerDismissal::Persistent
            },
            ..Default::default()
        }
    }
}

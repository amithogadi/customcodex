//! In-memory timing totals for the local terminal display.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeMetricTotals {
    pub count: u64,
    pub duration_ms: u64,
}

impl RuntimeMetricTotals {
    pub fn is_empty(self) -> bool {
        self.count == 0 && self.duration_ms == 0
    }

    pub fn merge(&mut self, other: Self) {
        self.count = self.count.saturating_add(other.count);
        self.duration_ms = self.duration_ms.saturating_add(other.duration_ms);
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RuntimeMetricsSummary {
    pub tool_calls: RuntimeMetricTotals,
    pub api_calls: RuntimeMetricTotals,
    pub streaming_events: RuntimeMetricTotals,
    pub websocket_calls: RuntimeMetricTotals,
    pub websocket_events: RuntimeMetricTotals,
    pub responses_api_overhead_ms: u64,
    pub responses_api_inference_time_ms: u64,
    pub responses_api_engine_iapi_ttft_ms: u64,
    pub responses_api_engine_service_ttft_ms: u64,
    pub responses_api_engine_iapi_tbt_ms: f64,
    pub responses_api_engine_service_tbt_ms: f64,
    pub turn_ttft_ms: u64,
    pub turn_ttfm_ms: u64,
}

impl RuntimeMetricsSummary {
    pub fn is_empty(self) -> bool {
        self.tool_calls.is_empty()
            && self.api_calls.is_empty()
            && self.streaming_events.is_empty()
            && self.websocket_calls.is_empty()
            && self.websocket_events.is_empty()
            && self.responses_api_overhead_ms == 0
            && self.responses_api_inference_time_ms == 0
            && self.responses_api_engine_iapi_ttft_ms == 0
            && self.responses_api_engine_service_ttft_ms == 0
            && self.responses_api_engine_iapi_tbt_ms == 0.0
            && self.responses_api_engine_service_tbt_ms == 0.0
            && self.turn_ttft_ms == 0
            && self.turn_ttfm_ms == 0
    }

    pub fn merge(&mut self, other: Self) {
        self.tool_calls.merge(other.tool_calls);
        self.api_calls.merge(other.api_calls);
        self.streaming_events.merge(other.streaming_events);
        self.websocket_calls.merge(other.websocket_calls);
        self.websocket_events.merge(other.websocket_events);
        if other.responses_api_overhead_ms > 0 {
            self.responses_api_overhead_ms = other.responses_api_overhead_ms;
        }
        if other.responses_api_inference_time_ms > 0 {
            self.responses_api_inference_time_ms = other.responses_api_inference_time_ms;
        }
        if other.responses_api_engine_iapi_ttft_ms > 0 {
            self.responses_api_engine_iapi_ttft_ms = other.responses_api_engine_iapi_ttft_ms;
        }
        if other.responses_api_engine_service_ttft_ms > 0 {
            self.responses_api_engine_service_ttft_ms = other.responses_api_engine_service_ttft_ms;
        }
        if other.responses_api_engine_iapi_tbt_ms > 0.0 {
            self.responses_api_engine_iapi_tbt_ms = other.responses_api_engine_iapi_tbt_ms;
        }
        if other.responses_api_engine_service_tbt_ms > 0.0 {
            self.responses_api_engine_service_tbt_ms = other.responses_api_engine_service_tbt_ms;
        }
        if other.turn_ttft_ms > 0 {
            self.turn_ttft_ms = other.turn_ttft_ms;
        }
        if other.turn_ttfm_ms > 0 {
            self.turn_ttfm_ms = other.turn_ttfm_ms;
        }
    }

    pub fn responses_api_summary(&self) -> RuntimeMetricsSummary {
        Self {
            responses_api_overhead_ms: self.responses_api_overhead_ms,
            responses_api_inference_time_ms: self.responses_api_inference_time_ms,
            responses_api_engine_iapi_ttft_ms: self.responses_api_engine_iapi_ttft_ms,
            responses_api_engine_service_ttft_ms: self.responses_api_engine_service_ttft_ms,
            responses_api_engine_iapi_tbt_ms: self.responses_api_engine_iapi_tbt_ms,
            responses_api_engine_service_tbt_ms: self.responses_api_engine_service_tbt_ms,
            ..RuntimeMetricsSummary::default()
        }
    }
}

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

// Bounded process-local display data. This module has no exporter, worker or I/O.
const MAX_THREADS: usize = 256;
static RUNTIME: OnceLock<Mutex<HashMap<String, RuntimeMetricsSummary>>> = OnceLock::new();

pub fn record_runtime_summary(thread_id: &str, summary: RuntimeMetricsSummary) {
    let mut summaries = RUNTIME
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if summaries.len() >= MAX_THREADS && !summaries.contains_key(thread_id) {
        return;
    }
    summaries
        .entry(thread_id.to_owned())
        .or_default()
        .merge(summary);
}

pub fn take_runtime_summary(thread_id: &str) -> Option<RuntimeMetricsSummary> {
    RUNTIME
        .get()?
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(thread_id)
}

#[derive(Clone, Copy)]
pub enum RuntimeOperation {
    Tool,
    Api,
    Websocket,
}

pub struct RuntimeTimer {
    thread_id: String,
    operation: RuntimeOperation,
    started: Instant,
}

impl RuntimeTimer {
    pub fn start(thread_id: impl ToString, operation: RuntimeOperation) -> Self {
        Self {
            thread_id: thread_id.to_string(),
            operation,
            started: Instant::now(),
        }
    }
}

impl Drop for RuntimeTimer {
    fn drop(&mut self) {
        let totals = RuntimeMetricTotals {
            count: 1,
            duration_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        };
        let mut summary = RuntimeMetricsSummary::default();
        match self.operation {
            RuntimeOperation::Tool => summary.tool_calls = totals,
            RuntimeOperation::Api => summary.api_calls = totals,
            RuntimeOperation::Websocket => summary.websocket_calls = totals,
        }
        record_runtime_summary(&self.thread_id, summary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_totals_merge_and_drain_per_thread() {
        let thread_id = "runtime-display-merge-test";
        let other_thread_id = "runtime-display-independent-test";
        let first = RuntimeMetricsSummary {
            tool_calls: RuntimeMetricTotals {
                count: 2,
                duration_ms: 30,
            },
            turn_ttft_ms: 12,
            ..Default::default()
        };
        record_runtime_summary(thread_id, first);
        record_runtime_summary(
            thread_id,
            RuntimeMetricsSummary {
                tool_calls: RuntimeMetricTotals {
                    count: 1,
                    duration_ms: 10,
                },
                ..Default::default()
            },
        );
        record_runtime_summary(other_thread_id, first);
        let summary = take_runtime_summary(thread_id).expect("local timings");
        assert_eq!(
            summary.tool_calls,
            RuntimeMetricTotals {
                count: 3,
                duration_ms: 40
            }
        );
        assert_eq!(summary.turn_ttft_ms, 12);
        assert!(take_runtime_summary(thread_id).is_none());
        assert_eq!(take_runtime_summary(other_thread_id), Some(first));
    }

    #[test]
    fn timer_records_one_local_operation_when_dropped() {
        let thread_id = "runtime-display-timer-test";
        drop(RuntimeTimer::start(thread_id, RuntimeOperation::Api));
        let summary = take_runtime_summary(thread_id).expect("local timer");
        assert_eq!(summary.api_calls.count, 1);
        assert_eq!(summary.tool_calls.count, 0);
    }
}

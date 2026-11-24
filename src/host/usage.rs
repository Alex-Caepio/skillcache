//! Normalized usage/cost component tracking for provider CLI invocations.
use serde_json::{Value, json as object};
use std::collections::{BTreeMap, BTreeSet};

pub fn counter(value: &Value) -> Option<u64> {
    value.as_u64().filter(|value| *value <= i64::MAX as u64)
}

pub fn amount(value: &Value) -> Option<f64> {
    if value.is_i64() || value.is_u64() {
        return counter(value).map(|value| value as f64);
    }
    value
        .as_f64()
        .filter(|value| value.is_finite() && *value >= 0. && *value < 9223372036854775808.)
}

fn add_optional(dst: &mut Option<u64>, src: Option<u64>) {
    if let Some(value) = src {
        *dst = Some(dst.unwrap_or(0).saturating_add(value));
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ComponentTotals {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_write_5m_input_tokens: Option<u64>,
    pub cache_write_1h_input_tokens: Option<u64>,
    pub model_calls: Option<u64>,
}

impl ComponentTotals {
    pub fn add(&mut self, other: &Self) {
        add_optional(&mut self.input_tokens, other.input_tokens);
        add_optional(&mut self.output_tokens, other.output_tokens);
        add_optional(
            &mut self.cache_read_input_tokens,
            other.cache_read_input_tokens,
        );
        add_optional(
            &mut self.cache_write_5m_input_tokens,
            other.cache_write_5m_input_tokens,
        );
        add_optional(
            &mut self.cache_write_1h_input_tokens,
            other.cache_write_1h_input_tokens,
        );
        add_optional(&mut self.model_calls, other.model_calls);
    }

    pub fn to_json(&self, source: Option<&str>) -> Value {
        object!({
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "cache_read_input_tokens": self.cache_read_input_tokens,
            "cache_write_5m_input_tokens": self.cache_write_5m_input_tokens,
            "cache_write_1h_input_tokens": self.cache_write_1h_input_tokens,
            "model_calls": self.model_calls,
            "source": source,
        })
    }
}

pub fn claude_message_components(usage: &Value) -> ComponentTotals {
    let cache = &usage["cache_creation"];
    let mut write_5m = counter(&cache["ephemeral_5m_input_tokens"]);
    let write_1h = counter(&cache["ephemeral_1h_input_tokens"]);
    if write_5m.is_none() && write_1h.is_none() {
        write_5m = counter(&usage["cache_creation_input_tokens"]);
    }
    ComponentTotals {
        input_tokens: counter(&usage["input_tokens"]),
        output_tokens: counter(&usage["output_tokens"]),
        cache_read_input_tokens: counter(&usage["cache_read_input_tokens"]),
        cache_write_5m_input_tokens: write_5m,
        cache_write_1h_input_tokens: write_1h,
        model_calls: Some(1),
    }
}

pub fn codex_turn_components(usage: &Value) -> ComponentTotals {
    let cached = counter(&usage["cached_input_tokens"])
        .or_else(|| counter(&usage["cache_read_input_tokens"]));
    let input =
        counter(&usage["input_tokens"]).map(|total| total.saturating_sub(cached.unwrap_or(0)));
    ComponentTotals {
        input_tokens: input,
        output_tokens: counter(&usage["output_tokens"]),
        cache_read_input_tokens: cached,
        cache_write_5m_input_tokens: None,
        cache_write_1h_input_tokens: None,
        model_calls: None,
    }
}

#[derive(Clone, Debug, Default)]
struct PhaseComponents {
    claude_messages: BTreeMap<String, ComponentTotals>,
    claude_reports: Vec<Option<u64>>,
    codex_totals: ComponentTotals,
    cursor_calls: BTreeSet<String>,
}

#[derive(Clone, Debug)]
pub struct UsageComponents {
    provider: String,
    phases: BTreeMap<String, PhaseComponents>,
}

impl UsageComponents {
    pub fn new(provider: &str) -> Self {
        Self {
            provider: provider.into(),
            phases: BTreeMap::new(),
        }
    }

    pub fn observe(&mut self, phase: &str, event: &Value) {
        if !event.is_object() || !["main", "recovery"].contains(&phase) {
            return;
        }
        let kind = event["type"].as_str().unwrap_or("");
        match self.provider.as_str() {
            "claude" if kind == "assistant" => {
                let message = &event["message"];
                if !message.is_object()
                    || message["role"].as_str().unwrap_or("assistant") != "assistant"
                {
                    return;
                }
                let Some(id) = message["id"].as_str().filter(|id| !id.is_empty()) else {
                    return;
                };
                let usage = &message["usage"];
                if !usage.is_object() {
                    return;
                }
                self.phases
                    .entry(phase.into())
                    .or_default()
                    .claude_messages
                    .insert(id.into(), claude_message_components(usage));
            }
            "claude" if kind == "result" => {
                self.phases
                    .entry(phase.into())
                    .or_default()
                    .claude_reports
                    .push(counter(&event["usage"]["output_tokens"]));
            }
            "codex" if kind == "turn.completed" => {
                let usage = &event["usage"];
                if !usage.is_object() {
                    return;
                }
                self.phases
                    .entry(phase.into())
                    .or_default()
                    .codex_totals
                    .add(&codex_turn_components(usage));
            }
            "cursor" if kind == "assistant" => {
                if let Some(call_id) = event["model_call_id"].as_str().filter(|id| !id.is_empty()) {
                    self.phases
                        .entry(phase.into())
                        .or_default()
                        .cursor_calls
                        .insert(call_id.into());
                }
            }
            _ => {}
        }
    }

    pub fn totals(&self) -> ComponentTotals {
        let mut total = ComponentTotals::default();
        match self.provider.as_str() {
            "claude" => {
                let mut output = Some(0u64);
                for phase in self.phases.values() {
                    for msg in phase.claude_messages.values() {
                        total.add(msg);
                    }
                    output = match (output, phase.claude_reports.as_slice()) {
                        (Some(sum), [Some(reported)]) => sum.checked_add(*reported),
                        _ => None,
                    };
                }
                total.output_tokens = output.filter(|_| !self.phases.is_empty());
            }
            "codex" => {
                for phase in self.phases.values() {
                    total.add(&phase.codex_totals);
                }
            }
            "cursor" => {
                let calls: u64 = self
                    .phases
                    .values()
                    .map(|phase| phase.cursor_calls.len() as u64)
                    .sum();
                if calls > 0 {
                    total.model_calls = Some(calls);
                }
            }
            _ => {}
        }
        total
    }

    pub fn snapshot(&self) -> Value {
        let source = ["claude", "codex", "cursor"]
            .contains(&self.provider.as_str())
            .then_some(self.provider.as_str());
        self.totals().to_json(source)
    }
}

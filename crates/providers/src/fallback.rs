//! Model fallback (MASTER-PLAN §3 #37, qwen `turn.ts:747-761`): when the
//! PRIMARY model's wire fails with a fallback-able error, the turn continues
//! on the next candidate and the switch is recorded as an EVENT the surface
//! can show ("switched to X because Y") — never silent.
//!
//! Fallback-able: `Unauthorized` (the credential does not cover that model)
//! and `RateLimited` (the model's quota, not the task's fault).
//! NOT fallback-able: `Transient` (the retry budget owns it),
//! `ContextLength` (a compaction concern, switching models would hide it),
//! `Permanent` (the task itself is wrong on every model).

use serde::Serialize;

use crate::sampler::{SampleRequest, Sampler, SamplerError};

/// One recorded model switch (#37).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FallbackEvent {
    pub from: String,
    pub to: String,
    pub reason: String,
}

/// Build a sampler per candidate model name (the host's factory — e.g. the
/// openai wire with a different `model`).
pub type SamplerFactory = std::sync::Arc<dyn Fn(&str) -> Box<dyn Sampler> + Send + Sync>;

pub struct FallbackSampler {
    candidates: Vec<String>,
    factory: SamplerFactory,
    events: std::sync::Mutex<Vec<FallbackEvent>>,
}

impl FallbackSampler {
    /// `candidates[0]` is the primary; the rest are tried in order.
    /// Panics-proof: an empty candidate list makes every sample a
    /// `Permanent` error (fail closed — there is no model to ask).
    pub fn new(candidates: Vec<String>, factory: SamplerFactory) -> FallbackSampler {
        FallbackSampler { candidates, factory, events: std::sync::Mutex::new(Vec::new()) }
    }

    /// Drain the switches recorded since the last drain (the loop logs
    /// them and surfaces show the switch).
    pub fn drain_fallback_events(&self) -> Vec<FallbackEvent> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }
}

impl Sampler for FallbackSampler {
    fn sample(&self, request: &SampleRequest) -> Result<crate::sampler::SampleResponse, SamplerError> {
        let mut last_err: Option<SamplerError> = None;
        for (i, model) in self.candidates.iter().enumerate() {
            let sampler = (self.factory)(model);
            match sampler.sample(request) {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    let fallbackable = matches!(
                        e,
                        SamplerError::Unauthorized | SamplerError::RateLimited { .. }
                    ) && i + 1 < self.candidates.len();
                    if fallbackable {
                        let to = self.candidates[i + 1].clone();
                        self.events.lock().unwrap().push(FallbackEvent {
                            from: model.clone(),
                            to: to.clone(),
                            reason: e.to_string(),
                        });
                        last_err = None;
                        continue;
                    }
                    last_err = Some(e);
                    break;
                }
            }
        }
        Err(last_err.unwrap_or(SamplerError::Permanent(
            "no fallback candidates configured".into(),
        )))
    }

    fn drain_fallback_events(&self) -> Vec<FallbackEvent> {
        self.events.lock().unwrap().drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sampler::{SampleResponse, ScriptedModel, ScriptedStep, StopReason, Usage};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn factory_calls(counter: std::sync::Arc<AtomicUsize>) -> SamplerFactory {
        std::sync::Arc::new(move |model: &str| {
            counter.fetch_add(1, Ordering::SeqCst);
            let step = match model {
                "primary" => ScriptedStep {
                    error: Some(SamplerError::Unauthorized),
                    ..Default::default()
                },
                "backup" => ScriptedStep {
                    text: "from the backup".into(),
                    stop_reason: Some(StopReason::EndTurn),
                    usage: Usage { input_tokens: 1, output_tokens: 2 },
                    ..Default::default()
                },
                _ => ScriptedStep {
                    error: Some(SamplerError::Permanent("unknown candidate".into())),
                    ..Default::default()
                },
            };
            Box::new(ScriptedModel::new(vec![step]))
        })
    }

    fn req() -> SampleRequest {
        SampleRequest { messages: vec![], tools: vec![], max_tokens: None, structured_output_schema: None }
    }

    #[test]
    fn unauthorized_on_primary_falls_back_and_records_the_event() {
        let counter = std::sync::Arc::new(AtomicUsize::new(0));
        let s = FallbackSampler::new(
            vec!["primary".into(), "backup".into()],
            factory_calls(std::sync::Arc::clone(&counter)),
        );
        let resp = s.sample(&req()).expect("backup answers");
        assert_eq!(resp.text, "from the backup");
        let events = s.drain_fallback_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].from, "primary");
        assert_eq!(events[0].to, "backup");
        assert!(events[0].reason.contains("401"), "the reason names the wire error");
        assert_eq!(counter.load(Ordering::SeqCst), 2, "both models were built");
        // drained → empty
        assert!(s.drain_fallback_events().is_empty());
    }

    #[test]
    fn transient_is_never_a_fallback() {
        let counter = std::sync::Arc::new(AtomicUsize::new(0));
        let counter2 = std::sync::Arc::clone(&counter);
        let s = FallbackSampler::new(
            vec!["primary".into(), "backup".into()],
            std::sync::Arc::new(move |m: &str| {
                counter2.fetch_add(1, Ordering::SeqCst);
                let step = match m {
                    "primary" => ScriptedStep {
                        error: Some(SamplerError::Transient("blip".into())),
                        ..Default::default()
                    },
                    _ => Default::default(),
                };
                Box::new(ScriptedModel::new(vec![step]))
            }),
        );
        let err = s.sample(&req()).unwrap_err();
        assert!(matches!(err, SamplerError::Transient(_)), "transient stays with the retry budget");
        assert!(s.drain_fallback_events().is_empty(), "no silent model switch");
        assert_eq!(counter.load(Ordering::SeqCst), 1, "the backup was never built");
    }

    #[test]
    fn last_candidate_error_passes_through_without_an_event() {
        let s = FallbackSampler::new(
            vec!["only".into()],
            std::sync::Arc::new(|_: &str| {
                Box::new(ScriptedModel::new(vec![ScriptedStep {
                    error: Some(SamplerError::RateLimited { retry_after_secs: Some(3) }),
                    ..Default::default()
                }]))
            }),
        );
        let err = s.sample(&req()).unwrap_err();
        assert!(matches!(err, SamplerError::RateLimited { .. }));
        assert!(s.drain_fallback_events().is_empty());
    }

    #[test]
    fn empty_candidates_fail_closed() {
        let s = FallbackSampler::new(
            vec![],
            std::sync::Arc::new(|_: &str| -> Box<dyn Sampler> {
                Box::new(ScriptedModel::new(vec![]))
            }),
        );
        assert!(matches!(s.sample(&req()), Err(SamplerError::Permanent(_))));
    }

    #[test]
    fn chain_walks_multiple_candidates_and_records_each_switch() {
        let s = FallbackSampler::new(
            vec!["a".into(), "b".into(), "c".into()],
            std::sync::Arc::new(|m: &str| {
                let step = match m {
                    "c" => ScriptedStep {
                        text: "third time lucky".into(),
                        stop_reason: Some(StopReason::EndTurn),
                        ..Default::default()
                    },
                    _ => ScriptedStep {
                        error: Some(SamplerError::RateLimited { retry_after_secs: None }),
                        ..Default::default()
                    },
                };
                Box::new(ScriptedModel::new(vec![step]))
            }),
        );
        let resp = s.sample(&req()).unwrap();
        assert_eq!(resp.text, "third time lucky");
        let events = s.drain_fallback_events();
        assert_eq!(events.len(), 2, "a→b and b→c");
        assert_eq!(events[0].from, "a");
        assert_eq!(events[0].to, "b");
        assert_eq!(events[1].from, "b");
        assert_eq!(events[1].to, "c");
    }

    #[test]
    fn response_shape_round_trips() {
        // the recovered SampleResponse keeps usage + stop reason intact
        let s = FallbackSampler::new(
            vec!["m".into()],
            std::sync::Arc::new(|_: &str| {
                Box::new(ScriptedModel::new(vec![ScriptedStep {
                    text: "ok".into(),
                    usage: Usage { input_tokens: 7, output_tokens: 9 },
                    ..Default::default()
                }]))
            }),
        );
        let resp: SampleResponse = s.sample(&req()).unwrap();
        assert_eq!(resp.usage.input_tokens, 7);
        assert_eq!(resp.usage.output_tokens, 9);
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
    }
}

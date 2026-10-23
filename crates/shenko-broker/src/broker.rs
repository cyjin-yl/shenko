//! Inference broker — dispatches to Anthropic API or stub. SPEC §13, §47
use tokio::sync::mpsc;
use tracing::{info, warn};
use anyhow::Result;

use crate::admission::AdmissionController;
use crate::provider::AnthropicProvider;
use crate::request::{InferenceRequest, InferenceResult, InferenceStatus, InferenceResponse};

#[derive(Clone)]
pub struct BrokerHandle { tx: mpsc::UnboundedSender<InferenceRequest> }

impl BrokerHandle {
    pub fn submit(&self, req: InferenceRequest) -> Result<()> {
        self.tx.send(req).map_err(|e| anyhow::anyhow!("broker: {e}"))
    }
}

pub struct Broker {
    rx: mpsc::UnboundedReceiver<InferenceRequest>,
    admission: AdmissionController,
    provider: Option<AnthropicProvider>,
}

impl Broker {
    pub fn new() -> (Self, BrokerHandle) {
        let (tx, rx) = mpsc::unbounded_channel();
        let provider = AnthropicProvider::from_env();
        if provider.is_some() {
            info!("Anthropic API key found — real inference enabled");
        } else {
            info!("No ANTHROPIC_API_KEY — stub mode");
        }
        (Self { rx, admission: AdmissionController::nominal(), provider }, BrokerHandle { tx })
    }

    pub async fn run(mut self) {
        info!("inference broker running");
        while let Some(req) = self.rx.recv().await {
            let _guard = match self.admission.try_admit() {
                Some(g) => g,
                None => {
                    let _ = req.result_tx.send(InferenceResult {
                        request_id: req.request_id,
                        status: InferenceStatus::QueuedCapacityWait,
                        response: None,
                    });
                    continue;
                }
            };

            let result = match &self.provider {
                Some(provider) => match provider.complete(&req.payload).await {
                    Ok(response) => InferenceResult {
                        request_id: req.request_id,
                        status: InferenceStatus::Success,
                        response: Some(response),
                    },
                    Err(e) => {
                        warn!(err=%e, "inference failed");
                        InferenceResult { request_id: req.request_id, status: InferenceStatus::Failed, response: None }
                    }
                },
                None => InferenceResult {
                    request_id: req.request_id,
                    status: InferenceStatus::Success,
                    response: Some(InferenceResponse {
                        content: "[stub: set ANTHROPIC_API_KEY]".to_string(),
                        stop_reason: "end_turn".to_string(),
                        usage: None,
                    }),
                },
            };
            let _ = req.result_tx.send(result);
        }
        info!("inference broker stopped");
    }
}

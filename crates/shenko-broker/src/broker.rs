use tokio::sync::mpsc;
use tracing::info;
use anyhow::Result;
use crate::admission::AdmissionController;
use crate::request::{InferenceRequest, InferenceResult, InferenceStatus, InferenceResponse};

#[derive(Clone)]
pub struct BrokerHandle { tx: mpsc::UnboundedSender<InferenceRequest> }

impl BrokerHandle {
    pub fn submit(&self, req: InferenceRequest) -> Result<()> { self.tx.send(req).map_err(|e| anyhow::anyhow!("broker: {e}")) }
}

pub struct Broker { rx: mpsc::UnboundedReceiver<InferenceRequest>, admission: AdmissionController }

impl Broker {
    pub fn new() -> (Self, BrokerHandle) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { rx, admission: AdmissionController::nominal() }, BrokerHandle { tx })
    }
    pub async fn run(mut self) {
        info!("inference broker running");
        while let Some(req) = self.rx.recv().await {
            let result = if self.admission.try_admit().is_some() {
                InferenceResult { request_id: req.request_id, status: InferenceStatus::Success, response: Some(InferenceResponse { content: String::new(), stop_reason: "end_turn".to_string(), usage: None }) }
            } else {
                InferenceResult { request_id: req.request_id, status: InferenceStatus::QueuedCapacityWait, response: None }
            };
            let _ = req.result_tx.send(result);
        }
        info!("inference broker stopped");
    }
}

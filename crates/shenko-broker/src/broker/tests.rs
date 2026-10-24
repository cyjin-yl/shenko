use super::*;
use crate::request::InferencePayload;
use shenko_types::ids::ControllerEpoch;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::oneshot;

struct Started {
    id: EntityId,
    priority: InferencePriority,
    finish: oneshot::Sender<ProviderOutcome>,
}

struct ControlledProvider {
    started: mpsc::UnboundedSender<Started>,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

struct ActiveCall(Arc<AtomicUsize>);
impl Drop for ActiveCall {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl InferenceProvider for ControlledProvider {
    async fn complete(&self, req: &InferenceRequest) -> ProviderOutcome {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        let _call = ActiveCall(self.active.clone());
        self.peak.fetch_max(active, Ordering::SeqCst);
        let (finish, result) = oneshot::channel();
        self.started
            .send(Started {
                id: req.request_id,
                priority: req.priority,
                finish,
            })
            .unwrap();
        result.await.unwrap_or(ProviderOutcome::Unknown)
    }
}

fn mock() -> (Arc<ControlledProvider>, mpsc::UnboundedReceiver<Started>) {
    let (started, rx) = mpsc::unbounded_channel();
    (
        Arc::new(ControlledProvider {
            started,
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
        }),
        rx,
    )
}

fn request(priority: InferencePriority) -> (InferenceRequest, oneshot::Receiver<InferenceResult>) {
    let (result_tx, rx) = oneshot::channel();
    (
        InferenceRequest {
            request_id: EntityId::new(),
            agent_id: EntityId::new(),
            conversation_id: EntityId::new(),
            model: "isolated-mock".into(),
            priority,
            epoch: ControllerEpoch(1),
            payload: InferencePayload {
                system_prompt: String::new(),
                messages: vec![],
                max_tokens: Some(8),
                tools: vec![],
            },
            result_tx,
        },
        rx,
    )
}

fn success() -> ProviderOutcome {
    ProviderOutcome::Terminal(Some(InferenceResponse {
        content: "隔离测试结果".into(),
        stop_reason: "end_turn".into(),
        usage: None,
    }))
}

// 超时仅用于测试卡住时退出；顺序、公平和并发断言全靠显式握手/轮次。
async fn started(rx: &mut mpsc::UnboundedReceiver<Started>) -> Started {
    tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn concurrent_dispatch_keeps_capacity_until_terminal_or_unknown() {
    let (provider, mut starts) = mock();
    let (broker, handle) = Broker::with_provider(BrokerConfig::new(2).unwrap(), provider.clone());
    let (first, first_result) = request(InferencePriority::Normal);
    let first_id = first.request_id;
    let (second, mut second_result) = request(InferencePriority::Normal);
    let second_id = second.request_id;
    let (third, mut third_result) = request(InferencePriority::Normal);
    let third_id = third.request_id;
    handle.submit(first).unwrap();
    handle.submit(second).unwrap();
    handle.submit(third).unwrap();
    let task = tokio::spawn(broker.run());
    let a = started(&mut starts).await;
    let b = started(&mut starts).await;
    // 同级派发按 FIFO；任务实际开始的顺序不依赖 executor 调度。
    assert_eq!(
        [a.id, b.id]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        [first_id, second_id].into_iter().collect()
    );
    assert_eq!(provider.active.load(Ordering::SeqCst), 2);
    assert_eq!(handle.snapshot().held, 2);
    assert!(starts.try_recv().is_err());
    assert!(matches!(
        third_result.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));

    let (first_call, second_call) = if a.id == first_id { (a, b) } else { (b, a) };
    assert!(first_call.finish.send(success()).is_ok());
    assert_eq!(first_result.await.unwrap().status, InferenceStatus::Success);
    let c = started(&mut starts).await;
    assert_eq!(c.id, third_id);
    assert_eq!(provider.active.load(Ordering::SeqCst), 2);
    assert_eq!(provider.peak.load(Ordering::SeqCst), 2);

    // 本地 provider future 已结束，不代表远端停止；两份 unknown 仍占满 C。
    assert!(second_call.finish.send(ProviderOutcome::Unknown).is_ok());
    assert!(c.finish.send(ProviderOutcome::Unknown).is_ok());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while handle
            .snapshot()
            .requests
            .values()
            .filter(|&&s| s == InFlightState::TerminationUnknown)
            .count()
            != 2
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (fourth, mut fourth_result) = request(InferencePriority::Urgent);
    handle.submit(fourth).unwrap();
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert_eq!(handle.snapshot().held, 2);
    assert_eq!(
        handle.snapshot().requests[&second_id],
        InFlightState::TerminationUnknown
    );
    assert!(starts.try_recv().is_err());
    assert!(matches!(
        second_result.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert!(matches!(
        third_result.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert!(matches!(
        fourth_result.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(handle.snapshot().held, 2, "本地 abort 不得冒充远端静止");
}

#[tokio::test]
async fn single_slot_continuous_subject_and_children_make_bounded_progress() {
    let (provider, mut starts) = mock();
    let config = BrokerConfig::new(1).unwrap();
    let (broker, handle) = Broker::with_provider(config, provider.clone());
    let (subject, subject_result) = request(InferencePriority::Subject);
    handle.submit(subject).unwrap();
    let task = tokio::spawn(broker.run());
    let mut current = started(&mut starts).await;
    assert_eq!(current.priority, InferencePriority::Subject);
    let mut results = vec![subject_result];
    let mut children = VecDeque::new();
    for _ in 0..3 {
        let (child, result) = request(InferencePriority::Low);
        children.push_back(child.request_id);
        handle.submit(child).unwrap();
        results.push(result);
    }
    let mut subject_streak = 1;
    let mut child_count = 0;
    // 每轮先登记主体后继，使主体始终 ready；不能靠主体队列耗尽让 child 前进。
    for _ in 0..9 {
        let (next_subject, result) = request(InferencePriority::Subject);
        handle.submit(next_subject).unwrap();
        results.push(result);
        assert!(current.finish.send(success()).is_ok());
        current = started(&mut starts).await;
        if current.priority == InferencePriority::Subject {
            subject_streak += 1;
            if !children.is_empty() {
                assert!(subject_streak <= 2);
            }
        } else {
            subject_streak = 0;
            assert_eq!(Some(current.id), children.pop_front(), "child 同级 FIFO");
            child_count += 1;
        }
        if children.is_empty() {
            break;
        }
    }
    assert_eq!(child_count, 3);
    assert_eq!(provider.peak.load(Ordering::SeqCst), 1);
    drop(provider);
    assert!(current.finish.send(success()).is_ok());
    drop(handle);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(call) = starts.recv().await {
            assert!(call.finish.send(success()).is_ok());
        }
        task.await.unwrap();
    })
    .await
    .unwrap();
    for result in results {
        assert_eq!(result.await.unwrap().status, InferenceStatus::Success);
    }

    // 同一公平轨迹的纯队列检查：紧急流仍优先，但不能让老 child 无限等待。
    let mut queue = ReadyQueue::new(config);
    let (low, _result) = request(InferencePriority::Low);
    let low_id = low.request_id;
    queue.push(low);
    let mut admitted = false;
    for round in 0..=3 * config.aging_rounds.get() {
        let (urgent, _result) = request(InferencePriority::Urgent);
        queue.push(urgent);
        let selected = queue.pop().unwrap();
        if round == 0 {
            assert_eq!(selected.priority, InferencePriority::Urgent);
        }
        if selected.request_id == low_id {
            admitted = true;
            break;
        }
    }
    assert!(admitted, "派发轮次 aging 必须给老 child 有限等待界限");
}

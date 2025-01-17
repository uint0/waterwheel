use super::{RUNNING_TASKS, TOTAL_TASKS, WORKER_ID};
use crate::{
    instrumented,
    messages::{TaskProgress, TaskRequest, TokenState},
    worker::{config_cache, Worker},
};
use anyhow::Result;
use cadence::{CountedExt, Gauged};
use chrono::{DateTime, Utc};
use futures::{FutureExt, TryStreamExt};
use lapin::{
    options::{
        BasicAckOptions, BasicConsumeOptions, BasicPublishOptions, BasicQosOptions,
        ExchangeDeclareOptions, QueueBindOptions, QueueDeclareOptions,
    },
    types::FieldTable,
    BasicProperties, Channel, Consumer, ExchangeKind,
};
use std::{sync::Arc, time::Duration};
use tracing::{debug, error, info, info_span};

// TODO - queues should be configurable for task routing
const TASK_QUEUE: &str = "waterwheel.tasks";

const RESULT_EXCHANGE: &str = "waterwheel.results";
const RESULT_QUEUE: &str = "waterwheel.results";

const DEFAULT_TASK_HEARTBEAT: Duration = Duration::from_secs(60);

pub async fn setup_queues(chan: &Channel) -> Result<()> {
    // declare queue for consuming incoming messages
    let mut args = FieldTable::default();
    args.insert("x-max-priority".into(), 3.into());

    chan.queue_declare(
        TASK_QUEUE,
        QueueDeclareOptions {
            durable: true,
            ..QueueDeclareOptions::default()
        },
        args,
    )
    .await?;

    // declare outgoing exchange and queue for progress reports
    chan.exchange_declare(
        RESULT_EXCHANGE,
        ExchangeKind::Direct,
        ExchangeDeclareOptions {
            durable: true,
            ..ExchangeDeclareOptions::default()
        },
        FieldTable::default(),
    )
    .await?;

    chan.queue_declare(
        RESULT_QUEUE,
        QueueDeclareOptions {
            durable: true,
            ..QueueDeclareOptions::default()
        },
        FieldTable::default(),
    )
    .await?;

    chan.queue_bind(
        RESULT_QUEUE,
        RESULT_EXCHANGE,
        "",
        QueueBindOptions::default(),
        FieldTable::default(),
    )
    .await?;

    Ok(())
}

pub async fn create_consumer(chan: &Channel) -> Result<Consumer> {
    chan.basic_qos(1, BasicQosOptions::default()).await?;

    let consumer = chan
        .basic_consume(
            TASK_QUEUE,
            "worker",
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await?;

    Ok(consumer)
}

pub async fn process_work(worker: Arc<Worker>) -> Result<!> {
    let statsd = worker.statsd.clone();

    let engine = worker.config.task_engine.get_impl()?;
    let task_timeout = Duration::from_secs(worker.config.default_task_timeout_secs);

    let chan = worker.amqp_conn.create_channel().await?;
    setup_queues(&chan).await?;
    let mut consumer = create_consumer(&chan).await?;

    debug!("worker consuming messages");
    while let Some(delivery) = consumer.try_next().await? {
        let task_req: TaskRequest = serde_json::from_slice(&delivery.data)?;

        let span = info_span!("running_task",
        task_run_id=?task_req.task_run_id,
        task_id=?task_req.task_id,
        trigger_datetime=?task_req.trigger_datetime.to_rfc3339(),
        );

        instrumented!(span, {
            info!("received task");

            let maybe_task_def = config_cache::get_task_def(&worker, task_req.task_id).await?;

            let running_task_guard = RUNNING_TASKS.boost();
            statsd
                .gauge_with_tags("tasks.running", RUNNING_TASKS.get() as u64)
                .with_tag("worker_id", &WORKER_ID.to_string())
                .send();
            statsd
                .incr_with_tags("tasks.received")
                .with_tag("worker_id", &WORKER_ID.to_string())
                .send();

            let started_datetime = Utc::now();
            publish_progress(
                &chan,
                &task_req,
                started_datetime,
                None,
                TokenState::Running,
            )
            .await?;

            delivery.ack(BasicAckOptions::default()).await?;
            debug!("task acked");

            let result = if let Some(task_def) = maybe_task_def {
                if task_def.image.is_some() {
                    let mut task = engine.run_task(&worker, task_req.clone(), task_def).boxed();

                    let mut ticker = tokio::time::interval(DEFAULT_TASK_HEARTBEAT);
                    let mut timeout = tokio::time::sleep(task_timeout).boxed();

                    loop {
                        tokio::select! {
                            _ = &mut timeout => {
                                error!("timeout running task");
                                break TokenState::Error;
                            }
                            _ = ticker.tick() => {
                                publish_progress(
                                    &chan,
                                    &task_req,
                                    started_datetime,
                                    None,
                                    TokenState::Running,
                                ).await?;
                            }
                            result = &mut task => {
                                break match result {
                                    Ok(true) => TokenState::Success,
                                    Ok(false) => TokenState::Failure,
                                    Err(err) => {
                                        error!("failed to run task: {:#}", err);
                                        TokenState::Error
                                    }
                                }
                            }
                        }
                    }
                } else {
                    // task has no image, mark success immediately
                    TokenState::Success
                }
            } else {
                TokenState::Error
            };

            let finished_datetime = Utc::now();

            TOTAL_TASKS.inc();
            drop(running_task_guard);

            statsd
                .gauge_with_tags("tasks.running", RUNNING_TASKS.get() as u64)
                .with_tag("worker_id", &WORKER_ID.to_string())
                .send();
            statsd
                .incr_with_tags("tasks.total")
                .with_tag("worker_id", &WORKER_ID.to_string())
                .with_tag("result", result.as_ref())
                .send();

            info!(result=result.as_ref(),
                started_datetime=?started_datetime.to_rfc3339(),
                "task completed");

            publish_progress(
                &chan,
                &task_req,
                started_datetime,
                Some(finished_datetime),
                result,
            )
            .await?;
        })?;
    }

    unreachable!("consumer stopped consuming")
}

async fn publish_progress(
    chan: &Channel,
    task_req: &TaskRequest,
    started_datetime: DateTime<Utc>,
    finished_datetime: Option<DateTime<Utc>>,
    result: TokenState,
) -> Result<()> {
    let payload = serde_json::to_vec(&TaskProgress {
        task_run_id: task_req.task_run_id,
        task_id: task_req.task_id,
        trigger_datetime: task_req.trigger_datetime,
        started_datetime,
        finished_datetime,
        worker_id: *WORKER_ID,
        result,
    })?;

    chan.basic_publish(
        RESULT_EXCHANGE,
        "",
        BasicPublishOptions::default(),
        &payload,
        BasicProperties::default(),
    )
    .await?;

    debug!(result=?result, "task result published");

    Ok(())
}

use crate::amqp;
use crate::db;
use crate::messages::TaskResult;
use crate::server::tokens::{increment_token, Token};
use async_std::sync::Sender;
use futures::TryStreamExt;
use lapin::options::{BasicAckOptions, BasicConsumeOptions, QueueDeclareOptions};
use lapin::types::FieldTable;
use log::{debug, info};
use sqlx::{types::Uuid, Connection};

const RESULT_QUEUE: &str = "waterwheel.results";

pub async fn process_progress(token_tx: Sender<Token>) -> anyhow::Result<!> {
    let pool = db::get_pool();
    let chan = amqp::get_amqp_channel().await?;

    // declare queue for consuming incoming messages
    chan.queue_declare(
        RESULT_QUEUE,
        QueueDeclareOptions {
            durable: true,
            ..QueueDeclareOptions::default()
        },
        FieldTable::default(),
    )
    .await?;

    let mut consumer = chan
        .basic_consume(
            RESULT_QUEUE,
            "server",
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await?;

    while let Some((chan, msg)) = consumer.try_next().await? {
        let task_result: TaskResult = serde_json::from_slice(&msg.data)?;
        info!("received task results: {:?}", task_result);

        let parent_token = task_result.get_token()?;

        let mut cursor = sqlx::query_as::<_, (Uuid,)>(
            "SELECT child_task_id
            FROM task_edge
            WHERE parent_task_id = $1",
        )
        .bind(&parent_token.task_id)
        .fetch(&pool);

        let mut conn = pool.acquire().await?;
        let mut txn = conn.begin().await?;
        let mut tokens_to_tx = Vec::new();

        while let Some((child_task_id,)) = cursor.try_next().await? {
            let token = Token {
                task_id: child_task_id,
                trigger_datetime: parent_token.trigger_datetime,
            };

            increment_token(&mut txn, &token).await?;
            tokens_to_tx.push(token);
        }

        sqlx::query(
            "UPDATE token
            SET state = 'success'
            WHERE task_id = $1
            AND trigger_datetime = $2",
        )
        .bind(&parent_token.task_id)
        .bind(&parent_token.trigger_datetime)
        .execute(&mut txn)
        .await?;

        txn.commit().await?;

        chan.basic_ack(msg.delivery_tag, BasicAckOptions::default())
            .await?;
        debug!("finished processing task results");

        // after committing the transaction we can tell the token processor to check thresholds
        for token in tokens_to_tx {
            token_tx.send(token).await;
        }
    }

    unreachable!("consumer stopped consuming")
}
/*
 * Author: Ruben Fiszel
 * Copyright: Windmill Labs, Inc 2022
 * This file and its contents are licensed under the AGPLv3 License.
 * Please see the included NOTICE for copyright information and
 * LICENSE-AGPL for a copy of the license.
 */

use std::{collections::HashMap, vec};

use async_recursion::async_recursion;
use itertools::Itertools;
use reqwest::Client;
use rsmq_async::RsmqConnection;
use serde_json::json;
use sqlx::{Pool, Postgres, Transaction};
use tracing::{instrument, Instrument};
use ulid::Ulid;
use uuid::Uuid;
use windmill_audit::{audit_log, ActionKind};
use windmill_common::{
    error::{self, Error},
    flow_status::{
        FlowStatus, FlowStatusModule, JobResult, MAX_RETRY_ATTEMPTS, MAX_RETRY_INTERVAL,
    },
    flows::{FlowModule, FlowModuleValue, FlowValue},
    jobs::{get_payload_tag_from_prefixed_path, JobKind, JobPayload, Metrics, QueuedJob, RawCode},
    schedule::{schedule_to_user, Schedule},
    scripts::{ScriptHash, ScriptLang},
    users::username_to_permissioned_as,
    METRICS_ENABLED,
};

use crate::{
    schedule::{get_schedule_opt, push_scheduled_job},
    QueueTransaction,
};

lazy_static::lazy_static! {
    pub static ref HTTP_CLIENT: Client = reqwest::ClientBuilder::new()
        .user_agent("windmill/beta")
        .build().unwrap();

    // TODO: these aren't synced, they should be moved into the queue abstraction once/if that happens.
    static ref QUEUE_PUSH_COUNT: prometheus::IntCounter = prometheus::register_int_counter!(
        "queue_push_count",
        "Total number of jobs pushed to the queue."
    )
    .unwrap();
    static ref QUEUE_DELETE_COUNT: prometheus::IntCounter = prometheus::register_int_counter!(
        "queue_delete_count",
        "Total number of jobs deleted from the queue."
    )
    .unwrap();
    static ref QUEUE_PULL_COUNT: prometheus::IntCounter = prometheus::register_int_counter!(
        "queue_pull_count",
        "Total number of jobs pulled from the queue."
    )
    .unwrap();
    pub static ref CLOUD_HOSTED: bool = std::env::var("CLOUD_HOSTED").is_ok();

    pub static ref ACCEPTED_TAGS: Vec<String> = std::env::var("WORKER_TAGS")
        .ok()
        .map(|x| x.split(',').map(|x| x.to_string()).collect())
        .unwrap_or_else(|| vec![
            "deno".to_string(),
            "python3".to_string(),
            "go".to_string(),
            "bash".to_string(),
            "postgresql".to_string(),
            "dependency".to_string(),
            "flow".to_string(),
            "hub".to_string(),
            "other".to_string()]);

    pub static ref ACCEPTED_TAGS_FILTER: String = format!(" AND ({})",
        ACCEPTED_TAGS.clone().into_iter().map(|x| format!("(tag = '{x}')")).join(" OR "));
}

const MAX_FREE_EXECS: i32 = 1000;
const MAX_FREE_CONCURRENT_RUNS: i32 = 15;
const RSMQ_MAIN_QUEUE: &'static str = "main_queue";

#[async_recursion]
pub async fn cancel_job<'c: 'async_recursion>(
    username: &str,
    reason: Option<String>,
    id: Uuid,
    w_id: &str,
    mut tx: Transaction<'c, Postgres>,
    db: &Pool<Postgres>,
    rsmq: Option<rsmq_async::MultiplexedRsmq>,
    force_cancel: bool,
) -> error::Result<(Transaction<'c, Postgres>, Option<Uuid>)> {
    let job_running = get_queued_job(id, &w_id, &mut tx).await?;

    if job_running.is_none() {
        return Ok((tx, None));
    }
    let job_running = job_running.unwrap();
    if job_running.running && !force_cancel {
        sqlx::query!(
        "UPDATE queue SET  canceled = true, canceled_by = $1, canceled_reason = $2, scheduled_for = now(), suspend = 0 WHERE id = $3 \
         AND workspace_id = $4 ",
        username,
        reason,
        id,
        w_id
    )
    .execute(&mut tx)
    .await?;
    } else {
        let reason = reason
            .clone()
            .unwrap_or_else(|| "No reason provided".to_string());
        let add_job = add_completed_job_error(
            &db,
            &job_running,
            format!("canceled by {username}: (force cancel: {force_cancel}"),
            serde_json::json!({"message": format!("Job canceled: {reason} by {username}"), "name": "Canceled", "reason": reason, "canceler": username}),
            None,
            rsmq.clone(),
        )
        .await;
        if let Err(e) = add_job {
            tracing::error!("Failed to add canceled job: {}", e);
        }
    }
    if let Some(mut rsmq) = rsmq.clone() {
        rsmq.change_message_visibility(RSMQ_MAIN_QUEUE, &id.to_string(), 0)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
    }

    let mut jobs = vec![id];
    let mut jobs_to_cancel = vec![];
    while !jobs.is_empty() {
        let p_job = jobs.pop();
        let new_jobs = sqlx::query_scalar!(
            "SELECT id FROM queue WHERE parent_job = $1 AND workspace_id = $2",
            p_job,
            w_id
        )
        .fetch_all(&mut tx)
        .await?;
        jobs.extend(new_jobs.clone());
        jobs_to_cancel.extend(new_jobs);
    }
    for job in jobs_to_cancel {
        let (ntx, _) = cancel_job(
            username,
            reason.clone(),
            job,
            w_id,
            tx,
            db,
            rsmq.clone(),
            force_cancel,
        )
        .await?;
        tx = ntx;
    }
    Ok((tx, Some(id)))
}

#[instrument(level = "trace", skip_all)]
pub async fn add_completed_job_error<R: rsmq_async::RsmqConnection + Clone + Send>(
    db: &Pool<Postgres>,
    queued_job: &QueuedJob,
    logs: String,
    e: serde_json::Value,
    metrics: Option<Metrics>,
    rsmq: Option<R>,
) -> Result<serde_json::Value, Error> {
    if *METRICS_ENABLED {
        metrics.map(|m| m.worker_execution_failed.inc());
    }
    let result = serde_json::json!({ "error": e });
    let _ = add_completed_job(db, &queued_job, false, false, result.clone(), logs, rsmq).await?;
    Ok(result)
}

fn flatten_jobs(modules: Vec<FlowStatusModule>) -> Vec<Uuid> {
    modules
        .into_iter()
        .filter_map(|m| match m {
            FlowStatusModule::Success { job, flow_jobs, .. }
            | FlowStatusModule::Failure { job, flow_jobs, .. } => {
                if let Some(flow_jobs) = flow_jobs {
                    Some(flow_jobs)
                } else {
                    Some(vec![job])
                }
            }
            _ => None,
        })
        .flatten()
        .collect::<Vec<_>>()
}

#[instrument(level = "trace", skip_all)]
pub async fn add_completed_job<R: rsmq_async::RsmqConnection + Clone + Send>(
    db: &Pool<Postgres>,
    queued_job: &QueuedJob,
    success: bool,
    skipped: bool,
    result: serde_json::Value,
    logs: String,
    rsmq: Option<R>,
) -> Result<Uuid, Error> {
    let is_flow =
        queued_job.job_kind == JobKind::Flow || queued_job.job_kind == JobKind::FlowPreview;
    let duration = if is_flow {
        let jobs = queued_job.parse_flow_status().map(|s| {
            let mut modules = s.modules;
            modules.extend([s.failure_module.module_status]);
            flatten_jobs(modules)
        });
        if let Some(jobs) = jobs {
            sqlx::query_scalar!(
                "SELECT SUM(duration_ms) as duration FROM completed_job WHERE id = ANY($1)",
                jobs.as_slice()
            )
            .fetch_one(db)
            .await
            .ok()
            .flatten()
        } else {
            tracing::warn!("Could not parse flow status");
            None
        }
    } else {
        None
    };

    let mem_peak = sqlx::query_scalar!("SELECT mem_peak FROM queue WHERE id = $1", &queued_job.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .flatten();
    let mut tx: QueueTransaction<'_, R> = (rsmq, db.begin().await?).into();
    let job_id = queued_job.id.clone();
    let duration = sqlx::query_scalar!(
        "INSERT INTO completed_job AS cj
                   ( workspace_id
                   , id
                   , parent_job
                   , created_by
                   , created_at
                   , started_at
                   , duration_ms
                   , success
                   , script_hash
                   , script_path
                   , args
                   , result
                   , logs
                   , raw_code
                   , raw_lock
                   , canceled
                   , canceled_by
                   , canceled_reason
                   , job_kind
                   , schedule_path
                   , permissioned_as
                   , flow_status
                   , raw_flow
                   , is_flow_step
                   , is_skipped
                   , language
                   , email
                   , visible_to_owner
                   , mem_peak
                   , tag
                )
            VALUES ($1, $2, $3, $4, $5, COALESCE($6, now()), COALESCE($26, (EXTRACT('epoch' FROM (now())) - EXTRACT('epoch' FROM (COALESCE($6, now()))))*1000), $7, $8, $9,\
                    $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $27, $28, $29, $30)
         ON CONFLICT (id) DO UPDATE SET success = $7, result = $11, logs = concat(cj.logs, $12) RETURNING duration_ms",
        queued_job.workspace_id,
        queued_job.id,
        queued_job.parent_job,
        queued_job.created_by,
        queued_job.created_at,
        queued_job.started_at,
        success,
        queued_job.script_hash.map(|x| x.0),
        queued_job.script_path,
        queued_job.args,
        result,
        logs,
        queued_job.raw_code,
        queued_job.raw_lock,
        queued_job.canceled,
        queued_job.canceled_by,
        queued_job.canceled_reason,
        queued_job.job_kind: JobKind,
        queued_job.schedule_path,
        queued_job.permissioned_as,
        queued_job.flow_status,
        queued_job.raw_flow,
        queued_job.is_flow_step,
        skipped,
        queued_job.language: ScriptLang,
        duration: Option<i64>,
        queued_job.email,
        queued_job.visible_to_owner,
        mem_peak,
        queued_job.tag,
    )
    .fetch_one(&mut tx)
    .await
    .map_err(|e| Error::InternalErr(format!("Could not add completed job {job_id}: {e}")))?;

    tx = delete_job(tx, &queued_job.workspace_id, job_id).await?;
    if !queued_job.is_flow_step
        && queued_job.job_kind != JobKind::Flow
        && queued_job.job_kind != JobKind::FlowPreview
        && queued_job.schedule_path.is_some()
        && queued_job.script_path.is_some()
    {
        tx = handle_maybe_scheduled_job(
            tx,
            db,
            queued_job.schedule_path.as_ref().unwrap(),
            queued_job.script_path.as_ref().unwrap(),
            &queued_job.workspace_id,
            success,
            if success { None } else { Some(result) },
        )
        .await?;
    }
    tx.commit().await?;

    #[cfg(feature = "enterprise")]
    if !is_flow && duration > 1000 {
        let additional_usage = duration / 1000;
        let w_id = &queued_job.workspace_id;
        let premium_workspace = *CLOUD_HOSTED
            && sqlx::query_scalar!("SELECT premium FROM workspace WHERE id = $1", w_id)
                .fetch_one(db)
                .await
                .map_err(|e| Error::InternalErr(format!("fetching if {w_id} is premium: {e}")))?;
        let _ = sqlx::query!(
                "INSERT INTO usage (id, is_workspace, month_, usage) 
                VALUES ($1, $2, EXTRACT(YEAR FROM current_date) * 12 + EXTRACT(MONTH FROM current_date), 0) 
                ON CONFLICT (id, is_workspace, month_) DO UPDATE SET usage = usage.usage + $3",
                if premium_workspace { w_id } else { &queued_job.email },
                premium_workspace,
                additional_usage)
                .execute(db)
                .await
                .map_err(|e| Error::InternalErr(format!("updating usage: {e}")));
    }

    tracing::debug!("Added completed job {}", queued_job.id);
    Ok(queued_job.id)
}

#[instrument(level = "trace", skip_all)]
pub async fn handle_maybe_scheduled_job<'c, R: rsmq_async::RsmqConnection + Clone + Send + 'c>(
    mut tx: QueueTransaction<'c, R>,
    db: &Pool<Postgres>,
    schedule_path: &str,
    script_path: &str,
    w_id: &str,
    success: bool,
    result: Option<serde_json::Value>,
) -> windmill_common::error::Result<QueueTransaction<'c, R>> {
    let schedule = get_schedule_opt(tx.transaction_mut(), w_id, schedule_path).await?;

    if schedule.is_none() {
        tracing::error!(
            "Schedule {schedule_path} in {w_id} not found. Impossible to schedule again"
        );
        return Ok(tx);
    }

    let schedule = schedule.unwrap();

    if schedule.enabled && script_path == schedule.script_path {
        if !success {
            if let Some(on_failure_path) = schedule.on_failure.clone() {
                let on_failure_result = handle_on_failure(
                    tx,
                    schedule_path,
                    script_path,
                    w_id,
                    &on_failure_path,
                    result,
                    &schedule.email,
                    &schedule_to_user(&schedule.path),
                    username_to_permissioned_as(&schedule.edited_by),
                )
                .await;

                match on_failure_result {
                    Ok(ntx) => {
                        tx = ntx;
                    }
                    Err(err) => {
                        sqlx::query!(
                        "UPDATE schedule SET enabled = false, error = $1 WHERE workspace_id = $2 AND path = $3",
                        format!("Could not trigger error handler: {err}"),
                        &schedule.workspace_id,
                        &schedule.path
                    )
                    .execute(db)
                    .await?;
                        tracing::warn!(
                            "Could not trigger error handler for {}: {}",
                            schedule_path,
                            err
                        );
                        return Err(err);
                    }
                }
            }
        }

        let res = push_scheduled_job(
            tx,
            Schedule {
                workspace_id: w_id.to_owned(),
                path: schedule.path.clone(),
                edited_by: schedule.edited_by,
                edited_at: schedule.edited_at,
                schedule: schedule.schedule,
                timezone: schedule.timezone,
                enabled: schedule.enabled,
                script_path: schedule.script_path,
                is_flow: schedule.is_flow,
                args: schedule
                    .args
                    .and_then(|e| serde_json::to_value(e).map_or(None, |v| Some(v))),
                extra_perms: serde_json::to_value(schedule.extra_perms).expect("hashmap -> json"),
                email: schedule.email,
                error: None,
                on_failure: schedule.on_failure,
            },
        )
        .await;
        match res {
            Ok(tx) => Ok(tx),
            Err(err) => {
                sqlx::query!(
                    "UPDATE schedule SET enabled = false, error = $1 WHERE workspace_id = $2 AND path = $3",
                    err.to_string(),
                    &schedule.workspace_id,
                    &schedule.path
                )
                .execute(db)
                .await?;
                tracing::warn!("Could not schedule job for {}: {}", schedule_path, err);
                Err(err)
            }
        }
    } else {
        Ok(tx)
    }
}

async fn handle_on_failure<'c, R: rsmq_async::RsmqConnection + Clone + Send + 'c>(
    mut tx: QueueTransaction<'c, R>,
    schedule_path: &str,
    script_path: &str,
    w_id: &str,
    on_failure_path: &str,
    result: Option<serde_json::Value>,
    username: &str,
    email: &str,
    permissioned_as: String,
) -> windmill_common::error::Result<QueueTransaction<'c, R>> {
    let (payload, tag) =
        get_payload_tag_from_prefixed_path(on_failure_path, tx.transaction_mut(), w_id).await?;

    let mut args = result
        .unwrap_or_else(|| json!({}))
        .as_object()
        .unwrap()
        .clone();
    args.insert("schedule_path".to_string(), json!(schedule_path));
    args.insert("path".to_string(), json!(script_path));
    let (uuid, tx) = push(
        tx,
        w_id,
        payload,
        args,
        username,
        email,
        permissioned_as,
        None,
        None,
        None,
        None,
        None,
        false,
        false,
        None,
        true,
        tag,
    )
    .await?;
    tracing::info!(
        "Pushed on_failure job {} for {} to queue",
        uuid,
        schedule_path
    );
    return Ok(tx);
}

pub async fn pull<R: rsmq_async::RsmqConnection + Clone>(
    db: &Pool<Postgres>,
    whitelist_workspaces: Option<Vec<String>>,
    blacklist_workspaces: Option<Vec<String>>,
    rsmq: Option<R>,
) -> windmill_common::error::Result<Option<QueuedJob>> {
    let mut workspaces_filter = String::new();
    if let Some(whitelist) = whitelist_workspaces {
        workspaces_filter.push_str(&format!(
            " AND workspace_id IN ({})",
            whitelist
                .into_iter()
                .map(|x| format!("'{x}'"))
                .collect::<Vec<String>>()
                .join(",")
        ));
        if let Some(_rsmq) = rsmq {
            todo!("REDIS: Implement workspace filters for redis");
        }
    }
    if let Some(blacklist) = blacklist_workspaces {
        workspaces_filter.push_str(&format!(
            " AND workspace_id NOT IN ({})",
            blacklist
                .into_iter()
                .map(|x| format!("'{x}'"))
                .collect::<Vec<String>>()
                .join(",")
        ));
        if let Some(_rsmq) = rsmq {
            todo!("REDIS: Implement workspace filters for redis");
        }
    }

    let job: Option<QueuedJob> = if let Some(mut rsmq) = rsmq {
        // TODO: REDIS: Race conditions / replace last_ping
        let msg = rsmq
            .pop_message::<Vec<u8>>(RSMQ_MAIN_QUEUE)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;

        if let Some(msg) = msg {
            let uuid = Uuid::from_bytes_le(
                msg.message
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("Failed to parsed Redis message"))?,
            );

            sqlx::query_as::<_, QueuedJob>(
                "UPDATE queue
            SET running = true
            , started_at = coalesce(started_at, now())
            , last_ping = now()
            , suspend_until = null
            WHERE id = $1
            RETURNING *",
            )
            .bind(uuid)
            .fetch_optional(db)
            .await?
        } else {
            None
        }
    } else {
        let accepted_tags_filter = &*ACCEPTED_TAGS_FILTER;
        /* Jobs can be started if they:
         * - haven't been started before,
         *   running = false
         * - are flows with a step that needed resume,
         *   suspend_until is non-null
         *   and suspend = 0 when the resume messages are received
         *   or suspend_until <= now() if it has timed out */
        sqlx::query_as::<_, QueuedJob>(&format!(
            "UPDATE queue
            SET running = true
              , started_at = coalesce(started_at, now())
              , last_ping = now()
              , suspend_until = null
            WHERE id = (
                SELECT id
                FROM queue
                WHERE ((running = false
                       AND scheduled_for <= now())
                   OR (suspend_until IS NOT NULL
                       AND (   suspend <= 0
                            OR suspend_until <= now()))) 
                    {workspaces_filter}
                    {accepted_tags_filter}
                ORDER BY scheduled_for
                FOR UPDATE SKIP LOCKED
                LIMIT 1
            )
            RETURNING *"
        ))
        .fetch_optional(db)
        .await?
    };

    if job.is_some() && *METRICS_ENABLED {
        QUEUE_PULL_COUNT.inc();
    }

    Ok(job)
}

#[async_recursion]
pub async fn get_result_by_id(
    db: Pool<Postgres>,
    w_id: String,
    flow_id: Uuid,
    node_id: String,
) -> error::Result<serde_json::Value> {
    let flow_job_result = sqlx::query!(
        "SELECT leaf_jobs->$1::text as leaf_jobs, parent_job FROM queue WHERE COALESCE((SELECT root_job FROM queue WHERE id = $2), $2) = id AND workspace_id = $3",
        node_id,
        flow_id,
        w_id,
    )
    .fetch_optional(&db)
    .await?;

    let flow_job_result = windmill_common::utils::not_found_if_none(
        flow_job_result,
        "Flow result by id",
        format!("{}, {}", flow_id, node_id),
    )?;

    let job_result = flow_job_result
        .leaf_jobs
        .map(|x| serde_json::from_value(x).ok())
        .flatten();

    if job_result.is_none() && flow_job_result.parent_job.is_some() {
        let parent_job = flow_job_result.parent_job.unwrap();
        let root_job = sqlx::query_scalar!("SELECT root_job FROM queue WHERE id = $1", parent_job)
            .fetch_optional(&db)
            .await?
            .flatten()
            .unwrap_or(parent_job);
        return get_result_by_id(db, w_id, root_job, node_id).await;
    }

    let result_id = windmill_common::utils::not_found_if_none(
        job_result,
        "Flow result by id",
        format!("{}, {}", flow_id, node_id),
    )?;

    let value = match result_id {
        JobResult::ListJob(x) => {
            let rows = sqlx::query_scalar!(
                "SELECT result FROM completed_job WHERE id = ANY($1) AND workspace_id = $2",
                x.as_slice(),
                w_id,
            )
            .fetch_all(&db)
            .await?
            .into_iter()
            .filter_map(|x| x)
            .collect::<Vec<serde_json::Value>>();
            serde_json::json!(rows)
        }
        JobResult::SingleJob(x) => sqlx::query_scalar!(
            "SELECT result FROM completed_job WHERE id = $1 AND workspace_id = $2",
            x,
            w_id,
        )
        .fetch_optional(&db)
        .await?
        .flatten()
        .unwrap_or(serde_json::Value::Null),
    };

    Ok(value)
}

#[instrument(level = "trace", skip_all)]
pub async fn delete_job<'c, R: rsmq_async::RsmqConnection + Clone + Send>(
    mut tx: QueueTransaction<'c, R>,
    w_id: &str,
    job_id: Uuid,
) -> windmill_common::error::Result<QueueTransaction<'c, R>> {
    if *METRICS_ENABLED {
        QUEUE_DELETE_COUNT.inc();
    }
    let job_removed = sqlx::query_scalar!(
        "DELETE FROM queue WHERE workspace_id = $1 AND id = $2 RETURNING 1",
        w_id,
        job_id
    )
    .fetch_one(&mut tx)
    .await
    .map_err(|e| Error::InternalErr(format!("Error during deletion of job {job_id}: {e}")))?
    .unwrap_or(0)
        == 1;
    tracing::debug!("Job {job_id} deleted: {job_removed}");
    Ok(tx)
}

pub async fn get_queued_job<'c>(
    id: Uuid,
    w_id: &str,
    tx: &mut Transaction<'c, Postgres>,
) -> error::Result<Option<QueuedJob>> {
    let r = sqlx::query_as::<_, QueuedJob>(
        "SELECT *
            FROM queue WHERE id = $1 AND workspace_id = $2",
    )
    .bind(id)
    .bind(w_id)
    .fetch_optional(tx)
    .await?;
    Ok(r)
}

// #[instrument(level = "trace", skip_all)]
pub async fn push<'c, R: rsmq_async::RsmqConnection + Send + 'c>(
    mut tx: QueueTransaction<'c, R>,
    workspace_id: &str,
    job_payload: JobPayload,
    args: serde_json::Map<String, serde_json::Value>,
    user: &str,
    email: &str,
    permissioned_as: String,
    scheduled_for_o: Option<chrono::DateTime<chrono::Utc>>,
    schedule_path: Option<String>,
    parent_job: Option<Uuid>,
    root_job: Option<Uuid>,
    job_id: Option<Uuid>,
    is_flow_step: bool,
    mut same_worker: bool,
    pre_run_error: Option<&windmill_common::error::Error>,
    visible_to_owner: bool,
    mut tag: Option<String>,
) -> Result<(Uuid, QueueTransaction<'c, R>), Error> {
    let args_json = serde_json::Value::Object(args);
    let job_id: Uuid = if let Some(job_id) = job_id {
        let conflicting_id = sqlx::query_scalar!(
            "SELECT 1 FROM queue WHERE id = $1 UNION ALL select 1 FROM completed_job WHERE id = $1",
            job_id
        )
        .fetch_optional(&mut tx)
        .await?;

        if conflicting_id.is_some() {
            return Err(Error::BadRequest(format!(
                "Job with id {job_id} already exists"
            )));
        }

        job_id
    } else {
        Ulid::new().into()
    };

    #[cfg(feature = "enterprise")]
    {
        let premium_workspace = *CLOUD_HOSTED
            && sqlx::query_scalar!("SELECT premium FROM workspace WHERE id = $1", workspace_id)
                .fetch_one(&mut tx)
                .await
                .map_err(|e| {
                    Error::InternalErr(format!("fetching if {workspace_id} is premium: {e}"))
                })?;

        // we track only non flow steps
        let usage = if !matches!(
            job_payload,
            JobPayload::Flow(_) | JobPayload::RawFlow { .. }
        ) {
            sqlx::query_scalar!(
                    "INSERT INTO usage (id, is_workspace, month_, usage) 
                    VALUES ($1, $2, EXTRACT(YEAR FROM current_date) * 12 + EXTRACT(MONTH FROM current_date), 0) 
                    ON CONFLICT (id, is_workspace, month_) DO UPDATE SET usage = usage.usage + 1 
                    RETURNING usage.usage",
                    if premium_workspace { workspace_id } else { email },
                    premium_workspace
                )
                .fetch_one(&mut tx)
                .await
                .map_err(|e| Error::InternalErr(format!("updating usage: {e}")))?
        } else if *CLOUD_HOSTED && !premium_workspace {
            sqlx::query_scalar!(
                "
        SELECT usage.usage + 1 FROM usage 
        WHERE is_workspace = false AND
     month_ = EXTRACT(YEAR FROM current_date) * 12 + EXTRACT(MONTH FROM current_date)
     AND id = $1",
                email
            )
            .fetch_optional(&mut tx)
            .await?
            .flatten()
            .unwrap_or(0)
        } else {
            0
        };

        if *CLOUD_HOSTED && !premium_workspace {
            let is_super_admin =
                sqlx::query_scalar!("SELECT super_admin FROM password WHERE email = $1", email)
                    .fetch_optional(&mut tx)
                    .await?
                    .unwrap_or(false);

            if !is_super_admin {
                if usage > MAX_FREE_EXECS
                    && !matches!(job_payload, JobPayload::Dependencies { .. })
                    && !matches!(job_payload, JobPayload::FlowDependencies { .. })
                {
                    return Err(error::Error::BadRequest(format!(
                    "User {email} has exceeded the free usage limit of {MAX_FREE_EXECS} that applies outside of premium workspaces."
                )));
                }
                let in_queue =
                    sqlx::query_scalar!("SELECT COUNT(id) FROM queue WHERE email = $1", email)
                        .fetch_one(&mut tx)
                        .await?
                        .unwrap_or(0);

                if in_queue > MAX_FREE_EXECS.into() {
                    return Err(error::Error::BadRequest(format!(
                    "User {email} has exceeded the jobs in queue limit of {MAX_FREE_EXECS} that applies outside of premium workspaces."
                )));
                }

                let concurrent_runs = sqlx::query_scalar!(
                    "SELECT COUNT(id) FROM queue WHERE running = true AND email = $1",
                    email
                )
                .fetch_one(&mut tx)
                .await?
                .unwrap_or(0);

                if concurrent_runs > MAX_FREE_CONCURRENT_RUNS.into() {
                    return Err(error::Error::BadRequest(format!(
                    "User {email} has exceeded the concurrent runs limit of {MAX_FREE_CONCURRENT_RUNS} that applies outside of premium workspaces."
                )));
                }
            }
        }
    }

    let (script_hash, script_path, raw_code_tuple, job_kind, mut raw_flow, language) =
        match job_payload {
            JobPayload::ScriptHash { hash, path } => {
                let language = sqlx::query_scalar!(
                    "SELECT language as \"language: ScriptLang\" FROM script WHERE hash = $1 AND workspace_id = $2",
                    hash.0,
                    workspace_id
                )
                .fetch_one(&mut tx)
                .await
                .map_err(|e| {
                    Error::InternalErr(format!(
                        "fetching language for hash {hash} in {workspace_id}: {e}"
                    ))
                })?;
                (
                    Some(hash.0),
                    Some(path),
                    None,
                    JobKind::Script,
                    None,
                    Some(language),
                )
            }
            JobPayload::ScriptHub { path } => {
                (
                    None,
                    Some(path),
                    None,
                    // Some((script.content, script.lockfile)),
                    JobKind::Script_Hub,
                    None,
                    None,
                )
            }
            JobPayload::Code(RawCode { content, path, language, lock }) => (
                None,
                path,
                Some((content, lock)),
                JobKind::Preview,
                None,
                Some(language),
            ),
            JobPayload::Dependencies { hash, dependencies, language } => (
                Some(hash.0),
                None,
                Some((dependencies, None)),
                JobKind::Dependencies,
                None,
                Some(language),
            ),
            JobPayload::FlowDependencies { path } => {
                let value_json = sqlx::query_scalar!(
                    "SELECT value FROM flow WHERE path = $1 AND workspace_id = $2",
                    path,
                    workspace_id
                )
                .fetch_optional(&mut tx)
                .await?
                .ok_or_else(|| Error::InternalErr(format!("not found flow at path {:?}", path)))?;
                let value = serde_json::from_value::<FlowValue>(value_json).map_err(|err| {
                    Error::InternalErr(format!(
                        "could not convert json to flow for {path}: {err:?}"
                    ))
                })?;
                (
                    None,
                    Some(path),
                    None,
                    JobKind::FlowDependencies,
                    Some(value),
                    None,
                )
            }
            JobPayload::RawFlow { value, path } => {
                (None, path, None, JobKind::FlowPreview, Some(value), None)
            }
            JobPayload::Flow(flow) => {
                let value_json = sqlx::query_scalar!(
                    "SELECT value FROM flow WHERE path = $1 AND workspace_id = $2",
                    flow,
                    workspace_id
                )
                .fetch_optional(&mut tx)
                .await?
                .ok_or_else(|| Error::InternalErr(format!("not found flow at path {:?}", flow)))?;
                let value = serde_json::from_value::<FlowValue>(value_json).map_err(|err| {
                    Error::InternalErr(format!(
                        "could not convert json to flow for {flow}: {err:?}"
                    ))
                })?;
                (None, Some(flow), None, JobKind::Flow, Some(value), None)
            }
            JobPayload::Identity => (None, None, None, JobKind::Identity, None, None),
            JobPayload::Graphql => (None, None, None, JobKind::Graphql, None, None),
            JobPayload::Http => (None, None, None, JobKind::Http, None, None),
        };

    let is_running = same_worker;
    if let Some(flow) = raw_flow.as_ref() {
        same_worker = same_worker || flow.same_worker;

        for module in flow.modules.iter() {
            if let Some(retry) = &module.retry {
                if retry.max_attempts() > MAX_RETRY_ATTEMPTS {
                    Err(Error::BadRequest(format!(
                        "retry attempts exceeds the maximum of {MAX_RETRY_ATTEMPTS}"
                    )))?
                }

                if matches!(retry.max_interval(), Some(interval) if interval > MAX_RETRY_INTERVAL) {
                    let max = MAX_RETRY_INTERVAL.as_secs();
                    Err(Error::BadRequest(format!(
                        "retry interval exceeds the maximum of {max} seconds"
                    )))?
                }
            }
        }

        // If last module has a sleep or suspend, we insert a virtual identity module
        if flow.modules.len() > 0
            && (flow.modules[flow.modules.len() - 1].sleep.is_some()
                || flow.modules[flow.modules.len() - 1].suspend.is_some())
        {
            let mut modules = flow.modules.clone();
            modules.push(FlowModule {
                id: format!("{}-v", flow.modules[flow.modules.len() - 1].id),
                value: FlowModuleValue::Identity,
                stop_after_if: None,
                summary: Some(
                    "Virtual module needed for suspend/sleep when last module".to_string(),
                ),
                mock: None,
                retry: None,
                sleep: None,
                suspend: None,
                cache_ttl: None,
            });
            raw_flow = Some(FlowValue { modules, ..flow.clone() });
        }
    }

    let (raw_code, raw_lock) = raw_code_tuple
        .map(|e| (Some(e.0), e.1))
        .unwrap_or_else(|| (None, None));

    let flow_status = raw_flow.as_ref().map(FlowStatus::new);

    let tag = if job_kind == JobKind::Dependencies || job_kind == JobKind::FlowDependencies {
        "dependency".to_string()
    } else if job_kind == JobKind::Flow || job_kind == JobKind::FlowPreview {
        "flow".to_string()
    } else if job_kind == JobKind::Identity {
        // identity is a light script, deno is too
        "deno".to_string()
    } else if job_kind == JobKind::Script_Hub {
        "hub".to_string()
    } else {
        if tag == Some("".to_string()) {
            tag = None;
        }
        tag.unwrap_or_else(|| {
            language
                .as_ref()
                .map(|x| x.as_str())
                .unwrap_or_else(|| "deno")
                .to_string()
        })
    };

    let uuid = sqlx::query_scalar!(
        "INSERT INTO queue
            (workspace_id, id, running, parent_job, created_by, permissioned_as, scheduled_for, 
                script_hash, script_path, raw_code, raw_lock, args, job_kind, schedule_path, raw_flow, \
         flow_status, is_flow_step, language, started_at, same_worker, pre_run_error, email, visible_to_owner, root_job, tag)
            VALUES ($1, $2, $3, $4, $5, $6, COALESCE($7, now()), $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, CASE WHEN $3 THEN now() END, $19, $20, $21, $22, $23, $24) \
         RETURNING id",
        workspace_id,
        job_id,
        is_running,
        parent_job,
        user,
        permissioned_as,
        scheduled_for_o,
        script_hash,
        script_path.clone(),
        raw_code,
        raw_lock,
        args_json,
        job_kind: JobKind,
        schedule_path,
        raw_flow.map(|f| serde_json::json!(f)),
        flow_status.map(|f| serde_json::json!(f)),
        is_flow_step,
        language: ScriptLang,
        same_worker,
        pre_run_error.map(|e| e.to_string()),
        email,
        visible_to_owner,
        root_job,
        tag
    )
    .fetch_one(&mut tx)
    .await
    .map_err(|e| Error::InternalErr(format!("Could not insert into queue {job_id}: {e}")))?;
    // TODO: technically the job isn't queued yet, as the transaction can be rolled back. Should be solved when moving these metrics to the queue abstraction.
    if *METRICS_ENABLED {
        QUEUE_PUSH_COUNT.inc();
    }

    {
        let uuid_string = job_id.to_string();
        let uuid_str = uuid_string.as_str();
        let mut hm = HashMap::from([("uuid", uuid_str), ("permissioned_as", &permissioned_as)]);

        let s: String;
        let operation_name = match job_kind {
            JobKind::Preview => "jobs.run.preview",
            JobKind::Script => {
                s = ScriptHash(script_hash.unwrap()).to_string();
                hm.insert("hash", s.as_str());
                "jobs.run.script"
            }
            JobKind::Flow => "jobs.run.flow",
            JobKind::FlowPreview => "jobs.run.flow_preview",
            JobKind::Script_Hub => "jobs.run.script_hub",
            JobKind::Dependencies => "jobs.run.dependencies",
            JobKind::Identity => "jobs.run.identity",
            JobKind::Http => "jobs.run.http",
            JobKind::Graphql => "jobs.run.graphql",
            JobKind::FlowDependencies => "jobs.run.flow_dependencies",
        };

        audit_log(
            &mut tx,
            &user,
            operation_name,
            ActionKind::Execute,
            workspace_id,
            script_path.as_ref().map(|x| x.as_str()),
            Some(hm),
        )
        .instrument(tracing::info_span!("job_run", email = &email))
        .await?;
    }
    if let Some(ref mut rsmq) = tx.rsmq {
        rsmq.send_message(job_id.to_bytes_le().to_vec(), scheduled_for_o);
    }

    Ok((uuid, tx))
}

pub fn canceled_job_to_result(job: &QueuedJob) -> serde_json::Value {
    let reason = job
        .canceled_reason
        .as_deref()
        .unwrap_or_else(|| "no reason given");
    let canceler = job.canceled_by.as_deref().unwrap_or_else(|| "unknown");
    serde_json::json!({"message": format!("Job canceled: {reason} by {canceler}"), "name": "Canceled", "reason": reason, "canceler": canceler})
}
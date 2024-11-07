use actix_web::web::Data;
use actix_web::{web, HttpRequest, HttpResponse};
use deserr::actix_web::AwebQueryParameter;
use deserr::Deserr;
use index_scheduler::{IndexScheduler, Query, TaskId};
use meilisearch_types::deserr::query_params::Param;
use meilisearch_types::deserr::DeserrQueryParamError;
use meilisearch_types::error::deserr_codes::*;
use meilisearch_types::error::{InvalidTaskDateError, ResponseError};
use meilisearch_types::index_uid::IndexUid;
use meilisearch_types::star_or::{OptionStarOr, OptionStarOrList};
use meilisearch_types::task_view::TaskView;
use meilisearch_types::tasks::{Kind, KindWithContent, Status};
use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::{Date, Duration, OffsetDateTime, Time};
use tokio::task;

use super::{get_task_id, is_dry_run, SummarizedTaskView};
use crate::analytics::{Aggregate, AggregateMethod, Analytics};
use crate::extractors::authentication::policies::*;
use crate::extractors::authentication::GuardedData;
use crate::extractors::sequential_extractor::SeqHandler;
use crate::{aggregate_methods, Opt};

const DEFAULT_LIMIT: u32 = 20;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("")
            .route(web::get().to(SeqHandler(get_tasks)))
            .route(web::delete().to(SeqHandler(delete_tasks))),
    )
    .service(web::resource("/cancel").route(web::post().to(SeqHandler(cancel_tasks))))
    .service(web::resource("/{task_id}").route(web::get().to(SeqHandler(get_task))));
}
#[derive(Debug, Deserr)]
#[deserr(error = DeserrQueryParamError, rename_all = camelCase, deny_unknown_fields)]
pub struct TasksFilterQuery {
    #[deserr(default = Param(DEFAULT_LIMIT), error = DeserrQueryParamError<InvalidTaskLimit>)]
    pub limit: Param<u32>,
    #[deserr(default, error = DeserrQueryParamError<InvalidTaskFrom>)]
    pub from: Option<Param<TaskId>>,
    #[deserr(default, error = DeserrQueryParamError<InvalidTaskReverse>)]
    pub reverse: Option<Param<bool>>,

    #[deserr(default, error = DeserrQueryParamError<InvalidTaskUids>)]
    pub uids: OptionStarOrList<u32>,
    #[deserr(default, error = DeserrQueryParamError<InvalidTaskCanceledBy>)]
    pub canceled_by: OptionStarOrList<u32>,
    #[deserr(default, error = DeserrQueryParamError<InvalidTaskTypes>)]
    pub types: OptionStarOrList<Kind>,
    #[deserr(default, error = DeserrQueryParamError<InvalidTaskStatuses>)]
    pub statuses: OptionStarOrList<Status>,
    #[deserr(default, error = DeserrQueryParamError<InvalidIndexUid>)]
    pub index_uids: OptionStarOrList<IndexUid>,

    #[deserr(default, error = DeserrQueryParamError<InvalidTaskAfterEnqueuedAt>, try_from(OptionStarOr<String>) = deserialize_date_after -> InvalidTaskDateError)]
    pub after_enqueued_at: OptionStarOr<OffsetDateTime>,
    #[deserr(default, error = DeserrQueryParamError<InvalidTaskBeforeEnqueuedAt>, try_from(OptionStarOr<String>) = deserialize_date_before -> InvalidTaskDateError)]
    pub before_enqueued_at: OptionStarOr<OffsetDateTime>,
    #[deserr(default, error = DeserrQueryParamError<InvalidTaskAfterStartedAt>, try_from(OptionStarOr<String>) = deserialize_date_after -> InvalidTaskDateError)]
    pub after_started_at: OptionStarOr<OffsetDateTime>,
    #[deserr(default, error = DeserrQueryParamError<InvalidTaskBeforeStartedAt>, try_from(OptionStarOr<String>) = deserialize_date_before -> InvalidTaskDateError)]
    pub before_started_at: OptionStarOr<OffsetDateTime>,
    #[deserr(default, error = DeserrQueryParamError<InvalidTaskAfterFinishedAt>, try_from(OptionStarOr<String>) = deserialize_date_after -> InvalidTaskDateError)]
    pub after_finished_at: OptionStarOr<OffsetDateTime>,
    #[deserr(default, error = DeserrQueryParamError<InvalidTaskBeforeFinishedAt>, try_from(OptionStarOr<String>) = deserialize_date_before -> InvalidTaskDateError)]
    pub before_finished_at: OptionStarOr<OffsetDateTime>,
}

impl TasksFilterQuery {
    fn into_query(self) -> Query {
        Query {
            limit: Some(self.limit.0),
            from: self.from.as_deref().copied(),
            reverse: self.reverse.as_deref().copied(),
            statuses: self.statuses.merge_star_and_none(),
            types: self.types.merge_star_and_none(),
            index_uids: self.index_uids.map(|x| x.to_string()).merge_star_and_none(),
            uids: self.uids.merge_star_and_none(),
            canceled_by: self.canceled_by.merge_star_and_none(),
            before_enqueued_at: self.before_enqueued_at.merge_star_and_none(),
            after_enqueued_at: self.after_enqueued_at.merge_star_and_none(),
            before_started_at: self.before_started_at.merge_star_and_none(),
            after_started_at: self.after_started_at.merge_star_and_none(),
            before_finished_at: self.before_finished_at.merge_star_and_none(),
            after_finished_at: self.after_finished_at.merge_star_and_none(),
        }
    }
}

impl TaskDeletionOrCancelationQuery {
    fn is_empty(&self) -> bool {
        matches!(
            self,
            TaskDeletionOrCancelationQuery {
                uids: OptionStarOrList::None,
                canceled_by: OptionStarOrList::None,
                types: OptionStarOrList::None,
                statuses: OptionStarOrList::None,
                index_uids: OptionStarOrList::None,
                after_enqueued_at: OptionStarOr::None,
                before_enqueued_at: OptionStarOr::None,
                after_started_at: OptionStarOr::None,
                before_started_at: OptionStarOr::None,
                after_finished_at: OptionStarOr::None,
                before_finished_at: OptionStarOr::None
            }
        )
    }
}

#[derive(Debug, Deserr)]
#[deserr(error = DeserrQueryParamError, rename_all = camelCase, deny_unknown_fields)]
pub struct TaskDeletionOrCancelationQuery {
    #[deserr(default, error = DeserrQueryParamError<InvalidTaskUids>)]
    pub uids: OptionStarOrList<u32>,
    #[deserr(default, error = DeserrQueryParamError<InvalidTaskCanceledBy>)]
    pub canceled_by: OptionStarOrList<u32>,
    #[deserr(default, error = DeserrQueryParamError<InvalidTaskTypes>)]
    pub types: OptionStarOrList<Kind>,
    #[deserr(default, error = DeserrQueryParamError<InvalidTaskStatuses>)]
    pub statuses: OptionStarOrList<Status>,
    #[deserr(default, error = DeserrQueryParamError<InvalidIndexUid>)]
    pub index_uids: OptionStarOrList<IndexUid>,

    #[deserr(default, error = DeserrQueryParamError<InvalidTaskAfterEnqueuedAt>, try_from(OptionStarOr<String>) = deserialize_date_after -> InvalidTaskDateError)]
    pub after_enqueued_at: OptionStarOr<OffsetDateTime>,
    #[deserr(default, error = DeserrQueryParamError<InvalidTaskBeforeEnqueuedAt>, try_from(OptionStarOr<String>) = deserialize_date_before -> InvalidTaskDateError)]
    pub before_enqueued_at: OptionStarOr<OffsetDateTime>,
    #[deserr(default, error = DeserrQueryParamError<InvalidTaskAfterStartedAt>, try_from(OptionStarOr<String>) = deserialize_date_after -> InvalidTaskDateError)]
    pub after_started_at: OptionStarOr<OffsetDateTime>,
    #[deserr(default, error = DeserrQueryParamError<InvalidTaskBeforeStartedAt>, try_from(OptionStarOr<String>) = deserialize_date_before -> InvalidTaskDateError)]
    pub before_started_at: OptionStarOr<OffsetDateTime>,
    #[deserr(default, error = DeserrQueryParamError<InvalidTaskAfterFinishedAt>, try_from(OptionStarOr<String>) = deserialize_date_after -> InvalidTaskDateError)]
    pub after_finished_at: OptionStarOr<OffsetDateTime>,
    #[deserr(default, error = DeserrQueryParamError<InvalidTaskBeforeFinishedAt>, try_from(OptionStarOr<String>) = deserialize_date_before -> InvalidTaskDateError)]
    pub before_finished_at: OptionStarOr<OffsetDateTime>,
}

impl TaskDeletionOrCancelationQuery {
    fn into_query(self) -> Query {
        Query {
            limit: None,
            from: None,
            reverse: None,
            statuses: self.statuses.merge_star_and_none(),
            types: self.types.merge_star_and_none(),
            index_uids: self.index_uids.map(|x| x.to_string()).merge_star_and_none(),
            uids: self.uids.merge_star_and_none(),
            canceled_by: self.canceled_by.merge_star_and_none(),
            before_enqueued_at: self.before_enqueued_at.merge_star_and_none(),
            after_enqueued_at: self.after_enqueued_at.merge_star_and_none(),
            before_started_at: self.before_started_at.merge_star_and_none(),
            after_started_at: self.after_started_at.merge_star_and_none(),
            before_finished_at: self.before_finished_at.merge_star_and_none(),
            after_finished_at: self.after_finished_at.merge_star_and_none(),
        }
    }
}

aggregate_methods!(
    CancelTasks => "Tasks Canceled",
    DeleteTasks => "Tasks Deleted",
);

#[derive(Serialize)]
struct TaskFilterAnalytics<Method: AggregateMethod> {
    filtered_by_uid: bool,
    filtered_by_index_uid: bool,
    filtered_by_type: bool,
    filtered_by_status: bool,
    filtered_by_canceled_by: bool,
    filtered_by_before_enqueued_at: bool,
    filtered_by_after_enqueued_at: bool,
    filtered_by_before_started_at: bool,
    filtered_by_after_started_at: bool,
    filtered_by_before_finished_at: bool,
    filtered_by_after_finished_at: bool,

    #[serde(skip)]
    marker: std::marker::PhantomData<Method>,
}

impl<Method: AggregateMethod + 'static> Aggregate for TaskFilterAnalytics<Method> {
    fn event_name(&self) -> &'static str {
        Method::event_name()
    }

    fn aggregate(self: Box<Self>, new: Box<Self>) -> Box<Self> {
        Box::new(Self {
            filtered_by_uid: self.filtered_by_uid | new.filtered_by_uid,
            filtered_by_index_uid: self.filtered_by_index_uid | new.filtered_by_index_uid,
            filtered_by_type: self.filtered_by_type | new.filtered_by_type,
            filtered_by_status: self.filtered_by_status | new.filtered_by_status,
            filtered_by_canceled_by: self.filtered_by_canceled_by | new.filtered_by_canceled_by,
            filtered_by_before_enqueued_at: self.filtered_by_before_enqueued_at
                | new.filtered_by_before_enqueued_at,
            filtered_by_after_enqueued_at: self.filtered_by_after_enqueued_at
                | new.filtered_by_after_enqueued_at,
            filtered_by_before_started_at: self.filtered_by_before_started_at
                | new.filtered_by_before_started_at,
            filtered_by_after_started_at: self.filtered_by_after_started_at
                | new.filtered_by_after_started_at,
            filtered_by_before_finished_at: self.filtered_by_before_finished_at
                | new.filtered_by_before_finished_at,
            filtered_by_after_finished_at: self.filtered_by_after_finished_at
                | new.filtered_by_after_finished_at,

            marker: std::marker::PhantomData,
        })
    }

    fn into_event(self: Box<Self>) -> serde_json::Value {
        serde_json::to_value(*self).unwrap_or_default()
    }
}

async fn cancel_tasks(
    index_scheduler: GuardedData<ActionPolicy<{ actions::TASKS_CANCEL }>, Data<IndexScheduler>>,
    params: AwebQueryParameter<TaskDeletionOrCancelationQuery, DeserrQueryParamError>,
    req: HttpRequest,
    opt: web::Data<Opt>,
    analytics: web::Data<Analytics>,
) -> Result<HttpResponse, ResponseError> {
    let params = params.into_inner();

    if params.is_empty() {
        return Err(index_scheduler::Error::TaskCancelationWithEmptyQuery.into());
    }

    analytics.publish(
        TaskFilterAnalytics::<CancelTasks> {
            filtered_by_uid: params.uids.is_some(),
            filtered_by_index_uid: params.index_uids.is_some(),
            filtered_by_type: params.types.is_some(),
            filtered_by_status: params.statuses.is_some(),
            filtered_by_canceled_by: params.canceled_by.is_some(),
            filtered_by_before_enqueued_at: params.before_enqueued_at.is_some(),
            filtered_by_after_enqueued_at: params.after_enqueued_at.is_some(),
            filtered_by_before_started_at: params.before_started_at.is_some(),
            filtered_by_after_started_at: params.after_started_at.is_some(),
            filtered_by_before_finished_at: params.before_finished_at.is_some(),
            filtered_by_after_finished_at: params.after_finished_at.is_some(),

            marker: std::marker::PhantomData,
        },
        &req,
    );

    let query = params.into_query();

    let (tasks, _) = index_scheduler.get_task_ids_from_authorized_indexes(
        &index_scheduler.read_txn()?,
        &query,
        index_scheduler.filters(),
    )?;
    let task_cancelation =
        KindWithContent::TaskCancelation { query: format!("?{}", req.query_string()), tasks };

    let uid = get_task_id(&req, &opt)?;
    let dry_run = is_dry_run(&req, &opt)?;
    let task =
        task::spawn_blocking(move || index_scheduler.register(task_cancelation, uid, dry_run))
            .await??;
    let task: SummarizedTaskView = task.into();

    Ok(HttpResponse::Ok().json(task))
}

async fn delete_tasks(
    index_scheduler: GuardedData<ActionPolicy<{ actions::TASKS_DELETE }>, Data<IndexScheduler>>,
    params: AwebQueryParameter<TaskDeletionOrCancelationQuery, DeserrQueryParamError>,
    req: HttpRequest,
    opt: web::Data<Opt>,
    analytics: web::Data<Analytics>,
) -> Result<HttpResponse, ResponseError> {
    let params = params.into_inner();

    if params.is_empty() {
        return Err(index_scheduler::Error::TaskDeletionWithEmptyQuery.into());
    }

    analytics.publish(
        TaskFilterAnalytics::<DeleteTasks> {
            filtered_by_uid: params.uids.is_some(),
            filtered_by_index_uid: params.index_uids.is_some(),
            filtered_by_type: params.types.is_some(),
            filtered_by_status: params.statuses.is_some(),
            filtered_by_canceled_by: params.canceled_by.is_some(),
            filtered_by_before_enqueued_at: params.before_enqueued_at.is_some(),
            filtered_by_after_enqueued_at: params.after_enqueued_at.is_some(),
            filtered_by_before_started_at: params.before_started_at.is_some(),
            filtered_by_after_started_at: params.after_started_at.is_some(),
            filtered_by_before_finished_at: params.before_finished_at.is_some(),
            filtered_by_after_finished_at: params.after_finished_at.is_some(),

            marker: std::marker::PhantomData,
        },
        &req,
    );

    let query = params.into_query();

    let (tasks, _) = index_scheduler.get_task_ids_from_authorized_indexes(
        &index_scheduler.read_txn()?,
        &query,
        index_scheduler.filters(),
    )?;
    let task_deletion =
        KindWithContent::TaskDeletion { query: format!("?{}", req.query_string()), tasks };

    let uid = get_task_id(&req, &opt)?;
    let dry_run = is_dry_run(&req, &opt)?;
    let task = task::spawn_blocking(move || index_scheduler.register(task_deletion, uid, dry_run))
        .await??;
    let task: SummarizedTaskView = task.into();

    Ok(HttpResponse::Ok().json(task))
}

#[derive(Debug, Serialize)]
pub struct AllTasks {
    results: Vec<TaskView>,
    total: u64,
    limit: u32,
    from: Option<u32>,
    next: Option<u32>,
}

async fn get_tasks(
    index_scheduler: GuardedData<ActionPolicy<{ actions::TASKS_GET }>, Data<IndexScheduler>>,
    params: AwebQueryParameter<TasksFilterQuery, DeserrQueryParamError>,
) -> Result<HttpResponse, ResponseError> {
    let mut params = params.into_inner();
    // We +1 just to know if there is more after this "page" or not.
    params.limit.0 = params.limit.0.saturating_add(1);
    let limit = params.limit.0;
    let query = params.into_query();

    let filters = index_scheduler.filters();
    let (tasks, total) = index_scheduler.get_tasks_from_authorized_indexes(query, filters)?;
    let mut results: Vec<_> = tasks.iter().map(TaskView::from_task).collect();

    // If we were able to fetch the number +1 tasks we asked
    // it means that there is more to come.
    let next = if results.len() == limit as usize { results.pop().map(|t| t.uid) } else { None };

    let from = results.first().map(|t| t.uid);
    let tasks = AllTasks { results, limit: limit.saturating_sub(1), total, from, next };

    Ok(HttpResponse::Ok().json(tasks))
}

async fn get_task(
    index_scheduler: GuardedData<ActionPolicy<{ actions::TASKS_GET }>, Data<IndexScheduler>>,
    task_uid: web::Path<String>,
) -> Result<HttpResponse, ResponseError> {
    let task_uid_string = task_uid.into_inner();

    let task_uid: TaskId = match task_uid_string.parse() {
        Ok(id) => id,
        Err(_e) => {
            return Err(index_scheduler::Error::InvalidTaskUids { task_uid: task_uid_string }.into())
        }
    };

    let query = index_scheduler::Query { uids: Some(vec![task_uid]), ..Query::default() };
    let filters = index_scheduler.filters();
    let (tasks, _) = index_scheduler.get_tasks_from_authorized_indexes(query, filters)?;

    if let Some(task) = tasks.first() {
        let task_view = TaskView::from_task(task);
        Ok(HttpResponse::Ok().json(task_view))
    } else {
        Err(index_scheduler::Error::TaskNotFound(task_uid).into())
    }
}

pub enum DeserializeDateOption {
    Before,
    After,
}

pub fn deserialize_date(
    value: &str,
    option: DeserializeDateOption,
) -> std::result::Result<OffsetDateTime, InvalidTaskDateError> {
    // We can't parse using time's rfc3339 format, since then we won't know what part of the
    // datetime was not explicitly specified, and thus we won't be able to increment it to the
    // next step.
    if let Ok(datetime) = OffsetDateTime::parse(value, &Rfc3339) {
        // fully specified up to the second
        // we assume that the subseconds are 0 if not specified, and we don't increment to the next second
        Ok(datetime)
    } else if let Ok(datetime) = Date::parse(
        value,
        format_description!("[year repr:full base:calendar]-[month repr:numerical]-[day]"),
    ) {
        let datetime = datetime.with_time(Time::MIDNIGHT).assume_utc();
        // add one day since the time was not specified
        match option {
            DeserializeDateOption::Before => Ok(datetime),
            DeserializeDateOption::After => {
                let datetime = datetime.checked_add(Duration::days(1)).unwrap_or(datetime);
                Ok(datetime)
            }
        }
    } else {
        Err(InvalidTaskDateError(value.to_owned()))
    }
}

pub fn deserialize_date_after(
    value: OptionStarOr<String>,
) -> std::result::Result<OptionStarOr<OffsetDateTime>, InvalidTaskDateError> {
    value.try_map(|x| deserialize_date(&x, DeserializeDateOption::After))
}
pub fn deserialize_date_before(
    value: OptionStarOr<String>,
) -> std::result::Result<OptionStarOr<OffsetDateTime>, InvalidTaskDateError> {
    value.try_map(|x| deserialize_date(&x, DeserializeDateOption::Before))
}

#[cfg(test)]
mod tests {
    use deserr::Deserr;
    use meili_snap::snapshot;
    use meilisearch_types::deserr::DeserrQueryParamError;
    use meilisearch_types::error::{Code, ResponseError};

    use crate::routes::tasks::{TaskDeletionOrCancelationQuery, TasksFilterQuery};

    fn deserr_query_params<T>(j: &str) -> Result<T, ResponseError>
    where
        T: Deserr<DeserrQueryParamError>,
    {
        let value = serde_urlencoded::from_str::<serde_json::Value>(j)
            .map_err(|e| ResponseError::from_msg(e.to_string(), Code::BadRequest))?;

        match deserr::deserialize::<_, _, DeserrQueryParamError>(value) {
            Ok(data) => Ok(data),
            Err(e) => Err(ResponseError::from(e)),
        }
    }

    #[test]
    fn deserialize_task_filter_dates() {
        {
            let params = "afterEnqueuedAt=2021-12-03&beforeEnqueuedAt=2021-12-03&afterStartedAt=2021-12-03&beforeStartedAt=2021-12-03&afterFinishedAt=2021-12-03&beforeFinishedAt=2021-12-03";
            let query = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap();

            snapshot!(format!("{:?}", query.after_enqueued_at), @"Other(2021-12-04 0:00:00.0 +00:00:00)");
            snapshot!(format!("{:?}", query.before_enqueued_at), @"Other(2021-12-03 0:00:00.0 +00:00:00)");
            snapshot!(format!("{:?}", query.after_started_at), @"Other(2021-12-04 0:00:00.0 +00:00:00)");
            snapshot!(format!("{:?}", query.before_started_at), @"Other(2021-12-03 0:00:00.0 +00:00:00)");
            snapshot!(format!("{:?}", query.after_finished_at), @"Other(2021-12-04 0:00:00.0 +00:00:00)");
            snapshot!(format!("{:?}", query.before_finished_at), @"Other(2021-12-03 0:00:00.0 +00:00:00)");
        }
        {
            let params =
                "afterEnqueuedAt=2021-12-03T23:45:23Z&beforeEnqueuedAt=2021-12-03T23:45:23Z";
            let query = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap();
            snapshot!(format!("{:?}", query.after_enqueued_at), @"Other(2021-12-03 23:45:23.0 +00:00:00)");
            snapshot!(format!("{:?}", query.before_enqueued_at), @"Other(2021-12-03 23:45:23.0 +00:00:00)");
        }
        {
            let params = "afterEnqueuedAt=1997-11-12T09:55:06-06:20";
            let query = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap();
            snapshot!(format!("{:?}", query.after_enqueued_at), @"Other(1997-11-12 9:55:06.0 -06:20:00)");
        }
        {
            let params = "afterEnqueuedAt=1997-11-12T09:55:06%2B00:00";
            let query = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap();
            snapshot!(format!("{:?}", query.after_enqueued_at), @"Other(1997-11-12 9:55:06.0 +00:00:00)");
        }
        {
            let params = "afterEnqueuedAt=1997-11-12T09:55:06.200000300Z";
            let query = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap();
            snapshot!(format!("{:?}", query.after_enqueued_at), @"Other(1997-11-12 9:55:06.2000003 +00:00:00)");
        }
        {
            // Stars are allowed in date fields as well
            let params = "afterEnqueuedAt=*&beforeStartedAt=*&afterFinishedAt=*&beforeFinishedAt=*&afterStartedAt=*&beforeEnqueuedAt=*";
            let query = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap();
            snapshot!(format!("{:?}", query), @"TaskDeletionOrCancelationQuery { uids: None, canceled_by: None, types: None, statuses: None, index_uids: None, after_enqueued_at: Star, before_enqueued_at: Star, after_started_at: Star, before_started_at: Star, after_finished_at: Star, before_finished_at: Star }");
        }
        {
            let params = "afterFinishedAt=2021";
            let err = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap_err();
            snapshot!(meili_snap::json_string!(err), @r###"
            {
              "message": "Invalid value in parameter `afterFinishedAt`: `2021` is an invalid date-time. It should follow the YYYY-MM-DD or RFC 3339 date-time format.",
              "code": "invalid_task_after_finished_at",
              "type": "invalid_request",
              "link": "https://docs.meilisearch.com/errors#invalid_task_after_finished_at"
            }
            "###);
        }
        {
            let params = "beforeFinishedAt=2021";
            let err = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap_err();
            snapshot!(meili_snap::json_string!(err), @r###"
            {
              "message": "Invalid value in parameter `beforeFinishedAt`: `2021` is an invalid date-time. It should follow the YYYY-MM-DD or RFC 3339 date-time format.",
              "code": "invalid_task_before_finished_at",
              "type": "invalid_request",
              "link": "https://docs.meilisearch.com/errors#invalid_task_before_finished_at"
            }
            "###);
        }
        {
            let params = "afterEnqueuedAt=2021-12";
            let err = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap_err();
            snapshot!(meili_snap::json_string!(err), @r###"
            {
              "message": "Invalid value in parameter `afterEnqueuedAt`: `2021-12` is an invalid date-time. It should follow the YYYY-MM-DD or RFC 3339 date-time format.",
              "code": "invalid_task_after_enqueued_at",
              "type": "invalid_request",
              "link": "https://docs.meilisearch.com/errors#invalid_task_after_enqueued_at"
            }
            "###);
        }

        {
            let params = "beforeEnqueuedAt=2021-12-03T23";
            let err = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap_err();
            snapshot!(meili_snap::json_string!(err), @r###"
            {
              "message": "Invalid value in parameter `beforeEnqueuedAt`: `2021-12-03T23` is an invalid date-time. It should follow the YYYY-MM-DD or RFC 3339 date-time format.",
              "code": "invalid_task_before_enqueued_at",
              "type": "invalid_request",
              "link": "https://docs.meilisearch.com/errors#invalid_task_before_enqueued_at"
            }
            "###);
        }
        {
            let params = "afterStartedAt=2021-12-03T23:45";
            let err = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap_err();
            snapshot!(meili_snap::json_string!(err), @r###"
            {
              "message": "Invalid value in parameter `afterStartedAt`: `2021-12-03T23:45` is an invalid date-time. It should follow the YYYY-MM-DD or RFC 3339 date-time format.",
              "code": "invalid_task_after_started_at",
              "type": "invalid_request",
              "link": "https://docs.meilisearch.com/errors#invalid_task_after_started_at"
            }
            "###);
        }
        {
            let params = "beforeStartedAt=2021-12-03T23:45";
            let err = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap_err();
            snapshot!(meili_snap::json_string!(err), @r###"
            {
              "message": "Invalid value in parameter `beforeStartedAt`: `2021-12-03T23:45` is an invalid date-time. It should follow the YYYY-MM-DD or RFC 3339 date-time format.",
              "code": "invalid_task_before_started_at",
              "type": "invalid_request",
              "link": "https://docs.meilisearch.com/errors#invalid_task_before_started_at"
            }
            "###);
        }
    }

    #[test]
    fn deserialize_task_filter_uids() {
        {
            let params = "uids=78,1,12,73";
            let query = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap();
            snapshot!(format!("{:?}", query.uids), @"List([78, 1, 12, 73])");
        }
        {
            let params = "uids=1";
            let query = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap();
            snapshot!(format!("{:?}", query.uids), @"List([1])");
        }
        {
            let params = "uids=cat,*,dog";
            let err = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap_err();
            snapshot!(meili_snap::json_string!(err), @r###"
            {
              "message": "Invalid value in parameter `uids[0]`: could not parse `cat` as a positive integer",
              "code": "invalid_task_uids",
              "type": "invalid_request",
              "link": "https://docs.meilisearch.com/errors#invalid_task_uids"
            }
            "###);
        }
        {
            let params = "uids=78,hello,world";
            let err = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap_err();
            snapshot!(meili_snap::json_string!(err), @r###"
            {
              "message": "Invalid value in parameter `uids[1]`: could not parse `hello` as a positive integer",
              "code": "invalid_task_uids",
              "type": "invalid_request",
              "link": "https://docs.meilisearch.com/errors#invalid_task_uids"
            }
            "###);
        }
        {
            let params = "uids=cat";
            let err = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap_err();
            snapshot!(meili_snap::json_string!(err), @r###"
            {
              "message": "Invalid value in parameter `uids`: could not parse `cat` as a positive integer",
              "code": "invalid_task_uids",
              "type": "invalid_request",
              "link": "https://docs.meilisearch.com/errors#invalid_task_uids"
            }
            "###);
        }
    }

    #[test]
    fn deserialize_task_filter_status() {
        {
            let params = "statuses=succeeded,failed,enqueued,processing,canceled";
            let query = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap();
            snapshot!(format!("{:?}", query.statuses), @"List([Succeeded, Failed, Enqueued, Processing, Canceled])");
        }
        {
            let params = "statuses=enqueued";
            let query = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap();
            snapshot!(format!("{:?}", query.statuses), @"List([Enqueued])");
        }
        {
            let params = "statuses=finished";
            let err = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap_err();
            snapshot!(meili_snap::json_string!(err), @r###"
            {
              "message": "Invalid value in parameter `statuses`: `finished` is not a valid task status. Available statuses are `enqueued`, `processing`, `succeeded`, `failed`, `canceled`.",
              "code": "invalid_task_statuses",
              "type": "invalid_request",
              "link": "https://docs.meilisearch.com/errors#invalid_task_statuses"
            }
            "###);
        }
    }
    #[test]
    fn deserialize_task_filter_types() {
        {
            let params = "types=documentAdditionOrUpdate,documentDeletion,settingsUpdate,indexCreation,indexDeletion,indexUpdate,indexSwap,taskCancelation,taskDeletion,dumpCreation,snapshotCreation";
            let query = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap();
            snapshot!(format!("{:?}", query.types), @"List([DocumentAdditionOrUpdate, DocumentDeletion, SettingsUpdate, IndexCreation, IndexDeletion, IndexUpdate, IndexSwap, TaskCancelation, TaskDeletion, DumpCreation, SnapshotCreation])");
        }
        {
            let params = "types=settingsUpdate";
            let query = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap();
            snapshot!(format!("{:?}", query.types), @"List([SettingsUpdate])");
        }
        {
            let params = "types=createIndex";
            let err = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap_err();
            snapshot!(meili_snap::json_string!(err), @r###"
            {
              "message": "Invalid value in parameter `types`: `createIndex` is not a valid task type. Available types are `documentAdditionOrUpdate`, `documentEdition`, `documentDeletion`, `settingsUpdate`, `indexCreation`, `indexDeletion`, `indexUpdate`, `indexSwap`, `taskCancelation`, `taskDeletion`, `dumpCreation`, `snapshotCreation`.",
              "code": "invalid_task_types",
              "type": "invalid_request",
              "link": "https://docs.meilisearch.com/errors#invalid_task_types"
            }
            "###);
        }
    }
    #[test]
    fn deserialize_task_filter_index_uids() {
        {
            let params = "indexUids=toto,tata-78";
            let query = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap();
            snapshot!(format!("{:?}", query.index_uids), @r###"List([IndexUid("toto"), IndexUid("tata-78")])"###);
        }
        {
            let params = "indexUids=index_a";
            let query = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap();
            snapshot!(format!("{:?}", query.index_uids), @r###"List([IndexUid("index_a")])"###);
        }
        {
            let params = "indexUids=1,hé";
            let err = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap_err();
            snapshot!(meili_snap::json_string!(err), @r###"
            {
              "message": "Invalid value in parameter `indexUids[1]`: `hé` is not a valid index uid. Index uid can be an integer or a string containing only alphanumeric characters, hyphens (-) and underscores (_), and can not be more than 512 bytes.",
              "code": "invalid_index_uid",
              "type": "invalid_request",
              "link": "https://docs.meilisearch.com/errors#invalid_index_uid"
            }
            "###);
        }
        {
            let params = "indexUids=hé";
            let err = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap_err();
            snapshot!(meili_snap::json_string!(err), @r###"
            {
              "message": "Invalid value in parameter `indexUids`: `hé` is not a valid index uid. Index uid can be an integer or a string containing only alphanumeric characters, hyphens (-) and underscores (_), and can not be more than 512 bytes.",
              "code": "invalid_index_uid",
              "type": "invalid_request",
              "link": "https://docs.meilisearch.com/errors#invalid_index_uid"
            }
            "###);
        }
    }

    #[test]
    fn deserialize_task_filter_general() {
        {
            let params = "from=12&limit=15&indexUids=toto,tata-78&statuses=succeeded,enqueued&afterEnqueuedAt=2012-04-23&uids=1,2,3";
            let query = deserr_query_params::<TasksFilterQuery>(params).unwrap();
            snapshot!(format!("{:?}", query), @r###"TasksFilterQuery { limit: Param(15), from: Some(Param(12)), reverse: None, uids: List([1, 2, 3]), canceled_by: None, types: None, statuses: List([Succeeded, Enqueued]), index_uids: List([IndexUid("toto"), IndexUid("tata-78")]), after_enqueued_at: Other(2012-04-24 0:00:00.0 +00:00:00), before_enqueued_at: None, after_started_at: None, before_started_at: None, after_finished_at: None, before_finished_at: None }"###);
        }
        {
            // Stars should translate to `None` in the query
            // Verify value of the default limit
            let params = "indexUids=*&statuses=succeeded,*&afterEnqueuedAt=2012-04-23&uids=1,2,3";
            let query = deserr_query_params::<TasksFilterQuery>(params).unwrap();
            snapshot!(format!("{:?}", query), @"TasksFilterQuery { limit: Param(20), from: None, reverse: None, uids: List([1, 2, 3]), canceled_by: None, types: None, statuses: Star, index_uids: Star, after_enqueued_at: Other(2012-04-24 0:00:00.0 +00:00:00), before_enqueued_at: None, after_started_at: None, before_started_at: None, after_finished_at: None, before_finished_at: None }");
        }
        {
            // Stars should also translate to `None` in task deletion/cancelation queries
            let params = "indexUids=*&statuses=succeeded,*&afterEnqueuedAt=2012-04-23&uids=1,2,3";
            let query = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap();
            snapshot!(format!("{:?}", query), @"TaskDeletionOrCancelationQuery { uids: List([1, 2, 3]), canceled_by: None, types: None, statuses: Star, index_uids: Star, after_enqueued_at: Other(2012-04-24 0:00:00.0 +00:00:00), before_enqueued_at: None, after_started_at: None, before_started_at: None, after_finished_at: None, before_finished_at: None }");
        }
        {
            // Star in from not allowed
            let params = "uids=*&from=*";
            let err = deserr_query_params::<TasksFilterQuery>(params).unwrap_err();
            snapshot!(meili_snap::json_string!(err), @r###"
            {
              "message": "Invalid value in parameter `from`: could not parse `*` as a positive integer",
              "code": "invalid_task_from",
              "type": "invalid_request",
              "link": "https://docs.meilisearch.com/errors#invalid_task_from"
            }
            "###);
        }
        {
            // From not allowed in task deletion/cancelation queries
            let params = "from=12";
            let err = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap_err();
            snapshot!(meili_snap::json_string!(err), @r###"
            {
              "message": "Unknown parameter `from`: expected one of `uids`, `canceledBy`, `types`, `statuses`, `indexUids`, `afterEnqueuedAt`, `beforeEnqueuedAt`, `afterStartedAt`, `beforeStartedAt`, `afterFinishedAt`, `beforeFinishedAt`",
              "code": "bad_request",
              "type": "invalid_request",
              "link": "https://docs.meilisearch.com/errors#bad_request"
            }
            "###);
        }
        {
            // Limit not allowed in task deletion/cancelation queries
            let params = "limit=12";
            let err = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap_err();
            snapshot!(meili_snap::json_string!(err), @r###"
            {
              "message": "Unknown parameter `limit`: expected one of `uids`, `canceledBy`, `types`, `statuses`, `indexUids`, `afterEnqueuedAt`, `beforeEnqueuedAt`, `afterStartedAt`, `beforeStartedAt`, `afterFinishedAt`, `beforeFinishedAt`",
              "code": "bad_request",
              "type": "invalid_request",
              "link": "https://docs.meilisearch.com/errors#bad_request"
            }
            "###);
        }
    }

    #[test]
    fn deserialize_task_delete_or_cancel_empty() {
        {
            let params = "";
            let query = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap();
            assert!(query.is_empty());
        }
        {
            let params = "statuses=*";
            let query = deserr_query_params::<TaskDeletionOrCancelationQuery>(params).unwrap();
            assert!(!query.is_empty());
            snapshot!(format!("{query:?}"), @"TaskDeletionOrCancelationQuery { uids: None, canceled_by: None, types: None, statuses: Star, index_uids: None, after_enqueued_at: None, before_enqueued_at: None, after_started_at: None, before_started_at: None, after_finished_at: None, before_finished_at: None }");
        }
    }
}

//! Which tasks a run moves or copies. Pure, so the dashboard preview and the runner always
//! agree and tests need no ClickUp.

use std::collections::HashSet;

use jiff::tz::TimeZone;

use super::{CREATE_LOOKBACK_SECS, Mode, Rule, Trigger, schedule};
use crate::clickup::client::Task;
use crate::modules::clickup::normalize_tag;

/// One task due to recur.
#[derive(Debug, Clone)]
pub struct Planned {
    pub rule: Rule,
    pub task: Task,
    /// Unix milliseconds, like ClickUp.
    pub old_due: i64,
    pub new_due: i64,
    /// The moved start date, when the task has one and the rule moves it.
    pub new_start: Option<i64>,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub items: Vec<Planned>,
    /// Tasks due to recur beyond the per run cap. A later run handles them.
    pub deferred: usize,
}

/// Matches tasks against rules. `fetched` holds every enabled rule, oldest first, with the
/// tasks read for its tag; a task with the tags of several rules belongs to the oldest of
/// them. `done` holds the (task id, due ms) occurrences already handled.
pub fn plan(
    fetched: &[(Rule, Vec<Task>)],
    now_ms: i64,
    tz: &TimeZone,
    done: &HashSet<(String, i64)>,
    cap: usize,
) -> Plan {
    let mut plan = Plan::default();
    let mut claimed = HashSet::new();
    for (rule, tasks) in fetched {
        for task in tasks {
            let tagged = task
                .tags
                .iter()
                .any(|t| normalize_tag(t.name()).as_deref() == Some(rule.tag.as_str()));
            if !tagged || !claimed.insert(task.id.as_str()) {
                continue;
            }
            let Some(item) = consider(rule, task, now_ms, tz, done) else {
                continue;
            };
            if plan.items.len() < cap {
                plan.items.push(item);
            } else {
                plan.deferred += 1;
            }
        }
    }
    plan
}

fn consider(
    rule: &Rule,
    task: &Task,
    now_ms: i64,
    tz: &TimeZone,
    done: &HashSet<(String, i64)>,
) -> Option<Planned> {
    if task.archived {
        return None;
    }
    let due = task.due_date?;
    let ready = match rule.trigger {
        Trigger::Close => task.status.as_ref().is_some_and(|s| s.is_done()),
        Trigger::Schedule => due <= now_ms,
    };
    if !ready || done.contains(&(task.id.clone(), due)) {
        return None;
    }
    if rule.mode == Mode::Create && due < now_ms - CREATE_LOOKBACK_SECS * 1000 {
        return None;
    }
    let (new_due, steps) = schedule::next_due(due, rule.every, rule.unit, tz, now_ms)?;
    let new_start = task
        .start_date
        .filter(|_| rule.shift_start)
        .and_then(|start| schedule::shift(start, steps, rule.every, rule.unit, tz));
    Some(Planned {
        rule: rule.clone(),
        task: task.clone(),
        old_due: due,
        new_due,
        new_start,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::clickup::mock;
    use crate::modules::recurring::Unit;

    const DAY: i64 = 86_400_000;
    // Thursday 2026-10-15 09:00:00 UTC.
    const NOW: i64 = 1_792_054_800_000;

    fn rule(id: i64, tag: &str) -> Rule {
        Rule {
            id,
            name: format!("Rule {id}"),
            tag: tag.into(),
            ..Rule::new()
        }
    }

    /// A task with `tags`, due `due` ms, in a status of `kind`.
    fn task(id: &str, due: Option<i64>, kind: &str, tags: &[&str]) -> Task {
        let mut t = mock::task(id, id, due);
        t["status"] = json!({"status": kind, "type": kind});
        t["tags"] = tags.iter().map(|t| json!({"name": t})).collect();
        serde_json::from_value(t).unwrap()
    }

    fn ids(plan: &Plan) -> Vec<&str> {
        plan.items.iter().map(|p| p.task.id.as_str()).collect()
    }

    #[test]
    fn close_rules_need_a_done_or_closed_status() {
        let tasks = vec![
            task("closed", Some(NOW - DAY), "closed", &["weekly"]),
            task("done", Some(NOW + DAY), "done", &["Weekly"]),
            task("open", Some(NOW - DAY), "open", &["weekly"]),
            task("custom", Some(NOW - DAY), "custom", &["weekly"]),
            task("undated", None, "closed", &["weekly"]),
            task("untagged", Some(NOW - DAY), "closed", &["other"]),
        ];
        let fetched = vec![(rule(1, "weekly"), tasks)];
        let plan = plan(&fetched, NOW, &TimeZone::UTC, &HashSet::new(), 25);
        assert_eq!(ids(&plan), ["closed", "done"]);
        assert_eq!(plan.items[0].new_due, NOW + 6 * DAY);
        // Done early: a week after its due date, not after today.
        assert_eq!(plan.items[1].new_due, NOW + 8 * DAY);
    }

    #[test]
    fn schedule_rules_need_a_passed_due_date() {
        let mut r = rule(1, "daily");
        r.trigger = Trigger::Schedule;
        r.unit = Unit::Day;
        let tasks = vec![
            task("late", Some(NOW - 3 * DAY - 1000), "open", &["daily"]),
            task("now", Some(NOW), "custom", &["daily"]),
            task("closed", Some(NOW - DAY), "closed", &["daily"]),
            task("future", Some(NOW + 1000), "open", &["daily"]),
        ];
        let plan = plan(&[(r, tasks)], NOW, &TimeZone::UTC, &HashSet::new(), 25);
        assert_eq!(ids(&plan), ["late", "now", "closed"]);
        assert_eq!(
            plan.items[0].new_due,
            NOW + DAY - 1000,
            "missed days skipped"
        );
        assert_eq!(plan.items[1].new_due, NOW + DAY);
    }

    #[test]
    fn start_dates_move_by_the_same_interval() {
        let mut t = task("t", Some(NOW - DAY), "closed", &["weekly"]);
        t.start_date = Some(NOW - 2 * DAY);
        let fetched = vec![(rule(1, "weekly"), vec![t.clone()])];
        let p = plan(&fetched, NOW, &TimeZone::UTC, &HashSet::new(), 25);
        assert_eq!(p.items[0].new_start, Some(NOW + 5 * DAY));

        let mut keep = rule(1, "weekly");
        keep.shift_start = false;
        let p = plan(&[(keep, vec![t])], NOW, &TimeZone::UTC, &HashSet::new(), 25);
        assert_eq!(p.items[0].new_start, None);
    }

    #[test]
    fn oldest_rule_claims_a_task_and_handled_ones_are_skipped() {
        let both = task("both", Some(NOW - DAY), "closed", &["weekly", "monthly"]);
        let mut monthly = rule(2, "monthly");
        monthly.unit = Unit::Month;
        let fetched = vec![
            (rule(1, "weekly"), vec![both.clone()]),
            (
                monthly,
                vec![both, task("m", Some(NOW - DAY), "closed", &["monthly"])],
            ),
        ];
        let p = plan(&fetched, NOW, &TimeZone::UTC, &HashSet::new(), 25);
        assert_eq!(ids(&p), ["both", "m"]);
        assert_eq!(p.items[0].rule.id, 1);
        assert_eq!(p.items[1].rule.id, 2);

        let done = HashSet::from([("both".to_owned(), NOW - DAY)]);
        let p = plan(&fetched, NOW, &TimeZone::UTC, &done, 25);
        assert_eq!(
            ids(&p),
            ["m"],
            "a handled task is not passed to the next rule"
        );
    }

    #[test]
    fn archived_tasks_and_old_copies_are_ignored() {
        let mut archived = task("archived", Some(NOW - DAY), "closed", &["weekly"]);
        archived.archived = true;
        let old = task("old", Some(NOW - 61 * DAY), "closed", &["weekly"]);
        let mut copy = rule(1, "weekly");
        copy.mode = Mode::Create;
        let fetched = vec![(copy, vec![archived.clone(), old.clone()])];
        assert!(
            plan(&fetched, NOW, &TimeZone::UTC, &HashSet::new(), 25)
                .items
                .is_empty()
        );
        // Moving has no look back limit.
        let fetched = vec![(rule(1, "weekly"), vec![archived, old])];
        let p = plan(&fetched, NOW, &TimeZone::UTC, &HashSet::new(), 25);
        assert_eq!(ids(&p), ["old"]);
    }

    #[test]
    fn cap_defers_the_rest() {
        let tasks: Vec<Task> = (0..5)
            .map(|i| task(&format!("t{i}"), Some(NOW - DAY), "closed", &["weekly"]))
            .collect();
        let p = plan(
            &[(rule(1, "weekly"), tasks)],
            NOW,
            &TimeZone::UTC,
            &HashSet::new(),
            3,
        );
        assert_eq!((p.items.len(), p.deferred), (3, 2));
    }
}

//! One scheduler loop for every periodic job: backups, image checks and self-update.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Local};
use croner::Cron;

use crate::ctx::Ctx;

pub fn parse(expr: &str) -> Result<Cron> {
    expr.parse::<Cron>().map_err(|e| anyhow!("invalid cron \"{expr}\": {e}"))
}

pub fn next(expr: &str) -> Option<DateTime<Local>> {
    parse(expr).ok()?.find_next_occurrence(&Local::now(), false).ok()
}

#[derive(Debug, Clone, PartialEq)]
pub enum Job {
    Backup(String),
    Images,
    SelfUpdate,
}

struct Entry {
    job: Job,
    expr: String,
    next: Option<DateTime<Local>>,
}

#[derive(Default)]
pub struct Scheduler {
    entries: Mutex<Vec<Entry>>,
}

impl Scheduler {
    /// Replace the job list. Jobs whose schedule did not change keep their next run time.
    pub fn set(&self, jobs: Vec<(Job, String)>) {
        let mut entries = self.entries.lock().unwrap();
        let old = std::mem::take(&mut *entries);
        *entries = jobs
            .into_iter()
            .map(|(job, expr)| {
                let next = old.iter().find(|e| e.job == job && e.expr == expr).map_or_else(|| next(&expr), |e| e.next);
                Entry { job, expr, next }
            })
            .collect();
    }

    fn due(&self) -> Vec<Job> {
        let now = Local::now();
        let mut entries = self.entries.lock().unwrap();
        let mut due = vec![];
        for e in entries.iter_mut() {
            if e.next.is_some_and(|n| n <= now) {
                due.push(e.job.clone());
                e.next = next(&e.expr);
            }
        }
        due
    }
}

pub async fn run(ctx: Arc<Ctx>) {
    loop {
        tokio::time::sleep(Duration::from_secs(15)).await;
        for job in ctx.scheduler.due() {
            let ctx = ctx.clone();
            tokio::spawn(async move { crate::reconcile::job(&ctx, job).await });
        }
    }
}
